use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use hanabi::gallery::{GalleryPublication, GalleryPublishState};
use hanabi::gallery_outbox::{GalleryOutbox, GalleryUploadFailure, GalleryUploader, RetryOutcome};
use hanabi::model::{Author, ImageRef, MediaItem, SourceKind};

fn sample_item() -> MediaItem {
    MediaItem {
        source: SourceKind::Douyin,
        source_id: "7671195794388553011".into(),
        author: Author {
            name: "author".into(),
            url: "https://example.test/author".into(),
        },
        title: Some("title".into()),
        url: "https://www.douyin.com/note/7671195794388553011".into(),
        tags: vec!["tag".into()],
        bookmark_count: None,
        is_r18: false,
        pixiv_type: None,
        page_count: 1,
        images: vec![ImageRef {
            url: "https://example.test/image.jpg".into(),
            referer: None,
            fallback_urls: vec![],
        }],
        origin: "repair".into(),
    }
}

struct FakeUploader {
    calls: AtomicUsize,
    fail: bool,
}

#[async_trait]
impl GalleryUploader for FakeUploader {
    async fn ingest_paths(
        &self,
        _item: &MediaItem,
        files: &[PathBuf],
        _publication: Option<&hanabi::gallery::GalleryPublication>,
    ) -> std::result::Result<(), GalleryUploadFailure> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        assert_eq!(files.len(), 1);
        assert!(files[0].is_file());
        if self.fail {
            return Err(GalleryUploadFailure::retryable("temporary R2 failure"));
        }
        Ok(())
    }
}

#[tokio::test]
async fn queue_survives_source_cleanup_and_restart_then_removes_copy_only_after_success() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("hanabi.db");
    let root = temp.path().join("gallery-outbox");
    let source_dir = temp.path().join("pending");
    std::fs::create_dir_all(&source_dir).unwrap();
    let source = source_dir.join("original.jpg");
    std::fs::write(&source, b"original bytes").unwrap();

    let outbox = GalleryOutbox::open(&db, &root).unwrap();
    let queued = outbox
        .enqueue(&sample_item(), std::slice::from_ref(&source), "HTTP 500")
        .unwrap();
    assert_eq!(outbox.pending_count().unwrap(), 1);
    assert_ne!(queued.files[0], source);
    assert!(queued.files[0].starts_with(std::fs::canonicalize(&root).unwrap()));
    assert_eq!(std::fs::read(&queued.files[0]).unwrap(), b"original bytes");

    std::fs::remove_dir_all(&source_dir).unwrap();
    drop(outbox);

    let restarted = GalleryOutbox::open(&db, &root).unwrap();
    let uploader = FakeUploader {
        calls: AtomicUsize::new(0),
        fail: false,
    };
    let outcome = restarted
        .retry_source_now(&uploader, "douyin", "7671195794388553011")
        .await
        .unwrap();
    assert_eq!(outcome, RetryOutcome::Succeeded);
    assert_eq!(uploader.calls.load(Ordering::Relaxed), 1);
    assert_eq!(restarted.pending_count().unwrap(), 0);
    assert!(!queued.files[0].exists());
}

#[tokio::test]
async fn failed_retry_keeps_database_row_and_persistent_copy() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("hanabi.db");
    let root = temp.path().join("gallery-outbox");
    let source = temp.path().join("original.jpg");
    std::fs::write(&source, b"original bytes").unwrap();
    let item = sample_item();
    let outbox = GalleryOutbox::open(&db, &root).unwrap();
    let queued = outbox.enqueue(&item, &[&source], "HTTP 500").unwrap();
    let uploader = FakeUploader {
        calls: AtomicUsize::new(0),
        fail: true,
    };

    let outcome = outbox
        .retry_source_now(&uploader, "douyin", &item.source_id)
        .await
        .unwrap();
    assert_eq!(outcome, RetryOutcome::Failed);
    assert_eq!(outbox.pending_count().unwrap(), 1);
    assert!(queued.files[0].is_file());
}

#[test]
fn startup_recovers_a_manifest_if_database_registration_was_lost() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("hanabi.db");
    let root = temp.path().join("gallery-outbox");
    let source = temp.path().join("original.jpg");
    std::fs::write(&source, b"original bytes").unwrap();
    let item = sample_item();
    let outbox = GalleryOutbox::open(&db_path, &root).unwrap();
    let queued = outbox.enqueue(&item, &[&source], "HTTP 500").unwrap();

    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute("DELETE FROM gallery_outbox", []).unwrap();
    drop(db);
    drop(outbox);

    let recovered = GalleryOutbox::open(&db_path, &root).unwrap();
    assert_eq!(recovered.pending_count().unwrap(), 1);
    assert!(queued.files[0].is_file());
}

