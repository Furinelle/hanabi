use std::path::Path;

use hanabi::image_dedup::inspect_image;
use image::{ImageBuffer, Rgb, RgbImage};

fn patterned(width: u32, height: u32) -> RgbImage {
    ImageBuffer::from_fn(width, height, |x, y| {
        let bx = x * 4 / width;
        let by = y * 4 / height;
        Rgb([
            (bx * 53 + by * 17) as u8,
            (bx * 19 + by * 61) as u8,
            (bx * 31 + by * 29) as u8,
        ])
    })
}

fn save_png(path: &Path, image: &RgbImage) {
    image
        .save_with_format(path, image::ImageFormat::Png)
        .unwrap();
}

#[test]
fn visual_keys_survive_a_resolution_change() {
    // Vitrine treats equal color/detail keys with at most one differing hash bit as the same image.
    let dir = tempfile::tempdir().unwrap();
    let small_path = dir.path().join("small.png");
    let large_path = dir.path().join("large.png");
    save_png(&small_path, &patterned(320, 240));
    save_png(&large_path, &patterned(1280, 960));

    let small = inspect_image(&small_path).unwrap();
    let large = inspect_image(&large_path).unwrap();
    assert_eq!(small.color_key, large.color_key);
    assert_eq!(small.detail_key, large.detail_key);
    assert!(
        (small.average_hash ^ large.average_hash).count_ones()
            + (small.difference_hash ^ large.difference_hash).count_ones()
            <= 1
    );
    assert_ne!(small.content_sha256, large.content_sha256);
}

#[test]
fn solid_colors_and_jpeg_noise_are_solid_but_small_details_survive() {
    let dir = tempfile::tempdir().unwrap();
    for (index, color) in [[0, 0, 0], [255, 255, 255], [230, 30, 80]]
        .iter()
        .enumerate()
    {
        let path = dir.path().join(format!("{index}.png"));
        save_png(&path, &ImageBuffer::from_pixel(100, 100, Rgb(*color)));
        let fingerprint = inspect_image(&path).unwrap();
        assert!(fingerprint.solid_color);
    }
    let path = dir.path().join("noise.png");
    let mut noisy = ImageBuffer::from_fn(100, 100, |x, y| Rgb([(x % 3) as u8, (y % 6) as u8, 0]));
    save_png(&path, &noisy);
    assert!(inspect_image(&path).unwrap().solid_color);
    noisy.put_pixel(50, 50, Rgb([30, 30, 30]));
    save_png(&path, &noisy);
    assert!(!inspect_image(&path).unwrap().solid_color);
}

#[test]
fn transparency_details_are_not_solid_color() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alpha.png");
    let image = image::RgbaImage::from_fn(20, 20, |x, _| {
        image::Rgba([0, 0, 0, if x < 10 { 0 } else { 255 }])
    });
    image.save(&path).unwrap();
    assert!(!inspect_image(&path).unwrap().solid_color);
}

#[test]
fn fully_transparent_images_are_solid_despite_hidden_rgb() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transparent.png");
    let image = image::RgbaImage::from_fn(20, 20, |x, _| {
        image::Rgba([if x < 10 { 0 } else { 255 }, 0, 0, 0])
    });
    image.save(&path).unwrap();
    assert!(inspect_image(&path).unwrap().solid_color);
}

#[test]
fn near_black_filter_preserves_dark_artwork_small_details_and_alpha() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dark.png");
    let mut image = image::RgbaImage::from_fn(256, 256, |x, _| {
        image::Rgba([if x < 128 { 0 } else { 2 }, 0, 0, 255])
    });
    image.put_pixel(0, 0, image::Rgba([30, 30, 30, 255]));
    image.save(&path).unwrap();
    assert!(
        !inspect_image(&path).unwrap().solid_color,
        "dark layers exceed mean 1"
    );

    image = image::RgbaImage::from_pixel(256, 256, image::Rgba([0, 0, 0, 255]));
    for x in 0..33 {
        image.put_pixel(x, 0, image::Rgba([17, 17, 17, 255]));
    }
    image.save(&path).unwrap();
    assert!(
        !inspect_image(&path).unwrap().solid_color,
        "visible detail exceeds 0.05 percent"
    );
    image.put_pixel(32, 0, image::Rgba([0, 0, 0, 255]));
    image.save(&path).unwrap();
    assert!(inspect_image(&path).unwrap().solid_color);
    image.put_pixel(255, 255, image::Rgba([0, 0, 0, 0]));
    image.save(&path).unwrap();
    assert!(
        !inspect_image(&path).unwrap().solid_color,
        "alpha shape survives"
    );

    let mut small = image::RgbImage::new(100, 100);
    small.put_pixel(50, 50, image::Rgb([30, 30, 30]));
    small.save(&path).unwrap();
    assert!(
        !inspect_image(&path).unwrap().solid_color,
        "small pixel detail survives"
    );
}

#[test]
fn near_black_filter_respects_mean_intensity_and_minimum_size() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("boundary.png");
    let mut image = image::RgbImage::from_fn(256, 256, |_, y| {
        image::Rgb(if y < 16 { [16; 3] } else { [0; 3] })
    });
    image.save(&path).unwrap();
    assert!(inspect_image(&path).unwrap().solid_color, "mean exactly 1");
    image.put_pixel(0, 0, image::Rgb([17; 3]));
    image.save(&path).unwrap();
    assert!(!inspect_image(&path).unwrap().solid_color, "mean above 1");

    let mut small = image::RgbImage::new(255, 256);
    small.put_pixel(0, 0, image::Rgb([17; 3]));
    small.save(&path).unwrap();
    assert!(
        !inspect_image(&path).unwrap().solid_color,
        "width below 256"
    );
}
