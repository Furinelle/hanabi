use std::cmp::Ordering;
use std::path::Path;

use hanabi::image_dedup::{
    classify_similarity, evaluate_work, init_schema, inspect_image, mark_work_status, record_work,
    remove_work, render_review_notice, ExactAction, MatchKind, WorkStatus,
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
fn catalog_replaces_pending_lower_quality_but_not_published_history() {
    let dir = tempfile::tempdir().unwrap();
    let small_path = dir.path().join("small.png");
    let large_path = dir.path().join("large.png");
    save_png(&small_path, &patterned(320, 240));
    save_png(&large_path, &patterned(1280, 960));
    let small = inspect_image(&small_path).unwrap();
    let large = inspect_image(&large_path).unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();

    let pixiv = item(SourceKind::Pixiv, "p1", "低清版");
    let x = item(SourceKind::X, "x1", "高清版");
    record_work(
        &conn,
        &pixiv,
        std::slice::from_ref(&small),
        WorkStatus::Pending,
    )
    .unwrap();

    let pending = evaluate_work(&conn, &x, std::slice::from_ref(&large)).unwrap();
    assert!(matches!(
        pending.exact_action,
        ExactAction::ReplacePending(ref old) if old.source_id == "p1"
    ));

    mark_work_status(&conn, &pixiv, WorkStatus::Published).unwrap();
    let published = evaluate_work(&conn, &x, std::slice::from_ref(&large)).unwrap();
    assert!(matches!(
        published.exact_action,
        ExactAction::SkipCurrent(ref old) if old.status == WorkStatus::Published
    ));

    remove_work(&conn, &pixiv).unwrap();
    assert!(matches!(
        evaluate_work(&conn, &x, &[large]).unwrap().exact_action,
        ExactAction::None
    ));
}

#[test]
fn similar_notice_contains_both_sources_resolution_and_file_size() {
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
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let pixiv = item(SourceKind::Pixiv, "p2", "原图");
    let douyin = item(SourceKind::Douyin, "d2", "改图");
    record_work(
        &conn,
        &pixiv,
        std::slice::from_ref(&original),
        WorkStatus::Published,
    )
    .unwrap();

    let evaluation = evaluate_work(&conn, &douyin, &[edited]).unwrap();
    assert!(matches!(evaluation.exact_action, ExactAction::None));
    assert_eq!(evaluation.similar.len(), 1);
    let notice = render_review_notice(&evaluation.similar);
    assert!(notice.contains("相似图片"));
    assert!(notice.contains("当前 640×480"));
    assert!(notice.contains("Pixiv p2"));
    assert!(notice.contains("640×480"));
    assert!(notice.contains("KiB"));
}

#[test]
fn mixed_work_drops_only_strict_duplicate_images_and_keeps_unique_ones() {
    let dir = tempfile::tempdir().unwrap();
    let duplicate_path = dir.path().join("duplicate.png");
    let unique_path = dir.path().join("unique.png");
    save_png(&duplicate_path, &patterned(640, 480));
    let unique = ImageBuffer::from_fn(640, 480, |x, y| {
        Rgb([(x % 251) as u8, (y % 241) as u8, ((x + y) % 239) as u8])
    });
    save_png(&unique_path, &unique);
    let duplicate = inspect_image(&duplicate_path).unwrap();
    let unique = inspect_image(&unique_path).unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let old = item(SourceKind::Pixiv, "p3", "已收录");
    let mut mixed = item(SourceKind::X, "x3", "两张图");
    mixed.page_count = 2;
    mixed.images.push(ImageRef {
        url: "https://example.test/x3-2.png".into(),
        referer: None,
        fallback_urls: vec![],
    });
    record_work(
        &conn,
        &old,
        std::slice::from_ref(&duplicate),
        WorkStatus::Published,
    )
    .unwrap();

    let evaluation = evaluate_work(&conn, &mixed, &[duplicate, unique]).unwrap();
    assert!(matches!(evaluation.exact_action, ExactAction::None));
    assert_eq!(evaluation.drop_current_indices, vec![0]);
}