#[test]
fn startup_recovers_a_completed_staging_directory_after_interrupted_enqueue() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("hanabi.db");
    let root = temp.path().join("gallery-outbox");
    let source = temp.path().join("original.jpg");
    std::fs::write(&source, b"original bytes").unwrap();
    let item = sample_item();
    let outbox = GalleryOutbox::open(&db_path, &root).unwrap();
    let queued = outbox.enqueue(&item, &[&source], "HTTP 500").unwrap();
    let original_dir = queued.files[0].parent().unwrap();
    let interrupted_dir = root.join(".interrupted-enqueue");

    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute("DELETE FROM gallery_outbox", []).unwrap();
    drop(db);
    std::fs::rename(original_dir, &interrupted_dir).unwrap();
    drop(outbox);

    let recovered = GalleryOutbox::open(&db_path, &root).unwrap();
    assert_eq!(recovered.pending_count().unwrap(), 1);
}

#[test]
fn cancel_retries_stops_queued_gallery_work() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("hanabi.db");
    let root = temp.path().join("gallery-outbox");
    let source = temp.path().join("original.jpg");
    std::fs::write(&source, b"original bytes").unwrap();
    let item = sample_item();
    let outbox = GalleryOutbox::open(&db_path, &root).unwrap();
    outbox
        .enqueue(&item, std::slice::from_ref(&source), "HTTP 500")
        .unwrap();
    assert_eq!(outbox.pending_count().unwrap(), 1);
    assert!(outbox.cancel_retries("douyin", &item.source_id).unwrap());
    assert_eq!(outbox.pending_count().unwrap(), 0);
}

#[test]
fn successful_initial_upload_resolves_the_staged_queue_without_retrying() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("hanabi.db");
    let root = temp.path().join("gallery-outbox");
    let source = temp.path().join("original.jpg");
    std::fs::write(&source, b"original bytes").unwrap();
    let item = sample_item();
    let outbox = GalleryOutbox::open(&db_path, &root).unwrap();
    let queued = outbox.stage(&item, &[&source]).unwrap();

    assert!(outbox
        .resolve_without_retry("douyin", &item.source_id)
        .unwrap());
    assert_eq!(outbox.pending_count().unwrap(), 0);
    assert!(!queued.files[0].exists());
}

#[tokio::test]
async fn prepublish_staging_survives_restart_but_is_not_retryable_until_activated() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("hanabi.db");
    let root = temp.path().join("gallery-outbox");
    let source = temp.path().join("original.jpg");
    std::fs::write(&source, b"original bytes").unwrap();
    let item = sample_item();
    let outbox = GalleryOutbox::open(&db_path, &root).unwrap();
    outbox.stage(&item, &[&source]).unwrap();
    // 模拟 manifest 已落盘、SQLite 登记却因崩溃丢失；恢复后也必须仍是 staging。
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute("DELETE FROM gallery_outbox", []).unwrap();
    drop(db);
    drop(outbox);

    let restarted = GalleryOutbox::open(&db_path, &root).unwrap();
    let uploader = FakeUploader {
        calls: AtomicUsize::new(0),
        fail: false,
    };
    let before_publish = restarted
        .retry_due_at(&uploader, i64::MAX, 10)
        .await
        .unwrap();
    assert_eq!(before_publish.succeeded, 0);
    assert_eq!(uploader.calls.load(Ordering::Relaxed), 0);

    assert!(restarted.activate("douyin", &item.source_id).unwrap());
    let after_publish = restarted
        .retry_due_at(&uploader, i64::MAX, 10)
        .await
        .unwrap();
    assert_eq!(after_publish.succeeded, 1);
    assert_eq!(uploader.calls.load(Ordering::Relaxed), 1);
}

