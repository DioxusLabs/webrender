use crate::{
    FastHashMap, FontInstance, GlyphFormat, GlyphKey, GlyphRasterError, GlyphRasterResult,
    RasterizedGlyph,
};
use api::{
    ColorU, FontInstanceFlags, FontInstanceKey, FontKey, FontRenderMode, GlyphDimensions,
    NativeFontHandle,
};
use glyphrast::image::{Content, Image};
use glyphrast::read_fonts::types::{GlyphId, Tag};
use glyphrast::zeno::{bounds, Fill, Format, Transform, Vector};
use glyphrast::{Font, Hinting, RasterTarget, Render, Source, StrikeWith};
use memmap2::Mmap;
#[cfg(any(target_os = "macos", target_os = "ios"))]
use skrifa::raw::FileRef;
use std::sync::Arc;

fn hinting(font: &FontInstance) -> Hinting {
    if font.synthetic_italics.is_enabled()
        || ((font.transform.scale_x != 0.0 || font.transform.scale_y != 0.0)
            && (font.transform.skew_x != 0.0 || font.transform.skew_y != 0.0))
    {
        return Hinting::None;
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "windows")))]
    let target = {
        use api::FontHinting;
        match (
            font.platform_options.unwrap_or_default().hinting,
            font.render_mode,
        ) {
            (FontHinting::None, _) => return Hinting::None,
            (FontHinting::Light, _) => RasterTarget::Light,
            (FontHinting::LCD, FontRenderMode::Subpixel) => {
                if font.flags.contains(FontInstanceFlags::LCD_VERTICAL) {
                    RasterTarget::VerticalLcd
                } else {
                    RasterTarget::Lcd
                }
            }
            _ => RasterTarget::Normal,
        }
    };
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "windows"))]
    let target = RasterTarget::Normal;
    if font.flags.contains(FontInstanceFlags::FORCE_AUTOHINT) {
        Hinting::Auto(target)
    } else if font.flags.contains(FontInstanceFlags::NO_AUTOHINT) {
        Hinting::Interpreter(target)
    } else {
        Hinting::AutoFallback(target)
    }
}

struct CachedFont {
    font: Font,
    key: glyphrast::FontKey,
}

struct SimpleFontHandle {
    pub index: u32,
    pub data: Mmap,
}

