use hanabi::gallery_sync::{
    catalog_media_url, import_catalog_image, needs_catalog_image, CatalogImageRecord,
};
use hanabi::image_dedup::{init_schema, ImageFingerprint, RegionFingerprint};
use hanabi::model::SourceKind;
use rusqlite::Connection;

fn record(page_index: u32, sha256: &str) -> CatalogImageRecord {
    CatalogImageRecord {
        work_id: "pixiv:42".into(),
        source: SourceKind::Pixiv,
        source_id: "42".into(),
        source_url: "https://www.pixiv.net/artworks/42".into(),
        title: "sample".into(),
        page_index,
        r2_key: format!("pixiv/42/batch/{page_index:02}.jpg"),
        byte_size: 1234,
        content_type: "image/jpeg".into(),
        sha256: sha256.into(),
    }
}

fn fingerprint(sha256: &str) -> ImageFingerprint {
    ImageFingerprint {
        content_sha256: sha256.into(),
        strict_key: "strict".into(),
        average_hash: 1,
        difference_hash: 2,
        color_key: "color".into(),
        detail_key: "detail".into(),
        width: 100,
        height: 200,
        bytes: 1234,
        format: "JPEG".into(),
        regions: vec![RegionFingerprint {
            width: 50,
            height: 200,
            average_hash: 3,
            difference_hash: 4,
            color_key: "region".into(),
        }],
    }
}

#[test]
fn catalog_import_is_idempotent_and_marks_the_image_published() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let image = record(0, "sha-a");

    assert!(needs_catalog_image(&conn, &image).unwrap());
    assert!(import_catalog_image(&conn, &image, &fingerprint("sha-a")).unwrap());
    assert!(!needs_catalog_image(&conn, &image).unwrap());
    assert!(!import_catalog_image(&conn, &image, &fingerprint("sha-a")).unwrap());

    let row: (i64, String) = conn
        .query_row(
            "SELECT COUNT(*),MIN(status) FROM image_fingerprints",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(row, (1, "published".into()));
}

#[test]
fn changed_catalog_image_updates_one_page_without_deleting_siblings() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let first = record(0, "sha-a");
    let second = record(1, "sha-b");
    import_catalog_image(&conn, &first, &fingerprint("sha-a")).unwrap();
    import_catalog_image(&conn, &second, &fingerprint("sha-b")).unwrap();

    let changed = record(0, "sha-c");
    assert!(needs_catalog_image(&conn, &changed).unwrap());
    assert!(import_catalog_image(&conn, &changed, &fingerprint("sha-c")).unwrap());

    let rows: Vec<(i64, String)> = conn
        .prepare(
            "SELECT image_index,content_sha256 FROM image_fingerprints ORDER BY image_index",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(rows, vec![(0, "sha-c".into()), (1, "sha-b".into())]);
}

#[test]
fn catalog_media_url_preserves_r2_path_segments() {
    assert_eq!(
        catalog_media_url("https://gallery.example/", "pixiv/42/batch/00.jpg").unwrap(),
        "https://gallery.example/media/pixiv/42/batch/00.jpg"
    );
}
