//! Synthetic checks of the gallery strip join, the same cases as Vitrine's
//! tools/test_stitch_strips.py: shuffled strips are found and joined pixel for pixel, repeated
//! columns are dropped, the better copy of a repeated whole picture is kept, and other crops or
//! colourings, strips with missing columns, unrelated pages and pages sharing a layout stay.
use std::io::Cursor;

use hanabi::stitch::{stitch_pages, Page};
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, RgbImage};

struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }

    fn normal(&mut self) -> f64 {
        let (u, v) = (self.unit().max(1e-12), self.unit());
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }
}

/// Fine detail for the seams over large shapes, like a drawn picture.
fn picture(seed: u64, width: u32, height: u32) -> RgbImage {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let coarse = RgbImage::from_fn(width / 160, height / 160, |_, _| {
        image::Rgb([0, 0, 0].map(|_: u8| (rng.unit() * 255.0) as u8))
    });
    let coarse = image::imageops::resize(&coarse, width / 4, height / 4, FilterType::CatmullRom);
    let fine: Vec<f64> = coarse
        .as_raw()
        .iter()
        .map(|&c| rng.normal() + f64::from(c) / 40.0)
        .collect();
    let (lo, hi) = fine
        .iter()
        .fold((f64::MAX, f64::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
    let small = RgbImage::from_raw(
        width / 4,
        height / 4,
        fine.iter()
            .map(|v| ((v - lo) / (hi - lo) * 255.0) as u8)
            .collect(),
    )
    .unwrap();
    let mut big = image::imageops::resize(&small, width, height, FilterType::CatmullRom);
    for p in big.iter_mut() {
        *p = (f64::from(*p) + rng.normal() * 6.0).clamp(0.0, 255.0) as u8;
    }
    big
}

fn crop(image: &RgbImage, x: u32, y: u32, w: u32, h: u32) -> RgbImage {
    image::imageops::crop_imm(image, x, y, w, h).to_image()
}

fn encode(image: &RgbImage, format: ImageFormat) -> Vec<u8> {
    let mut bytes = Vec::new();
    match format {
        ImageFormat::Jpeg => {
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 92)
                .encode_image(image)
                .unwrap();
        }
        _ => image
            .write_to(&mut Cursor::new(&mut bytes), format)
            .unwrap(),
    }
    bytes
}

fn pages(images: &[RgbImage], format: ImageFormat) -> Vec<Page> {
    let (ext, content_type) = match format {
        ImageFormat::Jpeg => ("jpg", "image/jpeg"),
        _ => ("png", "image/png"),
    };
    images
        .iter()
        .enumerate()
        .map(|(n, image)| {
            (
                format!("p{n:02}.{ext}"),
                content_type,
                encode(image, format),
            )
        })
        .collect()
}

fn decode(page: &Page) -> RgbImage {
    image::load_from_memory(&page.2).unwrap().to_rgb8()
}

fn names(pages: &[Page]) -> Vec<&str> {
    pages.iter().map(|p| p.0.as_str()).collect()
}

fn have(tool: &str) -> bool {
    std::process::Command::new(tool)
        .arg("-version")
        .output()
        .is_ok()
}

#[test]
fn shuffled_jpeg_strips_join_and_beat_a_smaller_copy() {
    let whole = picture(1, 1440, 960);
    let strips = [0, 480, 960].map(|x| crop(&whole, x, 0, 480, 960));
    let small = image::imageops::resize(&whole, 720, 480, FilterType::Triangle);
    let input = pages(
        &[
            strips[2].clone(),
            strips[0].clone(),
            small,
            strips[1].clone(),
        ],
        ImageFormat::Jpeg,
    );
    let decoded: Vec<RgbImage> = [1, 3, 0].map(|i| decode(&input[i])).into();
    let out = stitch_pages(input);
    assert_eq!(out.len(), 1, "{:?}", names(&out));
    let joined = decode(&out[0]);
    assert_eq!((joined.width(), joined.height()), (1440, 960));
    if have("jpegtran") && have("djpeg") {
        assert_eq!(out[0].1, "image/jpeg");
    } else {
        // Without jpegtran the join is lossless WebP of the decoded strips.
        assert_eq!(out[0].1, "image/webp");
        let mut expected = RgbImage::new(1440, 960);
        for (n, strip) in decoded.iter().enumerate() {
            image::imageops::replace(&mut expected, strip, n as i64 * 480, 0);
        }
        assert_eq!(joined, expected);
    }
}

#[test]
fn repeated_columns_are_dropped_and_the_picture_comes_back_exactly() {
    let whole = picture(1, 1440, 960);
    let input = pages(
        &[crop(&whole, 0, 0, 700, 960), crop(&whole, 697, 0, 743, 960)],
        ImageFormat::Png,
    );
    let out = stitch_pages(input);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].1, "image/webp");
    assert_eq!(decode(&out[0]), whole);
}

