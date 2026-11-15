use crate::canvas::Canvas;
use crate::color::{blend_px, Rgba};
use crate::geom::Rect;
use crate::theme::Theme;
use anyhow::{ensure, Context, Result};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use std::io::{BufRead, Seek, Write};
use std::sync::Arc;

pub const DEFAULT_OPACITY: u8 = 35;
pub const MIN_OPACITY: u8 = 5;
const STATUS_SCRIM: f32 = 0.15;
const MAX_IMAGE_SIDE: u32 = 12_000;
const MAX_SOURCE_PIXELS: u64 = 66_000_000;
const MAX_DECODED_BYTES: u64 = MAX_SOURCE_PIXELS * 4;
const BLUR_DOWNSAMPLE: u32 = 8;

pub(crate) fn opacity_level(percent: u8) -> f32 {
    f32::from(percent.clamp(MIN_OPACITY, 100) - MIN_OPACITY) / f32::from(100 - MIN_OPACITY)
}

pub(crate) fn opacity_percent(level: f32) -> u8 {
    MIN_OPACITY + (level.clamp(0.0, 1.0) * f32::from(100 - MIN_OPACITY)).round() as u8
}

/// The selected photo cropped to the display's aspect ratio, in straight RGBA8.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wallpaper {
    width: u32,
    height: u32,
    pixels: Arc<[u8]>,
    blurred: Arc<[u8]>,
}

impl Wallpaper {
    pub fn decode(source: impl BufRead + Seek, width: u32, height: u32) -> Result<Self> {
        ensure!(width > 0 && height > 0, "the display has no area");
        let mut reader = ImageReader::new(source)
            .with_guessed_format()
            .context("read the picture format")?;
        ensure!(
            matches!(reader.format(), Some(ImageFormat::Jpeg | ImageFormat::Png)),
            "choose a JPG or PNG picture"
        );
        let mut limits = Limits::default();
        limits.max_image_width = Some(MAX_IMAGE_SIDE);
        limits.max_image_height = Some(MAX_IMAGE_SIDE);
        reader.limits(limits);

        let mut decoder = reader.into_decoder().context("open the picture")?;
        let (source_w, source_h) = decoder.dimensions();
        ensure!(
            u64::from(source_w) * u64::from(source_h) <= MAX_SOURCE_PIXELS,
            "the picture has too many pixels"
        );
        ensure!(
            decoder.total_bytes() <= MAX_DECODED_BYTES,
            "the decoded picture is too large"
        );
        let orientation = decoder
            .orientation()
            .context("read the picture orientation")?;
        let mut decoded = DynamicImage::from_decoder(decoder).context("decode the picture")?;
        decoded.apply_orientation(orientation);
        let rgba = decoded.into_rgba8();
        let (w, h) = (u64::from(rgba.width()), u64::from(rgba.height()));
        let (target_w, target_h) = (u64::from(width), u64::from(height));
        // Crop before resizing so very wide or tall pictures only allocate
        // space for the part that fits the display.
        let crop_w = w.min((h * target_w / target_h).max(1));
        let crop_h = h.min((w * target_h / target_w).max(1));
        let cropped = if (w, h) == (target_w, target_h) {
            rgba
        } else {
            let crop = image::imageops::crop_imm(
                &rgba,
                ((w - crop_w) / 2) as u32,
                ((h - crop_h) / 2) as u32,
                crop_w as u32,
                crop_h as u32,
            );
            image::imageops::resize(&*crop, width, height, image::imageops::FilterType::Triangle)
        };
        // The blur runs on a reduced image. Expand it once to screen
        // coordinates so scrolling cards can sample it directly.
        let reduced = image::imageops::thumbnail(
            &cropped,
            width.div_ceil(BLUR_DOWNSAMPLE),
            height.div_ceil(BLUR_DOWNSAMPLE),
        );
        let blurred = image::imageops::blur(&reduced, 4.0);
        let blurred = image::imageops::resize(
            &blurred,
            width,
            height,
            image::imageops::FilterType::Triangle,
        );
        Ok(Self {
            width,
            height,
            pixels: cropped.into_raw().into(),
            blurred: blurred.into_raw().into(),
        })
    }

    /// Write the display-sized pixels as a new image, without the source's metadata.
    pub fn write_png(&self, to: impl Write) -> Result<()> {
        let mut encoder = png::Encoder::new(to, self.width, self.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&self.pixels)?;
        writer.finish()?;
        Ok(())
    }

