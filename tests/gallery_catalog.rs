use std::path::Path;

use hanabi::gallery_catalog::{scan_catalog, CatalogImage};
use hanabi::model::SourceKind;
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

fn save(path: &Path, image: &RgbImage) {
    image
        .save_with_format(path, image::ImageFormat::Png)
        .unwrap();
}

fn entry(path: &Path, source: SourceKind, source_id: &str, key: &str) -> CatalogImage {
    CatalogImage {
        work_id: format!("{}:{source_id}", source.as_str()),
        source,
        source_id: source_id.into(),
        title: source_id.into(),
        author_name: "画师".into(),
        source_url: format!("https://example.test/{source_id}"),
        page_index: 0,
        r2_key: key.into(),
        path: path.into(),
    }
}

#[test]
fn strict_group_keeps_the_highest_resolution_and_never_becomes_similar() {
    let dir = tempfile::tempdir().unwrap();
    let low = dir.path().join("low.png");
    let high = dir.path().join("high.png");
    save(&low, &patterned(320, 240));
    save(&high, &patterned(1280, 960));

    let report = scan_catalog(&[
        entry(&low, SourceKind::X, "x1", "x/low.png"),
        entry(&high, SourceKind::Pixiv, "p1", "pixiv/high.png"),
    ])
    .unwrap();

    assert_eq!(report.strict_groups.len(), 1);
    assert_eq!(report.strict_groups[0].keep.image.r2_key, "pixiv/high.png");
    assert_eq!(report.strict_groups[0].remove.len(), 1);
    assert_eq!(report.strict_groups[0].remove[0].image.r2_key, "x/low.png");
    assert!(report.similar_pairs.is_empty());
}

#[test]
fn edited_image_is_review_only_and_does_not_enter_auto_remove_plan() {
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
    save(&original_path, &original);
    save(&edited_path, &edited);

    let report = scan_catalog(&[
        entry(
            &original_path,
            SourceKind::Douyin,
            "d1",
            "douyin/original.png",
        ),
        entry(&edited_path, SourceKind::X, "x2", "x/edited.png"),
    ])
    .unwrap();

    assert!(report.strict_groups.is_empty());
    assert_eq!(report.similar_pairs.len(), 1);
    assert!(report.similar_pairs[0].distance > 0);
}

#[test]
fn unrelated_images_produce_no_findings() {
    let dir = tempfile::tempdir().unwrap();
    let first_path = dir.path().join("first.png");
    let second_path = dir.path().join("second.png");
    save(&first_path, &patterned(640, 480));
    let checker = ImageBuffer::from_fn(640, 480, |x, y| {
        if (x / 20 + y / 20) % 2 == 0 {
            Rgb([0, 0, 0])
        } else {
            Rgb([255, 255, 255])
        }
    });
    save(&second_path, &checker);

    let report = scan_catalog(&[
        entry(&first_path, SourceKind::Pixiv, "p2", "pixiv/first.png"),
        entry(&second_path, SourceKind::X, "x3", "x/checker.png"),
    ])
    .unwrap();

    assert!(report.strict_groups.is_empty());
    assert!(report.similar_pairs.is_empty());
}

#[test]
fn split_panel_enters_review_report_but_not_strict_removal() {
    let dir = tempfile::tempdir().unwrap();
    let full_path = dir.path().join("full.png");
    let panel_path = dir.path().join("panel.png");
    let full = patterned(600, 400);
    let panel = image::imageops::crop_imm(&full, 300, 0, 300, 400).to_image();
    save(&full_path, &full);
    save(&panel_path, &panel);

    let report = scan_catalog(&[
        entry(&full_path, SourceKind::Pixiv, "full", "pixiv/full.png"),
        entry(&panel_path, SourceKind::Douyin, "panel", "douyin/panel.png"),
    ])
    .unwrap();

    assert!(report.strict_groups.is_empty());
    assert_eq!(report.similar_pairs.len(), 1);
    assert_eq!(report.similar_pairs[0].kind, "partial");
}

#[test]
fn same_platform_same_post_strict_images_produce_no_finding() {
    let dir = tempfile::tempdir().unwrap();
    let first_path = dir.path().join("first.png");
    let second_path = dir.path().join("second.png");
    save(&first_path, &patterned(320, 240));
    save(&second_path, &patterned(1280, 960));

    let report = scan_catalog(&[
        entry(&first_path, SourceKind::Pixiv, "p7", "pixiv/p7-1.png"),
        entry(&second_path, SourceKind::Pixiv, "p7", "pixiv/p7-2.png"),
    ])
    .unwrap();

    assert!(report.strict_groups.is_empty());
    assert!(report.similar_pairs.is_empty());
}

#[test]
fn same_platform_same_post_similar_images_produce_no_finding() {
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
    save(&original_path, &original);
    save(&edited_path, &edited);

    let report = scan_catalog(&[
        entry(&original_path, SourceKind::Douyin, "d8", "douyin/d8-1.png"),
        entry(&edited_path, SourceKind::Douyin, "d8", "douyin/d8-2.png"),
    ])
    .unwrap();

    assert!(report.strict_groups.is_empty());
    assert!(report.similar_pairs.is_empty());
}

#[test]
fn same_platform_different_posts_similar_images_enter_review_report() {
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
    save(&original_path, &original);
    save(&edited_path, &edited);

    let report = scan_catalog(&[
        entry(&original_path, SourceKind::X, "x9", "x/x9.png"),
        entry(&edited_path, SourceKind::X, "x10", "x/x10.png"),
    ])
    .unwrap();

    assert!(report.strict_groups.is_empty());
    assert_eq!(report.similar_pairs.len(), 1);
    assert_eq!(report.similar_pairs[0].kind, "visual");
}
