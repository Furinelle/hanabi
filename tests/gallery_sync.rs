use hanabi::gallery_sync::{
    catalog_media_url, fingerprint_upload_json, import_catalog_image, needs_catalog_image,
    sync_gallery_fingerprints, CatalogImageRecord,
};
use hanabi::image_dedup::{init_schema, ImageFingerprint, RegionFingerprint};
use hanabi::model::SourceKind;
use rusqlite::Connection;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

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
        solid_color: false,
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
        .prepare("SELECT image_index,content_sha256 FROM image_fingerprints ORDER BY image_index")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(rows, vec![(0, "sha-c".into()), (1, "sha-b".into())]);
}

#[test]
fn fingerprint_upload_uses_hex_hashes() {
    let body = fingerprint_upload_json("pixiv/1.jpg", &fingerprint("sha-a"));
    assert_eq!(body["r2_key"], "pixiv/1.jpg");
    assert_eq!(body["average_hash"], "0000000000000001");
    assert_eq!(body["regions"][0]["difference_hash"], "0000000000000004");
}

#[test]
fn catalog_media_url_preserves_r2_path_segments() {
    assert_eq!(
        catalog_media_url("https://gallery.example/", "pixiv/42/batch/00.jpg").unwrap(),
        "https://gallery.example/media/pixiv/42/batch/00.jpg"
    );
}

fn digest(image: &CatalogImageRecord) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&fingerprint_upload_json(
                &image.r2_key,
                &fingerprint(&image.sha256),
            ))
            .unwrap()
        )
    )
}

#[test]
fn fingerprint_digest_matches_catalog_contract() {
    assert_eq!(
        digest(&record(0, "sha-a")),
        "7d474f4b43cce9d9d5f182bbaf5176672905e82d8ffbe389feb6a528630052c2"
    );
}

fn catalog_record(image: &CatalogImageRecord, remote_digest: Option<String>) -> Value {
    json!({
        "catalog_id": format!("image-{:02}", image.page_index),
        "fingerprint_digest": remote_digest,
        "work_id": image.work_id, "source": image.source, "source_id": image.source_id,
        "source_url": image.source_url, "title": image.title, "page_index": image.page_index,
        "r2_key": image.r2_key, "byte_size": image.byte_size, "content_type": image.content_type,
        "sha256": image.sha256,
    })
}

fn seed_catalog(images: &[CatalogImageRecord]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("hanabi.db");
    let conn = Connection::open(&db).unwrap();
    init_schema(&conn).unwrap();
    for image in images {
        import_catalog_image(&conn, image, &fingerprint(&image.sha256)).unwrap();
    }
    (dir, db)
}

fn mock_sync(
    responses: Vec<(&'static str, &'static str, u16, Value)>,
) -> (String, std::thread::JoinHandle<Vec<Value>>) {
    use std::io::{BufRead, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let mut uploads = Vec::new();
        for (method, after, status, response) in responses {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(socket) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < deadline, "missing mock request");
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut reader = std::io::BufReader::new(&mut socket);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let parts = line.split_whitespace().collect::<Vec<_>>();
            assert_eq!(parts[0], method);
            let url = reqwest::Url::parse(&format!("http://localhost{}", parts[1])).unwrap();
            if method == "GET" {
                assert_eq!(url.path(), "/api/catalog");
                let query = url
                    .query_pairs()
                    .collect::<std::collections::BTreeMap<_, _>>();
                assert_eq!(query.len(), 3);
                assert_eq!(query["after"], after);
                assert_eq!(query["limit"], "100");
                assert_eq!(query["fingerprints"], "1");
            } else {
                assert_eq!(url.path(), "/api/catalog/fingerprints");
            }
            let mut length = 0;
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            if method == "PUT" {
                uploads.push(serde_json::from_slice(&body).unwrap());
            }
            let body = response.to_string();
            write!(socket,"HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
        uploads
    });
    (endpoint, server)
}