#[test]
fn stale_manifest_temp_file_does_not_block_activation() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("hanabi.db");
    let root = temp.path().join("gallery-outbox");
    let source = temp.path().join("original.jpg");
    std::fs::write(&source, b"original bytes").unwrap();
    let item = sample_item();
    let outbox = GalleryOutbox::open(&db_path, &root).unwrap();
    let queued = outbox.stage(&item, &[&source]).unwrap();
    let work_dir = queued.files[0].parent().unwrap();

    std::fs::write(work_dir.join("manifest.json.tmp"), b"interrupted update").unwrap();

    assert!(outbox.activate("douyin", &item.source_id).unwrap());
}

#[test]
fn enqueueing_an_existing_pending_item_refreshes_failure_and_retry_time() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("hanabi.db");
    let root = temp.path().join("gallery-outbox");
    let source = temp.path().join("original.jpg");
    std::fs::write(&source, b"original bytes").unwrap();
    let item = sample_item();
    let outbox = GalleryOutbox::open(&db_path, &root).unwrap();
    outbox.stage(&item, &[&source]).unwrap();
    assert!(outbox.activate("douyin", &item.source_id).unwrap());

    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute(
        "UPDATE gallery_outbox SET last_error='old', next_attempt_at=0",
        [],
    )
    .unwrap();
    drop(db);

    outbox
        .enqueue(&item, &[&source], "R2 InternalError 10001")
        .unwrap();
    let db = rusqlite::Connection::open(&db_path).unwrap();
    let (last_error, next_attempt_at): (String, i64) = db
        .query_row(
            "SELECT last_error,next_attempt_at FROM gallery_outbox",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(last_error, "R2 InternalError 10001");
    assert!(next_attempt_at > 0);
}

#[tokio::test]
async fn a_second_open_keeps_a_live_uploading_lease_but_recovers_an_expired_one() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("hanabi.db");
    let root = temp.path().join("gallery-outbox");
    let source = temp.path().join("original.jpg");
    std::fs::write(&source, b"original bytes").unwrap();
    let item = sample_item();
    let outbox = GalleryOutbox::open(&db_path, &root).unwrap();
    outbox.enqueue(&item, &[&source], "HTTP 500").unwrap();
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute(
        "UPDATE gallery_outbox
         SET state='uploading', claimed_at=CAST(strftime('%s','now') AS INTEGER)",
        [],
    )
    .unwrap();
    drop(db);

    let second = GalleryOutbox::open(&db_path, &root).unwrap();
    let uploader = FakeUploader {
        calls: AtomicUsize::new(0),
        fail: false,
    };
    assert_eq!(
        second
            .retry_source_now(&uploader, "douyin", &item.source_id)
            .await
            .unwrap(),
        RetryOutcome::Missing
    );
    assert_eq!(uploader.calls.load(Ordering::Relaxed), 0);

    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute("UPDATE gallery_outbox SET claimed_at=1", [])
        .unwrap();
    drop(db);
    let third = GalleryOutbox::open(&db_path, &root).unwrap();
    assert_eq!(
        third
            .retry_source_now(&uploader, "douyin", &item.source_id)
            .await
            .unwrap(),
        RetryOutcome::Succeeded
    );
    assert_eq!(uploader.calls.load(Ordering::Relaxed), 1);
}

#[test]
fn opening_an_old_outbox_table_adds_claimed_at_without_losing_rows() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("hanabi.db");
    let root = temp.path().join("gallery-outbox");
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute_batch(
        "CREATE TABLE gallery_outbox(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source_kind TEXT NOT NULL,
            source_id TEXT NOT NULL,
            item_meta TEXT NOT NULL,
            files TEXT NOT NULL,
            work_dir TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            next_attempt_at INTEGER NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0,
            state TEXT NOT NULL DEFAULT 'pending',
            last_error TEXT NOT NULL DEFAULT '',
            UNIQUE(source_kind,source_id)
         );",
    )
    .unwrap();
    drop(db);

    GalleryOutbox::open(&db_path, &root).unwrap();
    let db = rusqlite::Connection::open(&db_path).unwrap();
    let has_claimed_at: bool = db
        .prepare("SELECT name FROM pragma_table_info('gallery_outbox')")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .flatten()
        .any(|name| name == "claimed_at");
    assert!(has_claimed_at);
}

