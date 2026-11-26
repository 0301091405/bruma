//! Turning a selection plus its annotations into a finished image.

use tiny_skia::{IntSize, Pixmap};

use crate::blur;
use crate::doc::{Annotation, Shape};
use crate::draw::draw_annotation;
use crate::geom::{Point, Rect};
use crate::image::Image;
use crate::text::Fonts;
use crate::theme;

/// Crops `screen` to `selection` and draws the annotations on top, clipped to
/// the selection. The selection is rounded and clipped to the screen.
pub fn flatten(screen: &Image, items: &[Annotation], selection: Rect, fonts: &Fonts) -> Image {
    flatten_scaled(screen, items, selection, fonts, 1.0)
}

/// [`flatten`] for a display with the given UI scale, which sets how strong
/// the mist around focus areas is.
pub fn flatten_scaled(
    screen: &Image,
    items: &[Annotation],
    selection: Rect,
    fonts: &Fonts,
    scale: f32,
) -> Image {
    let area = selection.round().intersect(&screen.bounds());
    let crop = screen.crop(area);
    if items.is_empty() || crop.width == 0 || crop.height == 0 {
        return crop;
    }
    let (width, height) = (crop.width, crop.height);
    let offset = Point::new(-area.x, -area.y);

    // Focus areas: everything else in the capture goes out of focus.
    let focus: Vec<Rect> = items
        .iter()
        .filter_map(|item| match item.shape {
            Shape::Focus { rect } => Some(rect.translate(offset.x, offset.y)),
            _ => None,
        })
        .collect();
    let mut base = crop.rgba;
    if !focus.is_empty() {
        let (w, h) = (width as usize, height as usize);
        let misted = blur::mist(&base, w, h, scale, theme::SOFT_KEEP);
        let sharp = std::mem::replace(&mut base, misted);
        let bounds = Rect::new(0.0, 0.0, width as f32, height as f32);
        for rect in focus {
            let r = rect.round().intersect(&bounds);
            for y in r.y as usize..r.bottom() as usize {
                let (from, to) = ((y * w + r.x as usize) * 4, (y * w + r.right() as usize) * 4);
                base[from..to].copy_from_slice(&sharp[from..to]);
            }
        }
    }

    let Some(mut pixmap) = IntSize::from_wh(width, height).and_then(|s| Pixmap::from_vec(base, s))
    else {
        return screen.crop(area);
    };
    for item in items {
        draw_annotation(&mut pixmap, item, offset, screen, fonts);
    }
    Image::new(width, height, pixmap.take())
}

/// Reads a PNG into an image. Greyscale and palette images are expanded.
pub fn decode_png(bytes: &[u8]) -> Result<Image, png::DecodingError> {
    decode_png_meta(bytes).map(|(image, _)| image)
}

/// What a capture needs to be edited again: the picture before anything was
/// drawn on it, and the drawings.
pub struct Editable {
    pub original: Image,
    pub items: Vec<Annotation>,
}

/// Packs an editable capture into the text that goes into the PNG.
pub fn pack_editable(original: &Image, items: &[Annotation]) -> Result<String, png::EncodingError> {
    let picture = base64_encode(&encode_png(original)?);
    Ok(format!(
        "bruma 1\n{picture}\n{}",
        crate::doc::serialize(items)
    ))
}

/// Reads back what [`pack_editable`] wrote. `None` if the text is something else.
pub fn unpack_editable(meta: &str) -> Option<Editable> {
    let mut parts = meta.splitn(3, '\n');
    if parts.next()? != "bruma 1" {
        return None;
    }
    let original = decode_png(&base64_decode(parts.next()?)?).ok()?;
    let items = crate::doc::parse(parts.next().unwrap_or(""));
    Some(Editable { original, items })
}

