//! Image fingerprints: solid/near-black filler detection for intake, and the
//! perceptual hashes Vitrine uses to keep the best copy of duplicate images.
use std::path::Path;

use anyhow::{Context, Result};
use image::imageops::FilterType;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const HASH_SIDE: u32 = 8;
const COLOR_SIDE: u32 = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionFingerprint {
    pub width: u32,
    pub height: u32,
    pub average_hash: u64,
    pub difference_hash: u64,
    pub color_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageFingerprint {
    #[serde(default)]
    pub solid_color: bool,
    pub content_sha256: String,
    pub strict_key: String,
    pub average_hash: u64,
    pub difference_hash: u64,
    pub color_key: String,
    pub detail_key: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub format: String,
    #[serde(default)]
    pub regions: Vec<RegionFingerprint>,
}

pub fn inspect_image(path: &Path) -> Result<ImageFingerprint> {
    let encoded =
        std::fs::read(path).with_context(|| format!("读取图片失败: {}", path.display()))?;
    inspect_image_bytes(&encoded).with_context(|| format!("检查图片失败: {}", path.display()))
}

pub fn inspect_image_bytes(encoded: &[u8]) -> Result<ImageFingerprint> {
    fingerprint_from_bytes(encoded, split_regions)
}

#[cfg(test)]
fn inspect_image_bytes_sequential(encoded: &[u8]) -> Result<ImageFingerprint> {
    fingerprint_from_bytes(encoded, split_regions_sequential)
}

fn fingerprint_from_bytes(
    encoded: &[u8],
    split_regions: impl FnOnce(&image::DynamicImage) -> Vec<RegionFingerprint>,
) -> Result<ImageFingerprint> {
    let format = image::guess_format(encoded)
        .map(|value| format!("{value:?}").to_ascii_uppercase())
        .unwrap_or_else(|_| "UNKNOWN".into());
    let image = image::load_from_memory(encoded).context("解码图片失败")?;
    let solid_color = is_solid_color(&image);
    let width = image.width();
    let height = image.height();
    let visual = visual_fingerprint(&image);
    let strict_key =
        hex_digest(format!("{:016x}:{:016x}:{}", visual.0, visual.1, visual.3).as_bytes());
    let regions = split_regions(&image);

    Ok(ImageFingerprint {
        solid_color,
        content_sha256: hex_digest(encoded),
        strict_key,
        average_hash: visual.0,
        difference_hash: visual.1,
        color_key: visual.2,
        detail_key: visual.3,
        width,
        height,
        bytes: encoded.len() as u64,
        format,
        regions,
    })
}

// Allow tiny JPEG rounding noise, but inspect every pixel so small details survive.
fn is_solid_color(image: &image::DynamicImage) -> bool {
    let rgba = image.to_rgba8();
    if rgba.pixels().all(|pixel| pixel[3] == 0) {
        return true;
    }
    let mut min = [255_u8; 4];
    let mut max = [0_u8; 4];
    for pixel in rgba.pixels() {
        for channel in 0..4 {
            min[channel] = min[channel].min(pixel[channel]);
            max[channel] = max[channel].max(pixel[channel]);
            if max[channel] - min[channel] > 5 {
                return is_near_black_placeholder(&rgba);
            }
        }
    }
    true
}

fn is_near_black_placeholder(rgba: &image::RgbaImage) -> bool {
    // Only large opaque pages can be nearly invisible filler; preserve pixel art and alpha shapes.
    if rgba.width() < 256 || rgba.height() < 256 {
        return false;
    }
    let pixels = u64::from(rgba.width()) * u64::from(rgba.height());
    let mut intensity = 0_u64;
    let mut visible = 0_u64;
    for pixel in rgba.pixels() {
        if pixel[3] != 255 {
            return false;
        }
        let peak = pixel[0].max(pixel[1]).max(pixel[2]);
        intensity += u64::from(peak);
        visible += u64::from(peak > 16);
        if intensity > pixels || visible * 2000 > pixels {
            return false;
        }
    }
    true
}

fn luma_hashes(image: &image::DynamicImage) -> (u64, u64) {
    let gray = image.to_luma8();
    let average = image::imageops::resize(&gray, HASH_SIDE, HASH_SIDE, FilterType::Triangle);
    let mean = average.pixels().map(|p| u64::from(p[0])).sum::<u64>() / 64;
    let mut average_hash = 0_u64;
    for (index, pixel) in average.pixels().enumerate() {
        if u64::from(pixel[0]) >= mean {
            average_hash |= 1_u64 << index;
        }
    }
    let difference = image::imageops::resize(&gray, HASH_SIDE + 1, HASH_SIDE, FilterType::Triangle);
    let mut difference_hash = 0_u64;
    for y in 0..HASH_SIDE {
        for x in 0..HASH_SIDE {
            if difference.get_pixel(x, y)[0] > difference.get_pixel(x + 1, y)[0] {
                difference_hash |= 1_u64 << (y * HASH_SIDE + x);
            }
        }
    }
    (average_hash, difference_hash)
}

fn quantized_digest(rgb: &image::RgbImage, side: u32) -> String {
    let resized = image::imageops::resize(rgb, side, side, FilterType::Triangle);
    let bytes: Vec<u8> = resized
        .pixels()
        .flat_map(|p| p.0.map(|channel| channel >> 4))
        .collect();
    hex_digest(&bytes)
}

fn compact_visual_fingerprint(image: &image::DynamicImage) -> (u64, u64, String) {
    let (average_hash, difference_hash) = luma_hashes(image);
    let rgb = image.to_rgb8();
    (
        average_hash,
        difference_hash,
        quantized_digest(&rgb, COLOR_SIDE),
    )
}

fn visual_fingerprint(image: &image::DynamicImage) -> (u64, u64, String, String) {
    let (average_hash, difference_hash) = luma_hashes(image);
    let rgb = image.to_rgb8();
    (
        average_hash,
        difference_hash,
        quantized_digest(&rgb, COLOR_SIDE),
        quantized_digest(&rgb, 32),
    )
}

fn region_windows(image: &image::DynamicImage) -> Vec<(u32, u32, u32, u32)> {
    let mut output = Vec::new();
    for (columns, rows) in [(2, 1), (3, 1), (1, 2), (1, 3), (2, 2), (3, 3)] {
        for row in 0..rows {
            for column in 0..columns {
                let x0 = image.width() * column / columns;
                let x1 = image.width() * (column + 1) / columns;
                let y0 = image.height() * row / rows;
                let y1 = image.height() * (row + 1) / rows;
                if x1 <= x0 || y1 <= y0 {
                    continue;
                }
                output.push((x0, y0, x1 - x0, y1 - y0));
            }
        }
    }
    output
}

fn region_fingerprint(
    image: &image::DynamicImage,
    x0: u32,
    y0: u32,
    width: u32,
    height: u32,
) -> RegionFingerprint {
    let region = image.crop_imm(x0, y0, width, height);
    let visual = compact_visual_fingerprint(&region);
    RegionFingerprint {
        width,
        height,
        average_hash: visual.0,
        difference_hash: visual.1,
        color_key: visual.2,
    }
}

fn split_regions(image: &image::DynamicImage) -> Vec<RegionFingerprint> {
    region_windows(image)
        .into_par_iter()
        .map(|(x0, y0, width, height)| region_fingerprint(image, x0, y0, width, height))
        .collect()
}

#[cfg(test)]
fn split_regions_sequential(image: &image::DynamicImage) -> Vec<RegionFingerprint> {
    region_windows(image)
        .into_iter()
        .map(|(x0, y0, width, height)| region_fingerprint(image, x0, y0, width, height))
        .collect()
}

fn hex_digest(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb, RgbImage};
    use std::path::Path;

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

    fn save_with_format(path: &Path, image: &RgbImage, format: image::ImageFormat) {
        image.save_with_format(path, format).unwrap();
    }

    #[test]
    fn parallel_fingerprint_matches_sequential_reference() {
        let dir = tempfile::tempdir().unwrap();
        let png_path = dir.path().join("wide.png");
        let jpeg_path = dir.path().join("tall.jpg");
        let bmp_path = dir.path().join("square.bmp");
        save_with_format(&png_path, &patterned(320, 240), image::ImageFormat::Png);
        save_with_format(&jpeg_path, &patterned(200, 400), image::ImageFormat::Jpeg);
        save_with_format(&bmp_path, &patterned(480, 480), image::ImageFormat::Bmp);

        let expected_320x240_region_sizes: [(u32, u32); 23] = [
            (160, 240),
            (160, 240),
            (106, 240),
            (107, 240),
            (107, 240),
            (320, 120),
            (320, 120),
            (320, 80),
            (320, 80),
            (320, 80),
            (160, 120),
            (160, 120),
            (160, 120),
            (160, 120),
            (106, 80),
            (107, 80),
            (107, 80),
            (106, 80),
            (107, 80),
            (107, 80),
            (106, 80),
            (107, 80),
            (107, 80),
        ];

        for path in [&png_path, &jpeg_path, &bmp_path] {
            let encoded = std::fs::read(path).unwrap();
            let production = inspect_image_bytes(&encoded).unwrap();
            let sequential = inspect_image_bytes_sequential(&encoded).unwrap();
            assert_eq!(
                production,
                sequential,
                "fingerprint mismatch for {}",
                path.display()
            );
            assert_eq!(
                production.regions.len(),
                23,
                "region order/count mismatch for {}",
                path.display()
            );
        }

        let png = inspect_image(&png_path).unwrap();
        let sizes: Vec<(u32, u32)> = png
            .regions
            .iter()
            .map(|region| (region.width, region.height))
            .collect();
        assert_eq!(
            sizes, expected_320x240_region_sizes,
            "320×240 region window order drifted"
        );
    }
}