#[tokio::test]
async fn moved_database_and_outbox_rebase_paths_to_the_new_root() {
    let temp = tempfile::tempdir().unwrap();
    let old_db = temp.path().join("old.db");
    let old_root = temp.path().join("old-outbox");
    let source = temp.path().join("original.jpg");
    std::fs::write(&source, b"original bytes").unwrap();
    let item = sample_item();
    let old = GalleryOutbox::open(&old_db, &old_root).unwrap();
    old.enqueue(&item, &[&source], "HTTP 500").unwrap();
    let db = rusqlite::Connection::open(&old_db).unwrap();
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    drop(db);
    drop(old);

    let new_db = temp.path().join("new.db");
    let new_root = temp.path().join("new-outbox");
    std::fs::copy(&old_db, &new_db).unwrap();
    std::fs::rename(&old_root, &new_root).unwrap();
    assert!(!old_root.exists());

    let moved = GalleryOutbox::open(&new_db, &new_root).unwrap();
    let uploader = FakeUploader {
        calls: AtomicUsize::new(0),
        fail: false,
    };
    assert_eq!(
        moved
            .retry_source_now(&uploader, "douyin", &item.source_id)
            .await
            .unwrap(),
        RetryOutcome::Succeeded
    );
    assert_eq!(uploader.calls.load(Ordering::Relaxed), 1);
}

#[test]
fn completed_row_cannot_delete_a_directory_outside_the_outbox_root() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("hanabi.db");
    let root = temp.path().join("gallery-outbox");
    let outside = temp.path().join("outside-sentinel");
    std::fs::create_dir_all(&outside).unwrap();
    let sentinel = outside.join("keep.txt");
    std::fs::write(&sentinel, b"keep").unwrap();
    let outbox = GalleryOutbox::open(&db_path, &root).unwrap();
    drop(outbox);
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute(
        "INSERT INTO gallery_outbox(
            source_kind,source_id,item_meta,files,work_dir,created_at,
            next_attempt_at,attempts,state,last_error,claimed_at
         ) VALUES('douyin','evil','{}','[]',?1,0,0,0,'completed','',0)",
        [outside.to_string_lossy().as_ref()],
    )
    .unwrap();
    drop(db);

    assert!(GalleryOutbox::open(&db_path, &root).is_err());
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
}

#[cfg(unix)]
#[tokio::test]
async fn claim_rejects_a_symlinked_db_file_without_reading_the_target() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("hanabi.db");
    let root = temp.path().join("gallery-outbox");
    let source = temp.path().join("original.jpg");
    let sentinel = temp.path().join("outside-secret.jpg");
    std::fs::write(&source, b"original bytes").unwrap();
    std::fs::write(&sentinel, b"secret").unwrap();
    let item = sample_item();
    let outbox = GalleryOutbox::open(&db_path, &root).unwrap();
    let queued = outbox.enqueue(&item, &[&source], "HTTP 500").unwrap();
    std::fs::remove_file(&queued.files[0]).unwrap();
    symlink(&sentinel, &queued.files[0]).unwrap();
    let uploader = FakeUploader {
        calls: AtomicUsize::new(0),
        fail: false,
    };

    assert_eq!(
        outbox
            .retry_source_now(&uploader, "douyin", &item.source_id)
            .await
            .unwrap(),
        RetryOutcome::Failed
    );
    assert_eq!(uploader.calls.load(Ordering::Relaxed), 0);
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"secret");
}

#[tokio::test]
async fn claim_rejects_an_outside_db_work_dir_without_deleting_it() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("hanabi.db");
    let root = temp.path().join("gallery-outbox");
    let source = temp.path().join("original.jpg");
    let outside = temp.path().join("outside-sentinel");
    std::fs::create_dir_all(&outside).unwrap();
    let sentinel = outside.join("keep.txt");
    std::fs::write(&source, b"original bytes").unwrap();
    std::fs::write(&sentinel, b"keep").unwrap();
    let item = sample_item();
    let outbox = GalleryOutbox::open(&db_path, &root).unwrap();
    outbox.enqueue(&item, &[&source], "HTTP 500").unwrap();
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute(
        "UPDATE gallery_outbox SET work_dir=?1",
        [outside.to_string_lossy().as_ref()],
    )
    .unwrap();
    drop(db);
    let uploader = FakeUploader {
        calls: AtomicUsize::new(0),
        fail: false,
    };

    assert_eq!(
        outbox
            .retry_source_now(&uploader, "douyin", &item.source_id)
            .await
            .unwrap(),
        RetryOutcome::Failed
    );
    assert_eq!(uploader.calls.load(Ordering::Relaxed), 0);
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
}