#[test]
fn stacked_strips_join_top_to_bottom() {
    let whole = picture(1, 1440, 960);
    let input = pages(
        &[
            crop(&whole, 0, 0, 1440, 500),
            crop(&whole, 0, 500, 1440, 460),
        ],
        ImageFormat::Png,
    );
    let out = stitch_pages(input);
    assert_eq!(out.len(), 1);
    assert_eq!(decode(&out[0]), whole);
}

#[test]
fn other_crops_and_unrelated_pages_stay_beside_the_join() {
    let whole = picture(1, 1440, 960);
    let strips = [0, 480, 960].map(|x| crop(&whole, x, 0, 480, 960));
    let mut variant = crop(&whole, 90, 60, 1260, 840);
    for p in variant.iter_mut() {
        *p = (f64::from(*p) * 0.8 + 30.0).clamp(0.0, 255.0) as u8;
    }
    let mut images = strips.to_vec();
    images.extend([variant, picture(7, 1440, 960)]);
    let out = stitch_pages(pages(&images, ImageFormat::Png));
    assert_eq!(names(&out), ["p00.webp", "p03.png", "p04.png"]);
}

#[test]
fn strips_missing_columns_or_unrelated_pages_are_left_alone() {
    let whole = picture(1, 1440, 960);
    for images in [
        vec![crop(&whole, 0, 0, 700, 960), crop(&whole, 706, 0, 734, 960)],
        vec![
            picture(2, 720, 960),
            picture(3, 720, 960),
            picture(4, 720, 960),
        ],
    ] {
        let input = pages(&images, ImageFormat::Png);
        let before = names(&input).join(",");
        assert_eq!(names(&stitch_pages(input)).join(","), before);
    }
}

#[test]
fn pages_sharing_a_layout_are_not_one_picture() {
    // Different pictures in one letterbox frame line up at every edge.
    let framed: Vec<RgbImage> = [5, 6]
        .map(|seed| {
            let mut f = picture(seed, 480, 960);
            for (x, y, p) in f.enumerate_pixels_mut() {
                if !(40..440).contains(&x) {
                    *p = image::Rgb([255; 3]);
                }
                if !(200..760).contains(&y) {
                    *p = image::Rgb([0; 3]);
                }
            }
            f
        })
        .into();
    let out = stitch_pages(pages(&framed, ImageFormat::Png));
    assert_eq!(out.len(), 2);
    // Frames in shared bars whose backgrounds differ jump in brightness at the would-be seam.
    let shaded: Vec<RgbImage> = [220u8, 40]
        .map(|shade| {
            RgbImage::from_fn(480, 960, |_, y| match y {
                0..200 | 760.. => image::Rgb([0; 3]),
                400..420 => image::Rgb([128; 3]),
                _ => image::Rgb([shade; 3]),
            })
        })
        .into();
    let out = stitch_pages(pages(&shaded, ImageFormat::Png));
    assert_eq!(out.len(), 2);
}

#[test]
fn single_pages_and_unequal_sizes_pass_through_undecoded() {
    let lone = pages(&[picture(1, 480, 320)], ImageFormat::Png);
    assert_eq!(stitch_pages(lone.clone()), lone);
    let mixed = pages(
        &[picture(1, 480, 320), picture(2, 400, 300)],
        ImageFormat::Png,
    );
    assert_eq!(stitch_pages(mixed.clone()), mixed);
    let broken: Vec<Page> = vec![
        ("a.jpg".into(), "image/jpeg", b"not an image".to_vec()),
        ("b.jpg".into(), "image/jpeg", b"not an image".to_vec()),
    ];
    assert_eq!(stitch_pages(broken.clone()), broken);
    let _ = DynamicImage::new_rgb8(1, 1);
}