    pub(crate) fn show_behind(
        &self,
        canvas: &mut Canvas,
        theme: &Theme,
        opacity: u8,
        cards: &[Rect],
        scroll: i32,
    ) {
        debug_assert_eq!((canvas.w, canvas.h), (self.width, self.height));
        let stride = self.width as usize * 4;
        let rows = canvas
            .data
            .chunks_exact_mut(stride)
            .zip(self.pixels.chunks_exact(stride));
        let glass_strength = (u32::from(opacity) * 255 + 50) / 100;
        let bg = theme.pal.bg;
        let mut spans = Vec::new();
        for (y, (out, photo)) in rows.enumerate() {
            let out = out.as_chunks_mut::<4>().0;
            let photo = photo.as_chunks::<4>().0;
            let strength = theme_strength(theme, y as f32, opacity);
            let page_y = y as f32 + scroll as f32 + 0.5;
            spans.clear();
            for card in cards {
                if page_y >= card.y - 1.0 && page_y <= card.y + card.h + 1.0 {
                    let left = ((card.x - 1.0).floor() as usize).min(out.len());
                    let right = ((card.x + card.w + 1.0).ceil() as usize).clamp(left, out.len());
                    spans.push((left, right));
                }
            }
            spans.sort_unstable();
            let blurred = self.blurred[y * stride..(y + 1) * stride].as_chunks::<4>().0;
            let mut end = 0;
            for (left, right) in spans.iter().copied() {
                let left = left.max(end);
                if left >= right {
                    continue;
                }
                composite_wallpaper(&mut out[end..left], &photo[end..left], bg, strength);
                for ((pixel, sharp), blurred) in out[left..right]
                    .iter_mut()
                    .zip(&photo[left..right])
                    .zip(&blurred[left..right])
                {
                    let [r, g, b, alpha] = *pixel;
                    if alpha == 255 {
                        continue;
                    }
                    let glass = themed_pixel(*blurred, bg, glass_strength);
                    // Fade the blur along the rounded card's antialiased edge.
                    let tint = u32::from(theme.pal.card.a);
                    if u32::from(alpha) < tint {
                        *pixel = themed_pixel(*sharp, bg, strength);
                        let [gr, gg, gb, _] = glass;
                        blend_px(pixel, Rgba::rgb(gr, gg, gb), u32::from(alpha) * 255 / tint);
                    } else {
                        *pixel = glass;
                    }
                    blend_px(pixel, Rgba::rgb(r, g, b), u32::from(alpha));
                }
                end = right;
            }
            composite_wallpaper(&mut out[end..], &photo[end..], bg, strength);
        }
    }

    /// Draw the full portrait crop inside a sample of the current theme.
    pub(crate) fn preview(&self, canvas: &mut Canvas, theme: &Theme, area: Rect, opacity: u8) {
        canvas.fill_rect(area.x, area.y, area.w, area.h, theme.pal.bg);
        let available_w = area.w.floor() as u32;
        let available_h = area.h.floor() as u32;
        let width = ((u64::from(available_h) * u64::from(self.width) / u64::from(self.height))
            as u32)
            .min(available_w);
        let height = ((u64::from(available_w) * u64::from(self.height) / u64::from(self.width))
            as u32)
            .min(available_h);
        if width == 0 || height == 0 {
            return;
        }

        let x0 = (area.x + (area.w - width as f32) / 2.0).round() as u32;
        let y0 = (area.y + (area.h - height as f32) / 2.0).round() as u32;
        for y in canvas.held().start.max(y0)..canvas.held().end.min(y0 + height) {
            let src_y = (y - y0) * self.height / height;
            let strength = theme_strength(theme, src_y as f32, opacity);
            let row = (src_y * self.width * 4) as usize;
            for x in canvas.cols().start.max(x0)..canvas.cols().end.min(x0 + width) {
                let src_x = (x - x0) * self.width / width;
                let at = row + src_x as usize * 4;
                let pixel = self.pixels[at..at + 4]
                    .try_into()
                    .expect("four values in a photo pixel");
                let [r, g, b, _] = themed_pixel(pixel, theme.pal.bg, strength);
                canvas.blend(x as i32, y as i32, Rgba::rgb(r, g, b), 1.0);
            }
        }
    }
}

fn composite_wallpaper(out: &mut [[u8; 4]], photo: &[[u8; 4]], bg: Rgba, strength: u32) {
    for (pixel, sharp) in out.iter_mut().zip(photo) {
        let [r, g, b, alpha] = *pixel;
        if alpha < 255 {
            *pixel = themed_pixel(*sharp, bg, strength);
            blend_px(pixel, Rgba::rgb(r, g, b), u32::from(alpha));
        }
    }
}

/// The wallpaper shows through most strongly behind the clock and status
/// icons. Below that strip, return to the theme's tint before the back arrow.
fn theme_strength(theme: &Theme, y: f32, opacity: u8) -> u32 {
    let fade = ((y - theme.m.dp(40.0)) / theme.m.dp(22.0)).clamp(0.0, 1.0);
    let body = if theme.dark { 0.37 } else { 0.48 };
    let top = 1.0 - STATUS_SCRIM;
    let maximum = top + (body - top) * fade;
    let minimum = f32::from(MIN_OPACITY) / 100.0;
    ((minimum + (maximum - minimum) * opacity_level(opacity)) * 255.0).round() as u32
}