#[test]
fn manifest_rejects_absolute_parent_and_multi_component_filenames() {
    for malicious in ["/tmp/outside.jpg", "../outside.jpg", "nested/file.jpg"] {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("hanabi.db");
        let root = temp.path().join("gallery-outbox");
        let source = temp.path().join("original.jpg");
        std::fs::write(&source, b"original bytes").unwrap();
        let item = sample_item();
        let outbox = GalleryOutbox::open(&db_path, &root).unwrap();
        let queued = outbox.stage(&item, &[&source]).unwrap();
        let work_dir = queued.files[0].parent().unwrap();
        let manifest_path = work_dir.join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["files"] = serde_json::json!([malicious]);
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let db = rusqlite::Connection::open(&db_path).unwrap();
        db.execute("DELETE FROM gallery_outbox", []).unwrap();
        drop(db);
        drop(outbox);

        let reopened = GalleryOutbox::open(&db_path, &root).unwrap();
        assert_eq!(reopened.pending_count().unwrap(), 0, "accepted {malicious}");
    }
}

struct PermanentFailureUploader {
    calls: AtomicUsize,
}

#[async_trait]
impl GalleryUploader for PermanentFailureUploader {
    async fn ingest_paths(
        &self,
        _item: &MediaItem,
        _files: &[PathBuf],
        _publication: Option<&hanabi::gallery::GalleryPublication>,
    ) -> std::result::Result<(), GalleryUploadFailure> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Err(GalleryUploadFailure::permanent("HTTP 413"))
    }
}

#[tokio::test]
async fn permanent_upload_failure_becomes_dead_letter_and_is_not_claimed_again() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("hanabi.db");
    let root = temp.path().join("gallery-outbox");
    let source = temp.path().join("original.jpg");
    std::fs::write(&source, b"original bytes").unwrap();
    let item = sample_item();
    let outbox = GalleryOutbox::open(&db_path, &root).unwrap();
    let queued = outbox.enqueue(&item, &[&source], "HTTP 500").unwrap();
    let uploader = PermanentFailureUploader {
        calls: AtomicUsize::new(0),
    };

    assert_eq!(
        outbox
            .retry_source_now(&uploader, "douyin", &item.source_id)
            .await
            .unwrap(),
        RetryOutcome::Failed
    );
    assert_eq!(
        outbox
            .retry_source_now(&uploader, "douyin", &item.source_id)
            .await
            .unwrap(),
        RetryOutcome::Missing
    );
    assert_eq!(uploader.calls.load(Ordering::Relaxed), 1);
    assert!(queued.files[0].is_file());
    let db = rusqlite::Connection::open(&db_path).unwrap();
    let state: String = db
        .query_row("SELECT state FROM gallery_outbox", [], |row| row.get(0))
        .unwrap();
    assert_eq!(state, "failed");
}

struct BlockingSuccessUploader {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    failure: Option<bool>,
}

#[async_trait]
impl GalleryUploader for BlockingSuccessUploader {
    async fn ingest_paths(
        &self,
        _item: &MediaItem,
        _files: &[PathBuf],
        _publication: Option<&hanabi::gallery::GalleryPublication>,
    ) -> std::result::Result<(), GalleryUploadFailure> {
        self.started.notify_one();
        self.release.notified().await;
        match self.failure {
            Some(true) => Err(GalleryUploadFailure::permanent("permanent failure")),
            Some(false) => Err(GalleryUploadFailure::retryable("temporary failure")),
            None => Ok(()),
        }
    }
}

