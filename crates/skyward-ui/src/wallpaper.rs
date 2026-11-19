use crate::canvas::Canvas;
use crate::color::{blend_px, Rgba};
use crate::geom::Rect;
use crate::theme::Theme;
use anyhow::{ensure, Context, Result};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, RgbaImage};
use std::io::{BufRead, Seek, Write};
use std::sync::Arc;

pub const DEFAULT_OPACITY: u8 = 35;
pub const MIN_OPACITY: u8 = 5;
const STATUS_SCRIM: f32 = 0.15;
const CROP_SCRIM: u32 = 115;
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
        let crop = WallpaperCrop::decode(source, (width, height))?;
        if crop.image.dimensions() == (width, height) {
            return Ok(Self::from_image(Arc::unwrap_or_clone(crop.image)));
        }
        Ok(crop.render())
    }

    fn from_image(cropped: RgbaImage) -> Self {
        let (width, height) = cropped.dimensions();
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
        Self {
            width,
            height,
            pixels: cropped.into_raw().into(),
            blurred: blurred.into_raw().into(),
        }
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

/// An oriented picture and its current framing, kept in memory until applied.
#[derive(Clone, PartialEq)]
pub struct WallpaperCrop {
    image: Arc<RgbaImage>,
    display: (u32, u32),
    center: (f32, f32),
    zoom: f32,
}

impl std::fmt::Debug for WallpaperCrop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WallpaperCrop")
            .field("source_size", &self.image.dimensions())
            .field("center", &self.center)
            .field("zoom", &self.zoom)
            .finish()
    }
}

impl WallpaperCrop {
    pub fn decode(source: impl BufRead + Seek, display: (u32, u32)) -> Result<Self> {
        ensure!(display.0 > 0 && display.1 > 0, "the display has no area");
        let image = decode_picture(source)?;
        let center = (image.width() as f32 / 2.0, image.height() as f32 / 2.0);
        Ok(Self { image: Arc::new(image), display, center, zoom: 0.0 })
    }

    pub(crate) fn zoom_level(&self) -> f32 {
        self.zoom
    }

    pub(crate) fn zoom_percent(&self) -> u32 {
        (self.factor() * 100.0).round() as u32
    }

    pub(crate) fn set_zoom(&mut self, level: f32) {
        self.zoom = level.clamp(0.0, 1.0);
        self.keep_in_picture();
    }

    fn factor(&self) -> f32 {
        1.0 + self.zoom * 2.0
    }

    /// Keep the screen-shaped frame filled, even while dragging or zooming out.
    fn size(&self) -> (u32, u32) {
        let (w, h) = self.image.dimensions();
        let (dw, dh) = (u64::from(self.display.0), u64::from(self.display.1));
        let cover_w = u64::from(w).min(u64::from(h) * dw / dh);
        let cover_h = u64::from(h).min(u64::from(w) * dh / dw);
        let factor = self.factor();
        let shrink = |side: u64| ((side as f32 / factor).round() as u32).max(1);
        (shrink(cover_w), shrink(cover_h))
    }

    fn keep_in_picture(&mut self) {
        let (w, h) = self.size();
        let (half_w, half_h) = (w as f32 / 2.0, h as f32 / 2.0);
        self.center.0 = self.center.0.clamp(half_w, self.image.width() as f32 - half_w);
        self.center.1 = self.center.1.clamp(half_h, self.image.height() as f32 - half_h);
    }

    /// Move the picture along with the finger in the scaled preview.
    pub fn pan(&mut self, dx: f32, dy: f32, preview: Rect) {
        let (w, h) = self.size();
        self.center.0 -= dx * w as f32 / preview.w;
        self.center.1 -= dy * h as f32 / preview.h;
        self.keep_in_picture();
    }

    fn region(&self) -> (u32, u32, u32, u32) {
        let (w, h) = self.size();
        let x = (self.center.0 - w as f32 / 2.0).floor() as u32;
        let y = (self.center.1 - h as f32 / 2.0).floor() as u32;
        (x, y, w, h)
    }