#[inline]
fn themed_pixel([r, g, b, a]: [u8; 4], bg: Rgba, strength: u32) -> [u8; 4] {
    let mut result = [bg.r, bg.g, bg.b, 255];
    blend_px(
        &mut result,
        Rgba::rgb(r, g, b),
        (u32::from(a) * strength + 127) / 255,
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::jpeg::JpegEncoder;
    use image::{ExtendedColorType, ImageBuffer, Rgb};
    use std::io::Cursor;

    #[test]
    fn wallpaper_decodes_jpeg_and_uses_its_camera_orientation() {
        let picture = ImageBuffer::from_fn(32, 16, |x, _| {
            if x < 16 {
                Rgb([240u8, 0, 0])
            } else {
                Rgb([0u8, 0, 240])
            }
        });
        let mut bytes = Vec::new();
        JpegEncoder::new_with_quality(&mut bytes, 95)
            .encode(picture.as_raw(), 32, 16, ExtendedColorType::Rgb8)
            .unwrap();
        // EXIF orientation 6 turns the source clockwise, red above blue.
        let exif = b"\xff\xe1\x00\x22Exif\x00\x00II\x2a\x00\x08\x00\x00\x00\x01\x00\x12\x01\x03\x00\x01\x00\x00\x00\x06\x00\x00\x00\x00\x00\x00\x00";
        bytes.splice(2..2, exif.iter().copied());
        let image = Wallpaper::decode(Cursor::new(bytes), 8, 16).unwrap();
        assert!(image.pixels[0] > 220, "red at the top");
        assert!(
            image.pixels[image.pixels.len() - 2] > 220,
            "blue at the bottom"
        );
    }

    #[test]
    fn all_side_by_side_cards_are_blurred() {
        let sharp = [220u8, 20, 20, 255].repeat(8);
        let blurred = [20u8, 20, 220, 255].repeat(8);
        let image = Wallpaper { width: 8, height: 1, pixels: sharp.into(), blurred: blurred.into() };
        let theme = Theme::light(8, 1, 1.0).frosted();
        let mut canvas = Canvas::new(8, 1);
        canvas.fill_rect(0.0, 0.0, 2.0, 1.0, theme.pal.card);
        canvas.fill_rect(5.0, 0.0, 2.0, 1.0, theme.pal.card);
        let cards = [Rect::new(5.0, 0.0, 2.0, 1.0), Rect::new(0.0, 0.0, 2.0, 1.0)];
        image.show_behind(&mut canvas, &theme, 100, &cards, 0);
        assert_eq!(&canvas.data[0..4], &canvas.data[5 * 4..6 * 4]);
    }

    #[test]
    fn frosted_wallpaper_blurs_the_scrolled_card_area_and_preserves_its_foreground() {
        let (w, h) = (96, 160);
        let mut stripes = Canvas::new(w, h);
        stripes.shade_rows(0, h, |x, _| {
            if (x as u32 / 4) % 2 == 0 {
                Rgba::rgb(0, 0, 0)
            } else {
                Rgba::rgb(255, 255, 255)
            }
        });
        let mut png = Vec::new();
        stripes.write_png(&mut png).unwrap();
        let image = Wallpaper::decode(Cursor::new(png), w, h).unwrap();
        let stock = Theme::light(w, h, 0.5);
        let glass = stock.frosted();
        let mut original = Canvas::new(w, h);
        image.show_behind(&mut original, &stock, 100, &[], 0);
        let px = |c: &Canvas, x: usize, y: usize| -> [u8; 4] {
            let at = (y * w as usize + x) * 4;
            c.data[at..at + 4].try_into().unwrap()
        };
        let scroll = 27;
        let card = Rect::new(8.0, 127.0, 80.0, 34.0);
        let mut frosted = Canvas::new(w, h);
        frosted.fill_round_rect(
            card.x,
            card.y - scroll as f32,
            card.w,
            card.h,
            6.0,
            glass.pal.card,
        );
        frosted.fill_rect(60.0, 108.0, 4.0, 4.0, Rgba::rgb(0, 0, 0));
        image.show_behind(&mut frosted, &glass, 100, &[card], scroll);
        assert_eq!(
            px(&frosted, 4, 110),
            px(&original, 4, 110),
            "the margin stays sharp"
        );
        assert_eq!(
            px(&frosted, 30, 98),
            px(&original, 30, 98),
            "the region begins at the scrolled position"
        );
        assert_eq!(
            px(&frosted, 26, 110),
            px(&frosted, 31, 110),
            "the fine stripes blur under the card"
        );
        assert_ne!(px(&original, 26, 110), px(&original, 31, 110));
        assert_eq!(
            px(&frosted, 61, 110),
            [0, 0, 0, 255],
            "foreground ink remains sharp"
        );
    }

    #[test]
    fn wallpaper_keeps_a_lighter_status_scrim_and_a_visible_minimum() {
        let light = Theme::light(1080, 2400, 2.75);
        let dark = Theme::dark(1080, 2400, 2.75);
        assert_eq!(theme_strength(&light, light.m.status_cy, 100), 217);
        assert_eq!(theme_strength(&dark, dark.m.status_cy, 100), 217);
        assert!(theme_strength(&light, 500.0, 100) <= 123);
        assert!(theme_strength(&dark, 500.0, 100) <= 95);
        assert_eq!(theme_strength(&light, light.m.status_cy, 5), 13);
        assert_eq!(theme_strength(&dark, 500.0, 5), 13);
    }
}