/// Reads a PNG and, if it has one, the text field Bruma put in it.
pub fn decode_png_meta(bytes: &[u8]) -> Result<(Image, Option<String>), png::DecodingError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let size = reader
        .output_buffer_size()
        .ok_or(png::DecodingError::LimitsExceeded)?;
    let mut buf = vec![0; size];
    let info = reader.next_frame(&mut buf)?;
    let data = &buf[..info.buffer_size()];
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => data.to_vec(),
        png::ColorType::Rgb => data
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => data
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        _ => data.iter().flat_map(|&v| [v, v, v, 255]).collect(),
    };
    let meta = reader
        .info()
        .utf8_text
        .iter()
        .find(|chunk| chunk.keyword == META_KEY)
        .and_then(|chunk| chunk.get_text().ok());
    Ok((Image::new(info.width, info.height, rgba), meta))
}

/// Sets a capture in a frame for sharing: rounded corners, a soft shadow, and
/// around it the capture itself far out of focus.
pub fn frame(image: &Image, scale: f32) -> Image {
    use tiny_skia::{
        FillRule, FilterQuality, LineCap, LineJoin, Paint, Pattern, SpreadMode, Transform,
    };
    let (w, h) = (image.width, image.height);
    if w == 0 || h == 0 {
        return image.clone();
    }
    let pad = (w.max(h) as f32 * 0.07)
        .clamp(28.0 * scale, 110.0 * scale)
        .round() as u32;
    let (cw, ch) = (w + 2 * pad, h + 2 * pad);
    let mut small = blur::downscale(&image.rgba, w as usize, h as usize, 8);
    blur::blur(&mut small, 6, 3);
    let backdrop = blur::upscale(&small, cw as usize, ch as usize, 0.62);
    let (Some(mut canvas), Some(picture)) = (
        IntSize::from_wh(cw, ch).and_then(|s| Pixmap::from_vec(backdrop, s)),
        IntSize::from_wh(w, h).and_then(|s| Pixmap::from_vec(image.rgba.clone(), s)),
    ) else {
        return image.clone();
    };
    let inner = Rect::new(pad as f32, pad as f32, w as f32, h as f32);
    let radius = 10.0 * scale;
    // A shadow built from a few faint layers, each a little wider.
    for layer in (1..=6).rev() {
        let grown = inner
            .inflate(layer as f32 * 2.0 * scale)
            .translate(0.0, 3.0 * scale);
        if let Some(path) = crate::render::rounded_rect(grown, radius + layer as f32 * 2.0 * scale)
        {
            crate::draw::fill_path(&mut canvas, &path, theme::INK, 0.06);
        }
    }
    if let Some(path) = crate::render::rounded_rect(inner, radius) {
        let paint = Paint {
            shader: Pattern::new(
                picture.as_ref(),
                SpreadMode::Pad,
                FilterQuality::Nearest,
                1.0,
                Transform::from_translate(inner.x, inner.y),
            ),
            anti_alias: true,
            ..Paint::default()
        };
        canvas.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
        let rim = scale.round().max(1.0);
        crate::draw::stroke_path(
            &mut canvas,
            &path,
            [255, 255, 255],
            0.2,
            rim,
            LineCap::Butt,
            LineJoin::Round,
        );
    }
    Image::new(cw, ch, canvas.take())
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Binary data as text, so it can live in a text field of a file.
pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(BASE64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The reverse of [`base64_encode`]. `None` if the text is not valid.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text
        .bytes()
        .filter(|c| *c != b'=' && !c.is_ascii_whitespace())
    {
        let value = BASE64.iter().position(|b| *b == c)? as u32;
        acc = acc << 6 | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// The name of the text field in which a capture keeps its editable self.
const META_KEY: &str = "bruma";

/// Encodes an image as an RGB PNG (screens have no transparency).
pub fn encode_png(image: &Image) -> Result<Vec<u8>, png::EncodingError> {
    encode_png_with(image, None)
}

/// A PNG that also carries `meta` in a text field. Any viewer shows the
/// picture; Bruma can read the field back with [`decode_png_meta`].
pub fn encode_png_with(image: &Image, meta: Option<&str>) -> Result<Vec<u8>, png::EncodingError> {
    let mut rgb = Vec::with_capacity(image.width as usize * image.height as usize * 3);
    for px in image.rgba.chunks_exact(4) {
        rgb.extend_from_slice(&px[..3]);
    }
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, image.width, image.height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        // A screenshot should land instantly; the fast mode is still compact.
        encoder.set_compression(png::Compression::Fast);
        if let Some(meta) = meta {
            encoder.add_itxt_chunk(META_KEY.to_string(), meta.to_string())?;
        }
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&rgb)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn white(w: u32, h: u32) -> Image {
        Image::filled(w, h, [255, 255, 255, 255])
    }

    fn red_rect(rect: Rect) -> Annotation {
        Annotation {
            shape: Shape::Rect { rect },
            color: 0,
            stroke: 4.0,
        }
    }

    #[test]
    fn output_size_matches_rounded_selection() {
        let out = flatten(
            &white(200, 100),
            &[],
            Rect::new(10.4, 10.4, 50.0, 30.0),
            &Fonts::empty(),
        );
        assert_eq!((out.width, out.height), (50, 30));
    }

    #[test]
    fn annotation_lands_at_selection_offset() {
        let screen = white(200, 200);
        let items = [red_rect(Rect::new(60.0, 60.0, 40.0, 40.0))];
        let out = flatten(
            &screen,
            &items,
            Rect::new(50.0, 50.0, 100.0, 100.0),
            &Fonts::empty(),
        );
        assert_eq!(
            out.pixel(10, 30),
            [0xE5, 0x34, 0x2B, 255],
            "left border of the rect"
        );
        assert_eq!(
            out.pixel(30, 30),
            [255, 255, 255, 255],
            "inside stays untouched"
        );
        assert_eq!(
            out.pixel(2, 2),
            [255, 255, 255, 255],
            "outside stays untouched"
        );
    }

    #[test]
    fn annotation_outside_selection_changes_nothing() {
        let screen = white(200, 200);
        let items = [red_rect(Rect::new(150.0, 150.0, 30.0, 30.0))];
        let sel = Rect::new(10.0, 10.0, 50.0, 50.0);
        assert_eq!(
            flatten(&screen, &items, sel, &Fonts::empty()),
            screen.crop(sel)
        );
    }

    #[test]
    fn selection_at_screen_edge_is_clipped() {
        let screen = white(100, 100);
        let items = [red_rect(Rect::new(80.0, 80.0, 40.0, 40.0))];
        let out = flatten(
            &screen,
            &items,
            Rect::new(70.0, 70.0, 80.0, 80.0),
            &Fonts::empty(),
        );
        assert_eq!((out.width, out.height), (30, 30));
    }

    #[test]
    fn empty_selection_gives_empty_image() {
        let out = flatten(
            &white(10, 10),
            &[red_rect(Rect::new(0.0, 0.0, 5.0, 5.0))],
            Rect::default(),
            &Fonts::empty(),
        );
        assert_eq!((out.width, out.height), (0, 0));
    }

    #[test]
    fn blur_flattens_detail() {
        let mut screen = white(64, 64);
        for y in 0..64 {
            for x in 0..64 {
                if (x + y) % 2 == 0 {
                    screen.set_pixel(x, y, [0, 0, 0, 255]);
                }
            }
        }
        let items = [Annotation {
            shape: Shape::Blur {
                rect: Rect::new(0.0, 0.0, 64.0, 64.0),
            },
            color: 0,
            stroke: 2.0,
        }];
        let out = flatten(&screen, &items, screen.bounds(), &Fonts::empty());
        let first = out.pixel(0, 0);
        assert_eq!(out.pixel(1, 0), first);
        assert_eq!(out.pixel(5, 5), first);
        assert!(
            (first[0] as i32 - 127).abs() <= 2,
            "block is the average grey, got {first:?}"
        );
    }

    #[test]
    fn focus_keeps_its_area_sharp_and_mists_the_rest() {
        // Fine vertical stripes: sharp they are black or white, misted they are grey.
        let mut screen = white(200, 120);
        for x in (0..200).step_by(2) {
            screen.fill_rect(Rect::new(x as f32, 0.0, 1.0, 120.0), [0, 0, 0, 255]);
        }
        let items = [Annotation {
            shape: Shape::Focus {
                rect: Rect::new(60.0, 30.0, 80.0, 60.0),
            },
            color: 0,
            stroke: 3.0,
        }];
        let out = flatten(&screen, &items, screen.bounds(), &Fonts::empty());
        assert_eq!(
            out.pixel(100, 60),
            [0, 0, 0, 255],
            "inside the focus area the stripe is intact"
        );
        assert_eq!(out.pixel(101, 60), [255, 255, 255, 255]);
        let outside = out.pixel(20, 60);
        assert!(
            (40..120).contains(&outside[0]),
            "outside is blurred to a dim grey, got {outside:?}"
        );
        assert_eq!(
            out.pixel(20, 60),
            out.pixel(21, 60),
            "and the stripes are gone"
        );
    }

    #[test]
    fn focus_outside_the_selection_still_mists_everything() {
        let screen = white(100, 100);
        let items = [Annotation {
            shape: Shape::Focus {
                rect: Rect::new(500.0, 500.0, 10.0, 10.0),
            },
            color: 0,
            stroke: 3.0,
        }];
        let out = flatten(&screen, &items, screen.bounds(), &Fonts::empty());
        assert!(out.pixel(50, 50)[0] < 200);
    }

    #[test]
    fn png_decodes_what_it_encodes() {
        let mut img = white(5, 4);
        img.set_pixel(2, 3, [1, 2, 3, 255]);
        assert_eq!(decode_png(&encode_png(&img).unwrap()).unwrap(), img);
        assert!(decode_png(b"not a png").is_err());
    }

    #[test]
    fn base64_round_trips_every_length() {
        for len in 0..40usize {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(
                base64_decode(&base64_encode(&bytes)).unwrap(),
                bytes,
                "length {len}"
            );
        }
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert!(base64_decode("not base64!").is_none());
    }

    #[test]
    fn a_capture_carries_its_editable_self() {
        let mut original = white(40, 30);
        original.set_pixel(3, 4, [10, 20, 30, 255]);
        let items = vec![red_rect(Rect::new(5.0, 5.0, 20.0, 10.0))];
        let flat = flatten(&original, &items, original.bounds(), &Fonts::empty());
        let meta = pack_editable(&original, &items).unwrap();
        let file = encode_png_with(&flat, Some(&meta)).unwrap();
        let (shown, found) = decode_png_meta(&file).unwrap();
        assert_eq!(shown, flat, "any viewer sees the finished picture");
        let editable = unpack_editable(&found.unwrap()).unwrap();
        assert_eq!(editable.original, original);
        assert_eq!(editable.items, items);
    }

    #[test]
    fn an_ordinary_png_has_nothing_to_edit() {
        let (_, meta) = decode_png_meta(&encode_png(&white(4, 4)).unwrap()).unwrap();
        assert!(meta.is_none());
        assert!(unpack_editable("something else").is_none());
        assert!(unpack_editable("bruma 1\n!!!\n").is_none());
    }

    #[test]
    fn frame_adds_a_margin_and_keeps_the_picture_in_the_middle() {
        let mut img = white(400, 200);
        img.fill_rect(Rect::new(0.0, 0.0, 400.0, 200.0), [200, 40, 40, 255]);
        let framed = frame(&img, 1.0);
        let pad = 28;
        assert_eq!(
            (framed.width, framed.height),
            (400 + 2 * pad, 200 + 2 * pad)
        );
        assert_eq!(
            framed.pixel(pad as i32 + 200, pad as i32 + 100),
            [200, 40, 40, 255]
        );
        let margin = framed.pixel(4, 4);
        assert!(
            margin[0] < 200 && margin[0] > 40,
            "the margin is the picture, dimmed and blurred: {margin:?}"
        );
        assert_eq!(
            (
                frame(&white(0, 0), 1.0).width,
                frame(&white(1, 1), 2.0).width
            ),
            (0, 1 + 2 * 56)
        );
    }

    #[test]
    fn png_round_trips() {
        let mut img = white(3, 2);
        img.set_pixel(1, 1, [10, 20, 30, 255]);
        let bytes = encode_png(&img).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; 3 * 2 * 3];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (3, 2));
        assert_eq!(&buf[(3 + 1) * 3..(3 + 1) * 3 + 3], &[10, 20, 30]);
    }
}