#[tokio::test]
async fn sync_uses_keyset_pages_and_uploads_only_changed_or_missing_fingerprints() {
    let images = vec![record(0, "sha-a"), record(1, "sha-b"), record(2, "sha-c")];
    let (_dir, db) = seed_catalog(&images);
    let first_page = |changed| {
        json!({"ok":true,"images":[
        catalog_record(&images[0], Some(digest(&images[0]))),
        catalog_record(&images[1], Some(if changed { "0".repeat(64) } else { digest(&images[1]) })),
    ],"next_cursor":"image-01"})
    };
    let last_page = |missing| {
        json!({"ok":true,"images":[
        catalog_record(&images[2], if missing { None } else { Some(digest(&images[2])) }),
    ],"next_cursor":null})
    };
    let (endpoint, server) = mock_sync(vec![
        ("GET", "", 200, first_page(true)),
        ("GET", "image-01", 200, last_page(true)),
        (
            "PUT",
            "",
            200,
            json!({"ok":true,"upserted":2,"unchanged":0}),
        ),
        ("GET", "", 200, first_page(false)),
        ("GET", "image-01", 200, last_page(false)),
    ]);
    let first = sync_gallery_fingerprints(&db, &endpoint, "fake")
        .await
        .unwrap();
    assert_eq!(
        (first.listed, first.imported, first.unchanged, first.pushed),
        (3, 0, 3, 2)
    );
    let second = sync_gallery_fingerprints(&db, &endpoint, "fake")
        .await
        .unwrap();
    assert_eq!((second.listed, second.imported, second.pushed), (3, 0, 0));
    let uploads = server.join().unwrap();
    assert_eq!(uploads.len(), 1);
    assert_eq!(
        uploads[0]["images"],
        json!([
            fingerprint_upload_json(&images[1].r2_key, &fingerprint("sha-b")),
            fingerprint_upload_json(&images[2].r2_key, &fingerprint("sha-c")),
        ])
    );
}

#[tokio::test]
async fn failed_or_incomplete_upload_ack_is_retried_on_the_next_sync() {
    let image = record(0, "sha-a");
    for (status, ack) in [
        (500, json!({"ok":false})),
        (200, json!({"ok":false,"upserted":1})),
        (200, json!({"ok":true,"upserted":0})),
        (200, json!({"ok":true})),
    ] {
        let (_dir, db) = seed_catalog(std::slice::from_ref(&image));
        let page = json!({"ok":true,"images":[catalog_record(&image,None)],"next_cursor":null});
        let (endpoint, server) = mock_sync(vec![
            ("GET", "", 200, page.clone()),
            ("PUT", "", status, ack),
            ("GET", "", 200, page),
            ("PUT", "", 200, json!({"ok":true,"upserted":1})),
        ]);
        assert_eq!(
            sync_gallery_fingerprints(&db, &endpoint, "fake")
                .await
                .unwrap()
                .pushed,
            0
        );
        assert_eq!(
            sync_gallery_fingerprints(&db, &endpoint, "fake")
                .await
                .unwrap()
                .pushed,
            1
        );
        assert_eq!(server.join().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn sync_rejects_old_catalog_contract_and_nonadvancing_cursors() {
    let image = record(0, "sha-a");
    let (_dir, db) = seed_catalog(std::slice::from_ref(&image));
    let valid = json!({"ok":true,"images":[catalog_record(&image,Some(digest(&image)))],"next_cursor":null});
    let mut missing_digest = valid.clone();
    missing_digest["images"][0]
        .as_object_mut()
        .unwrap()
        .remove("fingerprint_digest");
    let mut missing_cursor = valid.clone();
    missing_cursor
        .as_object_mut()
        .unwrap()
        .remove("next_cursor");
    let mut missing_id = valid.clone();
    missing_id["images"][0]
        .as_object_mut()
        .unwrap()
        .remove("catalog_id");
    let mut stalled = valid;
    stalled["next_cursor"] = json!("");
    for page in [missing_digest, missing_cursor, missing_id, stalled] {
        let (endpoint, server) = mock_sync(vec![("GET", "", 200, page)]);
        assert!(sync_gallery_fingerprints(&db, &endpoint, "fake")
            .await
            .is_err());
        assert!(server.join().unwrap().is_empty());
    }
    let (endpoint, server) = mock_sync(vec![
        (
            "GET",
            "",
            200,
            json!({"ok":true,"images":[catalog_record(&image,Some(digest(&image)))],"next_cursor":"image-00"}),
        ),
        (
            "GET",
            "image-00",
            200,
            json!({"ok":true,"images":[catalog_record(&image,Some(digest(&image)))],"next_cursor":null}),
        ),
    ]);
    assert!(sync_gallery_fingerprints(&db, &endpoint, "fake")
        .await
        .is_err());
    assert!(server.join().unwrap().is_empty());
}