#[tokio::test]
async fn expired_old_worker_cannot_commit_over_a_newer_attempt() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("hanabi.db");
    let root = temp.path().join("gallery-outbox");
    let source = temp.path().join("original.jpg");
    std::fs::write(&source, b"original bytes").unwrap();
    let item = sample_item();
    let outbox = GalleryOutbox::open(&db_path, &root).unwrap();
    let queued = outbox.enqueue(&item, &[&source], "HTTP 500").unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let old_outbox = outbox.clone();
    let old_item_id = item.source_id.clone();
    let old_uploader = BlockingSuccessUploader {
        started: started.clone(),
        release: release.clone(),
        failure: None,
    };
    let old_task = tokio::spawn(async move {
        old_outbox
            .retry_source_now(&old_uploader, "douyin", &old_item_id)
            .await
    });
    started.notified().await;

    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute("UPDATE gallery_outbox SET claimed_at=1", [])
        .unwrap();
    drop(db);
    let newer = GalleryOutbox::open(&db_path, &root).unwrap();
    let newer_started = Arc::new(tokio::sync::Notify::new());
    let newer_release = Arc::new(tokio::sync::Notify::new());
    let newer_uploader = BlockingSuccessUploader {
        started: newer_started.clone(),
        release: newer_release.clone(),
        failure: Some(false),
    };
    let newer_item_id = item.source_id.clone();
    let newer_task = tokio::spawn(async move {
        newer
            .retry_source_now(&newer_uploader, "douyin", &newer_item_id)
            .await
    });
    newer_started.notified().await;

    release.notify_one();
    assert!(old_task.await.unwrap().is_err());
    newer_release.notify_one();
    assert_eq!(newer_task.await.unwrap().unwrap(), RetryOutcome::Failed);
    let db = rusqlite::Connection::open(&db_path).unwrap();
    let (state, attempts): (String, i64) = db
        .query_row("SELECT state,attempts FROM gallery_outbox", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(state, "pending");
    assert_eq!(attempts, 2);
    assert!(queued.files[0].is_file());
}

async fn assert_expired_failure_cannot_commit(permanent: bool) {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("hanabi.db");
    let root = temp.path().join("gallery-outbox");
    let source = temp.path().join("original.jpg");
    std::fs::write(&source, b"original bytes").unwrap();
    let item = sample_item();
    let outbox = GalleryOutbox::open(&db_path, &root).unwrap();
    outbox.enqueue(&item, &[&source], "HTTP 500").unwrap();

    let old_started = Arc::new(tokio::sync::Notify::new());
    let old_release = Arc::new(tokio::sync::Notify::new());
    let old_outbox = outbox.clone();
    let old_item_id = item.source_id.clone();
    let old_uploader = BlockingSuccessUploader {
        started: old_started.clone(),
        release: old_release.clone(),
        failure: Some(permanent),
    };
    let old_task = tokio::spawn(async move {
        old_outbox
            .retry_source_now(&old_uploader, "douyin", &old_item_id)
            .await
    });
    old_started.notified().await;

    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute("UPDATE gallery_outbox SET claimed_at=1", [])
        .unwrap();
    drop(db);
    let newer = GalleryOutbox::open(&db_path, &root).unwrap();
    let newer_started = Arc::new(tokio::sync::Notify::new());
    let newer_release = Arc::new(tokio::sync::Notify::new());
    let newer_uploader = BlockingSuccessUploader {
        started: newer_started.clone(),
        release: newer_release.clone(),
        failure: None,
    };
    let newer_item_id = item.source_id.clone();
    let newer_task = tokio::spawn(async move {
        newer
            .retry_source_now(&newer_uploader, "douyin", &newer_item_id)
            .await
    });
    newer_started.notified().await;

    old_release.notify_one();
    assert!(old_task.await.unwrap().is_err());
    let db = rusqlite::Connection::open(&db_path).unwrap();
    let (state, attempts): (String, i64) = db
        .query_row("SELECT state,attempts FROM gallery_outbox", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(state, "uploading");
    assert_eq!(attempts, 2);
    drop(db);

    newer_release.notify_one();
    assert_eq!(newer_task.await.unwrap().unwrap(), RetryOutcome::Succeeded);
    assert_eq!(outbox.pending_count().unwrap(), 0);
}

#[tokio::test]
async fn expired_old_retryable_failure_cannot_commit_over_a_newer_attempt() {
    assert_expired_failure_cannot_commit(false).await;
}

#[tokio::test]
async fn expired_old_permanent_failure_cannot_dead_letter_a_newer_attempt() {
    assert_expired_failure_cannot_commit(true).await;
}

async fn assert_malformed_row_does_not_starve_following_work(column: &str, malformed: &str) {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("hanabi.db");
    let root = temp.path().join("gallery-outbox");
    let source = temp.path().join("original.jpg");
    std::fs::write(&source, b"original bytes").unwrap();
    let first = sample_item();
    let mut second = sample_item();
    second.source_id = "7671195794388553012".into();
    let outbox = GalleryOutbox::open(&db_path, &root).unwrap();
    outbox.enqueue(&first, &[&source], "HTTP 500").unwrap();
    outbox.enqueue(&second, &[&source], "HTTP 500").unwrap();

    let db = rusqlite::Connection::open(&db_path).unwrap();
    let sql = format!("UPDATE gallery_outbox SET {column}=?1 WHERE source_id=?2");
    db.execute(&sql, rusqlite::params![malformed, first.source_id])
        .unwrap();
    drop(db);

    let uploader = FakeUploader {
        calls: AtomicUsize::new(0),
        fail: false,
    };
    let summary = outbox.retry_due_at(&uploader, i64::MAX, 10).await.unwrap();
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.succeeded, 1);
    assert_eq!(uploader.calls.load(Ordering::Relaxed), 1);
    let db = rusqlite::Connection::open(&db_path).unwrap();
    let (state, last_error): (String, String) = db
        .query_row(
            "SELECT state,last_error FROM gallery_outbox WHERE source_id=?1",
            [&first.source_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "failed");
    assert!(!last_error.is_empty());
}

#[tokio::test]
async fn malformed_files_json_is_dead_lettered_without_starving_later_work() {
    assert_malformed_row_does_not_starve_following_work("files", "{bad-json").await;
}

#[tokio::test]
async fn malformed_item_meta_is_dead_lettered_without_starving_later_work() {
    assert_malformed_row_does_not_starve_following_work("item_meta", "{bad-json").await;
}

fn pixiv_item() -> MediaItem {
    MediaItem {
        source: SourceKind::Pixiv,
        source_id: "1".into(),
        author: Author {
            name: "author".into(),
            url: "https://example.test/author".into(),
        },
        title: Some("title".into()),
        url: "https://www.pixiv.net/artworks/1".into(),
        tags: vec!["tag".into()],
        bookmark_count: None,
        is_r18: false,
        pixiv_type: None,
        page_count: 1,
        images: vec![ImageRef {
            url: "https://example.test/image.jpg".into(),
            referer: None,
            fallback_urls: vec![],
        }],
        origin: "test".into(),
    }
}

#[test]
fn queued_work_preserves_publication_after_restart() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("hanabi.db");
    let root = temp.path().join("gallery-outbox");
    let source = temp.path().join("original.jpg");
    std::fs::write(&source, b"original bytes").unwrap();
    let publication = GalleryPublication {
        chat_id: -100123,
        message_ids: vec![41, 42],
        publish_state: GalleryPublishState::Full,
    };
    let outbox = GalleryOutbox::open(&db_path, &root).unwrap();
    outbox
        .enqueue_with_publication(
            &pixiv_item(),
            &[&source],
            "HTTP 500",
            Some(publication.clone()),
        )
        .unwrap();
    drop(outbox);
    let reopened = GalleryOutbox::open(&db_path, &root).unwrap();
    assert_eq!(
        reopened
            .query_queued("pixiv", "1")
            .unwrap()
            .unwrap()
            .publication,
        Some(publication)
    );
}