impl From<NativeFontHandle> for SimpleFontHandle {
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    fn from(native: NativeFontHandle) -> Self {
        let file = std::fs::File::open(&native.path).unwrap();
        let mapped = unsafe { Mmap::map(&file) }.unwrap();

        SimpleFontHandle {
            index: native.index,
            data: mapped,
        }
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    fn from(native: NativeFontHandle) -> Self {
        let file = std::fs::File::open(&native.path).unwrap();
        let mapped = unsafe { Mmap::map(&file) }.unwrap();
        let font_file = FileRef::new(&mapped).unwrap();
        let index = ttc_index_from_postscript_name(font_file, &native.name);

        SimpleFontHandle {
            index,
            data: mapped,
        }
    }
}

pub struct FontContext {
    font_cache: FastHashMap<FontKey, CachedFont>,
    location_cache: FastHashMap<(u64, FontInstanceKey), Font>,
    context: glyphrast::RasterContext,
    image: Image,
    outline: glyphrast::outline::Outline,
    next_key: u64,
}

impl FontContext {
    pub fn distribute_across_threads() -> bool {
        true
    }
    pub fn new() -> Self {
        Self {
            font_cache: Default::default(),
            location_cache: Default::default(),
            context: glyphrast::RasterContext::new(),
            image: Image::new(),
            outline: glyphrast::outline::Outline::new(),
            next_key: 0,
        }
    }
    pub fn begin_rasterize(_: &FontInstance) {}
    pub fn end_rasterize(_: &FontInstance) {}
    pub fn prepare_font(font: &mut FontInstance) {
        font.color = ColorU::new(255, 255, 255, 255);
        if font.render_mode == FontRenderMode::Mono {
            font.disable_subpixel_position();
        }
    }
    pub fn add_raw_font(&mut self, key: &FontKey, bytes: Arc<Vec<u8>>, index: u32) {
        self.add_font(key, bytes, index);
    }
    fn add_font(&mut self, key: &FontKey, bytes: Arc<dyn AsRef<[u8]> + Send + Sync>, index: u32) {
        if let Some(font) = Font::new(bytes, index) {
            self.delete_font(key);
            self.next_key += 1;
            self.font_cache.insert(
                *key,
                CachedFont {
                    font,
                    key: glyphrast::FontKey(self.next_key),
                },
            );
        }
    }
    pub fn add_native_font(&mut self, key: &FontKey, native: NativeFontHandle) {
        let handle = SimpleFontHandle::from(native);
        self.add_font(key, Arc::new(handle.data), handle.index);
    }
    pub fn delete_font(&mut self, key: &FontKey) {
        if let Some(font) = self.font_cache.remove(key) {
            self.location_cache.retain(|(id, _), _| *id != font.key.0);
        }
    }
    pub fn delete_font_instance(&mut self, instance: &FontInstance) {
        self.location_cache
            .retain(|(_, key), _| *key != instance.base.instance_key);
    }
    pub fn get_glyph_index(&mut self, key: FontKey, ch: char) -> Option<u32> {
        self.font_cache
            .get(&key)?
            .font
            .charmap()
            .map_unicode(ch)
            .map(|id| id.to_u32())
    }
    fn render(
        &mut self,
        instance: &FontInstance,
        key: &GlyphKey,
        dimensions_only: bool,
    ) -> Option<f32> {
        self.image.clear();
        let cached = self.font_cache.get(&instance.font_key)?;
        let font = self
            .location_cache
            .entry((cached.key.0, instance.base.instance_key))
            .or_insert_with(|| {
                cached
                    .font
                    .instance_builder()
                    .variations(
                        instance
                            .base
                            .variations
                            .iter()
                            .map(|v| (Tag::from_be_bytes(v.tag.to_be_bytes()), v.value)),
                    )
                    .build()
            });
        let req_size = instance.size.to_f64_px();
        let (x_scale, y_scale) = instance.transform.compute_scale().unwrap_or((1.0, 1.0));
        let size = (req_size * y_scale) as f32;
        let mut shape = instance.transform.invert_scale(x_scale, y_scale);
        if instance.flags.contains(FontInstanceFlags::FLIP_X) {
            shape = shape.flip_x();
        }
        if instance.flags.contains(FontInstanceFlags::FLIP_Y) {
            shape = shape.flip_y();
        }
        if instance.flags.contains(FontInstanceFlags::TRANSPOSE) {
            shape = shape.swap_xy();
        }
        let (mut tx, mut ty) = (0.0, 0.0);
        if instance.synthetic_italics.is_enabled() {
            let (s, (x, y)) = instance.synthesize_italics(shape, req_size * y_scale);
            shape = s;
            tx = x;
            ty = y;
        }
        let aspect = (x_scale / y_scale) as f32;
        let transform = Transform::new(
            shape.scale_x * aspect,
            -shape.skew_y * aspect,
            -shape.skew_x,
            shape.scale_y,
            tx as f32,
            -ty as f32,
        );
        let bold = if instance.flags.contains(FontInstanceFlags::SYNTHETIC_BOLD) {
            size / 48.0
        } else {
            0.0
        };
        let advance = if dimensions_only {
            font.glyph_metrics()
                .scaled_f32(size)
                .h_advance(GlyphId::new(key.index()))
                * shape.scale_x
                * aspect
                + bold
        } else {
            0.0
        };
        let mut scaler = self
            .context
            .builder(font, cached.key)
            .size(size)
            .hinting(hinting(instance))
            .build();
        let mut sources = [
            Source::ColorBitmap(StrikeWith::BestFit),
            Source::ColorOutline(0),
            Source::Outline,
            Source::Outline,
        ];
        if instance.flags.contains(FontInstanceFlags::EMBEDDED_BITMAPS) {
            sources[2] = Source::Bitmap(StrikeWith::BestFit);
        }
        let (dx, dy) = instance.get_subpx_offset(key);
        let offset = Vector::new(dx as f32, -dy as f32);
        if dimensions_only
            && !scaler.has_color_outlines()
            && !scaler.has_color_bitmaps()
            && !(instance.flags.contains(FontInstanceFlags::EMBEDDED_BITMAPS)
                && scaler.has_bitmaps())
        {
            scaler.scale_outline_into(key.index(), &mut self.outline)?;
            if bold != 0.0 {
                self.outline.embolden(bold, bold);
            }
            self.outline.transform(&transform);
            let mut bounds = bounds(self.outline.path(), Fill::NonZero, None);
            bounds.min = (bounds.min + offset).floor();
            bounds.max = (bounds.max + offset).ceil();
            self.image.placement = glyphrast::zeno::Placement {
                left: bounds.min.x as i32,
                top: bounds.max.y as i32,
                width: bounds.width() as u32,
                height: bounds.height() as u32,
            };
            return Some(advance);
        }
        let format = if instance.render_mode == FontRenderMode::Subpixel {
            if instance.flags.contains(FontInstanceFlags::SUBPIXEL_BGR) {
                Format::subpixel_bgra()
            } else {
                Format::Subpixel
            }
        } else {
            Format::Alpha
        };
        let rendered = Render::new(&sources)
            .format(format)
            .transform(Some(transform))
            .offset(offset)
            .embolden(bold)
            .default_color([255, 255, 255, 255])
            .render_into(&mut scaler, key.index(), &mut self.image);
        if rendered {
            Some(advance)
        } else {
            None
        }
    }
    pub fn get_glyph_dimensions(
        &mut self,
        instance: &FontInstance,
        key: &GlyphKey,
    ) -> Option<GlyphDimensions> {
        let advance = self.render(instance, key, true)?;
        let image = &self.image;
        Some(GlyphDimensions {
            left: image.placement.left,
            top: image.placement.top,
            width: image.placement.width as i32,
            height: image.placement.height as i32,
            advance,
        })
    }
    pub fn rasterize_glyph(
        &mut self,
        instance: &FontInstance,
        key: &GlyphKey,
    ) -> GlyphRasterResult {
        self.render(instance, key, false)
            .ok_or(GlyphRasterError::LoadFailed)?;
        let image = &self.image;
        let p = image.placement;
        if p.width == 0 || p.height == 0 {
            return Err(GlyphRasterError::LoadFailed);
        }
        let padding = if instance.use_texture_padding() { 1 } else { 0 };
        let width = p.width as usize + 2 * padding;
        let height = p.height as usize + 2 * padding;
        let mut bytes = vec![0; width * height * 4];
        for y in 0..p.height as usize {
            let start = ((y + padding) * width + padding) * 4;
            let row = &mut bytes[start..start + p.width as usize * 4];
            let src_start = y * p.width as usize;
            match image.content {
                Content::Mask => {
                    let src = &image.data[src_start..src_start + p.width as usize];
                    for (dst, &alpha) in row.chunks_exact_mut(4).zip(src) {
                        dst.copy_from_slice(&[alpha; 4]);
                    }
                }
                Content::SubpixelMask => {
                    row.copy_from_slice(
                        &image.data[src_start * 4..(src_start + p.width as usize) * 4],
                    );
                    for dst in row.chunks_exact_mut(4) {
                        dst[3] = dst[0].max(dst[1]).max(dst[2]);
                    }
                }
                Content::Color => {
                    let src = &image.data[src_start * 4..(src_start + p.width as usize) * 4];
                    for (dst, src) in row.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                        let a = src[3] as u32;
                        dst.copy_from_slice(&[
                            ((src[2] as u32 * a + 128) / 255) as u8,
                            ((src[1] as u32 * a + 128) / 255) as u8,
                            ((src[0] as u32 * a + 128) / 255) as u8,
                            src[3],
                        ]);
                    }
                }
            }
        }
        let bitmap = matches!(image.source, Source::Bitmap(_) | Source::ColorBitmap(_));
        let format = match image.content {
            Content::Color => GlyphFormat::ColorBitmap,
            Content::SubpixelMask => GlyphFormat::Subpixel,
            Content::Mask if bitmap => GlyphFormat::Bitmap,
            Content::Mask => GlyphFormat::Alpha,
        };
        Ok(RasterizedGlyph {
            left: (p.left - padding as i32) as f32,
            top: (p.top + padding as i32) as f32,
            width: width as i32,
            height: height as i32,
            scale: 1.0,
            format,
            bytes,
            is_packed_glyph: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BaseFontInstance, FontTransform, SubpixelDirection};
    use api::{units::DevicePoint, FontInstanceOptions, IdNamespace};

    fn setup() -> (FontContext, FontInstance) {
        let ns = IdNamespace(0);
        let key = FontKey::new(ns, 0);
        let mut context = FontContext::new();
        context.add_raw_font(
            &key,
            Arc::new(include_bytes!("../../../../wrench/reftests/text/FreeSans.ttf").to_vec()),
            0,
        );
        let base = BaseFontInstance::new(
            FontInstanceKey::new(ns, 0),
            key,
            24.0,
            Some(FontInstanceOptions {
                render_mode: FontRenderMode::Alpha,
                ..Default::default()
            }),
            None,
            Vec::new(),
        );
        let mut instance = FontInstance::from_base(Arc::new(base));
        FontContext::prepare_font(&mut instance);
        (context, instance)
    }

    fn glyph(index: u32) -> GlyphKey {
        GlyphKey::new(index, DevicePoint::zero(), crate::SubpixelDirection::None)
    }

    #[test]
    fn renders_and_reuses_font_state() {
        let (mut context, instance) = setup();
        let index = context.get_glyph_index(instance.font_key, 'A').unwrap();
        let key = glyph(index);
        let dimensions = context.get_glyph_dimensions(&instance, &key).unwrap();
        let first = context.rasterize_glyph(&instance, &key).unwrap();
        let second = context.rasterize_glyph(&instance, &key).unwrap();
        assert!(dimensions.advance > 0.0);
        assert!(first.width > 0 && first.height > 0);
        assert!(first.bytes.iter().any(|b| *b != 0));
        assert_eq!(first.bytes, second.bytes);
        assert_eq!(context.location_cache.len(), 1);
        context.delete_font_instance(&instance);
        assert!(context.location_cache.is_empty());
        context.rasterize_glyph(&instance, &key).unwrap();
        context.delete_font(&instance.font_key);
        assert!(context.font_cache.is_empty());
        assert!(context.location_cache.is_empty());
        assert!(context.rasterize_glyph(&instance, &key).is_err());
    }

    #[test]
    fn embedded_bitmap_best_fit() {
        let (mut context, mut instance) = setup();
        context.add_raw_font(
            &instance.font_key,
            Arc::new(include_bytes!("../../../../wrench/reftests/text/Proggy.ttf").to_vec()),
            0,
        );
        instance.flags.insert(FontInstanceFlags::EMBEDDED_BITMAPS);
        let index = context.get_glyph_index(instance.font_key, 'A').unwrap();
        let dimensions = context
            .get_glyph_dimensions(&instance, &glyph(index))
            .unwrap();
        let result = context.rasterize_glyph(&instance, &glyph(index)).unwrap();
        assert_eq!(
            (dimensions.width, dimensions.height),
            (result.width, result.height)
        );
        assert!(matches!(result.format, GlyphFormat::Bitmap));
        assert!(result.width > 0 && result.height > 0);
        assert!(result.bytes.iter().any(|b| *b != 0));
    }

    #[test]
    fn outline_dimensions_match_rendering() {
        let (mut context, mut instance) = setup();
        for (transform, italics) in [
            FontTransform::identity(),
            FontTransform::new(0.866, -0.5, 0.5, 0.866),
            FontTransform::new(1.0, 0.25, 0.0, 1.0),
            FontTransform::new(-1.5, 0.0, 0.0, 0.75),
        ]
        .iter()
        .copied()
        .flat_map(|transform| {
            [false, true]
                .iter()
                .copied()
                .map(move |italics| (transform, italics))
        }) {
            instance.transform = transform;
            Arc::make_mut(&mut instance.base).options.synthetic_italics = if italics {
                api::SyntheticItalics::enabled()
            } else {
                api::SyntheticItalics::disabled()
            };
            for flags in [
                FontInstanceFlags::empty(),
                FontInstanceFlags::SYNTHETIC_BOLD,
            ] {
                instance.flags = flags | FontInstanceFlags::SUBPIXEL_POSITION;
                for mode in [FontRenderMode::Alpha, FontRenderMode::Subpixel] {
                    instance.render_mode = mode;
                    for direction in [SubpixelDirection::Horizontal, SubpixelDirection::Vertical] {
                        for offset in [0.0, 0.25, 0.5, 0.75] {
                            for ch in ' '..='ÿ' {
                                let Some(index) = context.get_glyph_index(instance.font_key, ch)
                                else {
                                    continue;
                                };
                                let key = GlyphKey::new(
                                    index,
                                    DevicePoint::new(offset, offset),
                                    direction,
                                );
                                let d = context.get_glyph_dimensions(&instance, &key).unwrap();
                                assert!(context.image.data.is_empty());
                                context.render(&instance, &key, false).unwrap();
                                let p = context.image.placement;
                                assert_eq!((d.left, d.top, d.width, d.height),
                                           (p.left, p.top, p.width as i32, p.height as i32),
                                           "{ch:?} {offset} {direction:?} {mode:?} {flags:?} {transform:?}");
                            }
                        }
                    }
                }
            }
        }
        assert!(context
            .get_glyph_dimensions(&instance, &glyph(0xffff))
            .is_none());
    }

    #[test]
    fn subpixel_coverage_and_padding() {
        let (mut context, mut instance) = setup();
        instance.render_mode = FontRenderMode::Subpixel;
        let index = context.get_glyph_index(instance.font_key, 'A').unwrap();
        let key = glyph(index);
        let unpadded = context.rasterize_glyph(&instance, &key).unwrap();
        assert!(matches!(unpadded.format, GlyphFormat::Subpixel));
        assert!(unpadded.bytes.chunks_exact(4).any(|p| p[0] != p[2]));
        let right_edge = unpadded
            .bytes
            .chunks_exact(unpadded.width as usize * 4)
            .map(|row| &row[row.len() - 4..]);
        let (b, r) = right_edge.fold((0u32, 0u32), |(b, r), p| (b + p[0] as u32, r + p[2] as u32));
        assert!(r > b);
        assert!(unpadded
            .bytes
            .chunks_exact(4)
            .all(|p| p[3] == p[0].max(p[1]).max(p[2])));
        instance.flags.insert(FontInstanceFlags::SUBPIXEL_BGR);
        let bgr = context.rasterize_glyph(&instance, &key).unwrap();
        for (rgb, bgr) in unpadded
            .bytes
            .chunks_exact(4)
            .zip(bgr.bytes.chunks_exact(4))
        {
            assert_eq!(rgb, &[bgr[2], bgr[1], bgr[0], bgr[3]]);
        }
        instance.flags.remove(FontInstanceFlags::SUBPIXEL_BGR);
        instance.flags.insert(FontInstanceFlags::TEXTURE_PADDING);
        let padded = context.rasterize_glyph(&instance, &key).unwrap();
        assert_eq!(padded.width, unpadded.width + 2);
        assert_eq!(padded.height, unpadded.height + 2);
        assert_eq!(padded.left, unpadded.left - 1.0);
        assert_eq!(padded.top, unpadded.top + 1.0);
        assert!(padded.bytes[..padded.width as usize * 4]
            .iter()
            .all(|b| *b == 0));
    }

    #[test]
    fn empty_and_invalid_glyphs() {
        let (mut context, instance) = setup();
        let index = context.get_glyph_index(instance.font_key, ' ').unwrap();
        let dimensions = context
            .get_glyph_dimensions(&instance, &glyph(index))
            .unwrap();
        assert!(dimensions.advance > 0.0);
        assert!(context.rasterize_glyph(&instance, &glyph(index)).is_err());
        assert!(context.rasterize_glyph(&instance, &glyph(0xffff)).is_err());
        context.delete_font(&instance.font_key);
        context.add_raw_font(&instance.font_key, Arc::new(vec![0; 4]), 0);
        assert!(context.get_glyph_index(instance.font_key, 'A').is_none());
    }
}

/// CoreText font enumaration gives us a postscript name rather than an index.
/// This functions maps from postscript name to index
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn ttc_index_from_postscript_name(font_file: FileRef<'_>, postscript_name: &str) -> u32 {
    use skrifa::raw::types::NameId;
    use skrifa::raw::{FileRef, TableProvider as _};

    let index = match font_file {
        FileRef::Font(_) => 0,
        FileRef::Collection(collection) => 'idx: {
            for i in 0..collection.len() {
                let font = collection.get(i).unwrap();
                let name_table = font.name().unwrap();
                if name_table
                    .name_record()
                    .iter()
                    .filter(|record| record.name_id() == NameId::POSTSCRIPT_NAME)
                    .any(|record| {
                        record
                            .string(name_table.string_data())
                            .unwrap()
                            .chars()
                            .eq(postscript_name.chars())
                    })
                {
                    break 'idx i;
                }
            }

            panic!(
                "Font with postscript_name {} not found in collection",
                postscript_name
            );
        }
    };

    index
}