    pub fn render(&self) -> Wallpaper {
        let (x, y, w, h) = self.region();
        let crop = image::imageops::crop_imm(self.image.as_ref(), x, y, w, h);
        let (width, height) = self.display;
        Wallpaper::from_image(image::imageops::resize(
            &*crop, width, height, image::imageops::FilterType::Triangle,
        ))
    }

    pub(crate) fn preview_rect(&self, area: Rect) -> Rect {
        let (w, h) = (self.display.0 as f32, self.display.1 as f32);
        let scale = (area.w / w).min(area.h / h).max(0.0);
        let (w, h) = ((w * scale).floor(), (h * scale).floor());
        let (cx, cy) = area.centre();
        Rect::new((cx - w / 2.0).round(), (cy - h / 2.0).round(), w, h)
    }

    /// Use the same projection on either side of the crop frame so the
    /// dimmed part of the picture stays aligned as the picture moves.
    pub(crate) fn preview(&self, canvas: &mut Canvas, viewport: Rect, frame: Rect, bg: Rgba) {
        canvas.fill_rect(viewport.x, viewport.y, viewport.w, viewport.h, bg);
        let (left, top, width, height) = (frame.x as i64, frame.y as i64, frame.w as i64, frame.h as i64);
        let cols = canvas.cols();
        let rows = canvas.held();
        let first = cols.start.max(viewport.x as u32);
        let end = cols.end.min((viewport.x + viewport.w) as u32);
        if width <= 0 || height <= 0 || first >= end {
            return;
        }

        let (sx, sy, sw, sh) = self.region();
        let source_at = |pixel: i64, start: i64, span: i64, origin: u32, extent: u32| {
            // Round down at pixel centres, including points before the frame's origin.
            i64::from(origin) + ((2 * (pixel - start) + 1) * i64::from(extent)).div_euclid(2 * span)
        };
        let samples: Vec<_> = (first..end)
            .map(|x| {
                let x = i64::from(x);
                (source_at(x, left, width, sx, sw) as usize, (left..left + width).contains(&x))
            })
            .collect();
        let source_rows = self.image.as_raw().as_chunks::<4>().0.chunks_exact(self.image.width() as usize);
        let stride = cols.len() * 4;
        let inset = (first - cols.start) as usize * 4;
        let bytes = (end - first) as usize * 4;
        let black = Rgba::rgb(0, 0, 0);
        for y in rows.start.max(viewport.y as u32)..rows.end.min((viewport.y + viewport.h) as u32) {
            let source_y = source_at(i64::from(y), top, height, sy, sh);
            let Some(source) = source_rows.clone().nth(source_y as usize) else { continue };
            let inside_y = (top..top + height).contains(&i64::from(y));
            let at = (y - rows.start) as usize * stride + inset;
            let output = canvas.data[at..at + bytes].as_chunks_mut::<4>().0;
            for (pixel, &(source_x, inside_x)) in output.iter_mut().zip(&samples) {
                let Some(&photo) = source.get(source_x) else { continue };
                *pixel = if photo[3] == 255 { photo } else { themed_pixel(photo, bg, 255) };
                if !inside_x || !inside_y {
                    blend_px(pixel, black, CROP_SCRIM);
                }
            }
        }
    }
}