#[test]
fn opening_pre_publication_outbox_defaults_to_none_without_row_loss() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("hanabi.db");
    let root = temp.path().join("gallery-outbox");
    std::fs::create_dir_all(&root).unwrap();
    let root = std::fs::canonicalize(&root).unwrap();
    let work_dir = root.join("pixiv_1");
    std::fs::create_dir_all(&work_dir).unwrap();
    let file = work_dir.join("000.jpg");
    std::fs::write(&file, b"bytes").unwrap();
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute_batch(
        "CREATE TABLE gallery_outbox(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source_kind TEXT NOT NULL,
            source_id TEXT NOT NULL,
            item_meta TEXT NOT NULL,
            files TEXT NOT NULL,
            work_dir TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            next_attempt_at INTEGER NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0,
            state TEXT NOT NULL DEFAULT 'pending',
            last_error TEXT NOT NULL DEFAULT '',
            UNIQUE(source_kind,source_id)
         );",
    )
    .unwrap();
    db.execute(
        "INSERT INTO gallery_outbox(
            source_kind,source_id,item_meta,files,work_dir,created_at,next_attempt_at,attempts,state,last_error
         ) VALUES('pixiv','1','{}',?1,?2,1,1,0,'pending','')",
        rusqlite::params![
            serde_json::to_string(&vec![file.to_string_lossy()]).unwrap(),
            work_dir.to_string_lossy()
        ],
    )
    .unwrap();
    drop(db);

    let outbox = GalleryOutbox::open(&db_path, &root).unwrap();
    let queued = outbox.query_queued("pixiv", "1").unwrap().unwrap();
    assert_eq!(queued.publication, None);
    assert_eq!(outbox.pending_count().unwrap(), 1);
}
