use std::cmp::Ordering;
use std::path::Path;

use hanabi::image_dedup::{
    classify_similarity, init_schema, inspect_image, mark_work_status, record_work, remove_work,
    MatchKind, WorkStatus,
};
use hanabi::model::{Author, ImageRef, MediaItem, SourceKind};
use image::{ImageBuffer, Rgb, RgbImage};

fn item(source: SourceKind, id: &str, title: &str) -> MediaItem {
    MediaItem {
        source,
        source_id: id.into(),
        author: Author {
            name: "画师".into(),
            url: "https://example.test/artist".into(),
        },
        title: Some(title.into()),
        url: format!("https://example.test/{id}"),
        tags: vec![],
        bookmark_count: None,
        is_r18: false,
        pixiv_type: None,
        page_count: 1,
        images: vec![ImageRef {
            url: format!("https://example.test/{id}.png"),
            referer: None,
            fallback_urls: vec![],
        }],
        origin: "test".into(),
    }
}

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
fn strict_same_survives_resolution_change_and_prefers_more_pixels() {
    let dir = tempfile::tempdir().unwrap();
    let small_path = dir.path().join("small.png");
    let large_path = dir.path().join("large.png");
    save_png(&small_path, &patterned(320, 240));
    save_png(&large_path, &patterned(1280, 960));

    let small = inspect_image(&small_path).unwrap();
    let large = inspect_image(&large_path).unwrap();

    assert_eq!(
        classify_similarity(&small, &large),
        MatchKind::StrictSame,
        "small={small:?} large={large:?}"
    );
    assert_eq!(large.quality_cmp(&small), Ordering::Greater);
    assert_eq!(small.dimensions_label(), "320×240");
}

#[test]
fn a_small_visual_edit_is_similar_but_never_strict_same() {
    let dir = tempfile::tempdir().unwrap();
    let original_path = dir.path().join("original.png");
    let edited_path = dir.path().join("edited.png");
    let original = patterned(640, 480);
    let mut edited = original.clone();
    for y in 200..260 {
        for x in 280..360 {
            edited.put_pixel(x, y, Rgb([255, 255, 255]));
        }
    }
    save_png(&original_path, &original);
    save_png(&edited_path, &edited);

    let original = inspect_image(&original_path).unwrap();
    let edited = inspect_image(&edited_path).unwrap();
    assert!(matches!(
        classify_similarity(&original, &edited),
        MatchKind::Similar { .. }
    ));
    assert_ne!(original.strict_key, edited.strict_key);
}

#[test]
fn a_split_panel_is_partial_similarity_and_never_auto_deduplicated() {
    let dir = tempfile::tempdir().unwrap();
    let full_path = dir.path().join("full.png");
    let panel_path = dir.path().join("panel.png");
    let full = patterned(600, 400);
    let panel = image::imageops::crop_imm(&full, 0, 0, 300, 400).to_image();
    save_png(&full_path, &full);
    save_png(&panel_path, &panel);

    let full = inspect_image(&full_path).unwrap();
    let panel = inspect_image(&panel_path).unwrap();
    assert!(matches!(
        classify_similarity(&full, &panel),
        MatchKind::Partial { .. }
    ));
}

#[test]
fn unrelated_images_are_not_marked_similar() {
    let dir = tempfile::tempdir().unwrap();
    let first_path = dir.path().join("first.png");
    let second_path = dir.path().join("second.png");
    save_png(&first_path, &patterned(640, 480));
    let second = ImageBuffer::from_fn(640, 480, |x, y| {
        if (x / 20 + y / 20) % 2 == 0 {
            Rgb([0, 0, 0])
        } else {
            Rgb([255, 255, 255])
        }
    });
    save_png(&second_path, &second);

    let first = inspect_image(&first_path).unwrap();
    let second = inspect_image(&second_path).unwrap();
    assert_eq!(classify_similarity(&first, &second), MatchKind::Different);
}

#[test]
fn fingerprint_history_can_be_recorded_published_and_removed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("image.png");
    save_png(&path, &patterned(320, 240));
    let fingerprint = inspect_image(&path).unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let work = item(SourceKind::Pixiv, "p1", "作品");

    record_work(&conn, &work, &[fingerprint], WorkStatus::Pending).unwrap();
    let status: String = conn
        .query_row("SELECT status FROM image_fingerprints", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(status, "pending");

    mark_work_status(&conn, &work, WorkStatus::Published).unwrap();
    let status: String = conn
        .query_row("SELECT status FROM image_fingerprints", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(status, "published");

    remove_work(&conn, &work).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM image_fingerprints", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn solid_colors_and_jpeg_noise_do_not_match_but_small_details_survive() {
    let dir = tempfile::tempdir().unwrap();
    for (index, color) in [[0, 0, 0], [255, 255, 255], [230, 30, 80]]
        .iter()
        .enumerate()
    {
        let path = dir.path().join(format!("{index}.png"));
        save_png(&path, &ImageBuffer::from_pixel(100, 100, Rgb(*color)));
        let fingerprint = inspect_image(&path).unwrap();
        assert!(fingerprint.solid_color);
        assert_eq!(
            classify_similarity(&fingerprint, &fingerprint),
            MatchKind::Different
        );
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
    let fingerprint = inspect_image(&path).unwrap();
    assert!(fingerprint.solid_color);
    assert_eq!(
        classify_similarity(&fingerprint, &fingerprint),
        MatchKind::Different
    );
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