fn decode_picture(source: impl BufRead + Seek) -> Result<RgbaImage> {
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
    Ok(decoded.into_rgba8())
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
    fn wallpaper_crop_shows_the_surrounding_photo_only_in_the_preview() {
        let mut picture = Canvas::filled(24, 24, Rgba::rgb(0, 255, 0));
        picture.fill_rect(0.0, 0.0, 9.0, 24.0, Rgba::rgb(255, 0, 0));
        picture.fill_rect(15.0, 0.0, 9.0, 24.0, Rgba::rgb(0, 0, 255));
        picture.fill_rect(9.0, 0.0, 6.0, 6.0, Rgba::rgb(255, 255, 0));
        picture.fill_rect(9.0, 18.0, 6.0, 6.0, Rgba::rgb(255, 0, 255));
        let mut png = Vec::new();
        picture.write_png(&mut png).unwrap();
        let mut crop = WallpaperCrop::decode(Cursor::new(png), (6, 12)).unwrap();
        crop.set_zoom(0.5);
        let frame = Rect::new(6.0, 2.0, 6.0, 12.0);
        let mut preview = Canvas::new(18, 16);
        let viewport = Rect::new(0.0, 0.0, 18.0, 16.0);
        crop.preview(&mut preview, viewport, frame, Rgba::rgb(0, 0, 0));
        let rgba = |x: usize, y: usize| {
            let at = (y * preview.w as usize + x) * 4;
            &preview.data[at..at + 4]
        };
        assert_eq!(rgba(2, 8), &[140, 0, 0, 255], "dimmed left edge of the photo");
        assert_eq!(rgba(15, 8), &[0, 0, 140, 255], "dimmed right edge of the photo");
        assert_eq!(rgba(8, 0), &[140, 140, 0, 255], "dimmed area above the frame");
        assert_eq!(rgba(8, 15), &[140, 0, 140, 255], "dimmed area below the frame");
        assert_eq!(rgba(8, 8), &[0, 255, 0, 255]);
        assert!(crop.render().pixels.as_chunks::<4>().0.iter().all(|px| *px == [0, 255, 0, 255]));
    }

    #[test]
    fn wallpaper_crop_previews_and_saves_either_end_of_a_wide_picture() {
        let mut image = Canvas::new(96, 32);
        let red = Rgba::rgb(210, 24, 38);
        let green = Rgba::rgb(20, 150, 68);
        let blue = Rgba::rgb(25, 60, 214);
        image.fill_rect(0.0, 0.0, 32.0, 32.0, red);
        image.fill_rect(32.0, 0.0, 32.0, 32.0, green);
        image.fill_rect(64.0, 0.0, 32.0, 32.0, blue);
        let mut png = Vec::new();
        image.write_png(&mut png).unwrap();
        let mut crop = WallpaperCrop::decode(Cursor::new(png), (16, 32)).unwrap();
        let area = Rect::new(0.0, 0.0, 16.0, 32.0);
        assert_eq!(crop.region(), (40, 0, 16, 32));
        for (dx, want) in [(100.0, red), (-100.0, blue)] {
            crop.pan(dx, 0.0, area);
            let mut preview = Canvas::new(16, 32);
            crop.preview(&mut preview, area, area, Rgba::rgb(0, 0, 0));
            let saved = crop.render();
            assert_eq!(preview.data.as_slice(), &*saved.pixels);
            assert!(saved.pixels.as_chunks::<4>().0.iter().all(|px| *px == [want.r, want.g, want.b, 255]));
        }
    }

    #[test]
    fn portrait_wallpaper_crop_stays_inside_the_picture_after_panning_and_zooming_out() {
        let mut png = Vec::new();
        Canvas::filled(12, 60, Rgba::rgb(20, 40, 60)).write_png(&mut png).unwrap();
        let mut crop = WallpaperCrop::decode(Cursor::new(png), (12, 24)).unwrap();
        let area = Rect::new(0.0, 0.0, 12.0, 24.0);
        assert_eq!(crop.region(), (0, 18, 12, 24));
        crop.set_zoom(1.0);
        assert_eq!(crop.zoom_percent(), 300);
        crop.pan(-100.0, -100.0, area);
        assert_eq!(crop.region(), (8, 52, 4, 8));
        crop.set_zoom(0.0);
        assert_eq!(crop.region(), (0, 36, 12, 24));
    }

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
            if (x as u32 / 4).is_multiple_of(2) {
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
