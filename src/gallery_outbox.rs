//! 频道发布后图库入库失败的持久补偿队列。
//!
//! 队列只重试 Vitrine，不调用 Telegram，也不读写 `pushed`。入队时先把图片复制到
//! 独立目录并写 manifest，再用同一个 `hanabi.db` 事务登记；若进程恰在两步之间
//! 退出，下次打开队列会从 manifest 恢复数据库行。

use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::gallery::{GalleryClient, GalleryIngestError, GalleryPublication};
use crate::model::MediaItem;

const RETRY_BASE_SECS: i64 = 5 * 60;
const RETRY_MAX_SECS: i64 = 6 * 3600;
const WORKER_INTERVAL_SECS: u64 = 60;
const UPLOAD_LEASE_SECS: i64 = 15 * 60;
const MANIFEST_NAME: &str = "manifest.json";

fn pending_state() -> String {
    "pending".to_string()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn component(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .take(120)
        .collect()
}

fn error_summary(error: &str) -> String {
    error.chars().take(2000).collect()
}

fn retry_delay(attempts: i64) -> i64 {
    let shift = attempts.saturating_sub(1).min(6) as u32;
    (RETRY_BASE_SECS.saturating_mul(1_i64 << shift)).min(RETRY_MAX_SECS)
}

fn open_connection(path: &Path) -> Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(path).context("打开 gallery outbox 数据库失败")?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
    Ok(conn)
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    item: MediaItem,
    files: Vec<String>,
    created_at: i64,
    last_error: String,
    #[serde(default = "pending_state")]
    state: String,
    #[serde(default)]
    publication: Option<GalleryPublication>,
}

#[derive(Debug, Clone)]
pub struct QueuedGalleryWork {
    pub source_kind: String,
    pub source_id: String,
    pub files: Vec<PathBuf>,
    pub publication: Option<GalleryPublication>,
    state: String,
    work_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryOutcome {
    Succeeded,
    Failed,
    Missing,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RetrySummary {
    pub succeeded: usize,
    pub failed: usize,
}

#[derive(Debug, Clone)]
pub struct GalleryUploadFailure {
    message: String,
    retryable: bool,
}

impl GalleryUploadFailure {
    pub fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
        }
    }

    pub fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }
}

impl std::fmt::Display for GalleryUploadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[async_trait]
pub trait GalleryUploader: Send + Sync {
    async fn ingest_paths(
        &self,
        item: &MediaItem,
        files: &[PathBuf],
        publication: Option<&GalleryPublication>,
    ) -> std::result::Result<(), GalleryUploadFailure>;
}

#[async_trait]
impl GalleryUploader for GalleryClient {
    async fn ingest_paths(
        &self,
        item: &MediaItem,
        files: &[PathBuf],
        publication: Option<&GalleryPublication>,
    ) -> std::result::Result<(), GalleryUploadFailure> {
        self.ingest(item, files, publication)
            .await
            .map_err(|error: GalleryIngestError| {
                if error.is_retryable() {
                    GalleryUploadFailure::retryable(error.to_string())
                } else {
                    GalleryUploadFailure::permanent(error.to_string())
                }
            })
    }
}

#[derive(Debug, Clone)]
pub struct GalleryOutbox {
    db_path: PathBuf,
    root: PathBuf,
}

struct ClaimedWork {
    id: i64,
    item: MediaItem,
    files: Vec<PathBuf>,
    work_dir: PathBuf,
    attempts: i64,
    publication: Option<GalleryPublication>,
}

enum ClaimResult {
    Claimed(Box<ClaimedWork>),
    Rejected,
    Missing,
}

impl GalleryOutbox {
    pub fn open(db_path: impl AsRef<Path>, root: impl AsRef<Path>) -> Result<Self> {
        let db_path = absolute(db_path.as_ref())?;
        let root = absolute(root.as_ref())?;
        std::fs::create_dir_all(&root).context("创建 gallery outbox 目录失败")?;
        crate::util::restrict_dir(&root);
        let root = std::fs::canonicalize(&root).context("解析 gallery outbox 根目录失败")?;
        let this = Self { db_path, root };
        let conn = open_connection(&this.db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS gallery_outbox(
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                source_kind     TEXT NOT NULL,
                source_id       TEXT NOT NULL,
                item_meta       TEXT NOT NULL,
                files           TEXT NOT NULL,
                work_dir        TEXT NOT NULL,
                created_at      INTEGER NOT NULL,
                next_attempt_at INTEGER NOT NULL,
                attempts        INTEGER NOT NULL DEFAULT 0,
                state           TEXT NOT NULL DEFAULT 'pending',
                last_error      TEXT NOT NULL DEFAULT '',
                claimed_at      INTEGER NOT NULL DEFAULT 0,
                UNIQUE(source_kind, source_id)
             );",
        )
        .context("初始化 gallery outbox 表失败")?;
        // 兼容已创建但没有 lease 字段的旧 outbox 表。
        let _ = conn.execute(
            "ALTER TABLE gallery_outbox ADD COLUMN claimed_at INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE gallery_outbox ADD COLUMN publication_json TEXT NOT NULL DEFAULT 'null'",
            [],
        );
        let lease_cutoff = now_secs() - UPLOAD_LEASE_SECS;
        conn.execute(
            "UPDATE gallery_outbox
             SET state='pending',next_attempt_at=?1,claimed_at=0
             WHERE state='uploading' AND claimed_at<=?2",
            rusqlite::params![now_secs(), lease_cutoff],
        )?;
        this.recover_manifests(&conn)?;
        this.cleanup_completed(&conn)?;
        let staging: i64 = conn.query_row(
            "SELECT COUNT(*) FROM gallery_outbox WHERE state='staging'",
            [],
            |row| row.get(0),
        )?;
        if staging > 0 {
            tracing::warn!(
                count = staging,
                "发现未确认频道发布的 gallery outbox staging；保留供人工诊断，不会自动入库"
            );
        }
        Ok(this)
    }

    pub fn for_database(db_path: impl AsRef<Path>) -> Result<Self> {
        let db_path = absolute(db_path.as_ref())?;
        let root = std::env::var_os("HANABI_GALLERY_OUTBOX_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                db_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join("gallery-outbox")
            });
        Self::open(db_path, root)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn pending_count(&self) -> Result<i64> {
        let conn = open_connection(&self.db_path)?;
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM gallery_outbox WHERE state <> 'completed'",
            [],
            |row| row.get(0),
        )?)
    }

    fn validate_work_dir(&self, dir: &Path) -> Result<PathBuf> {
        if !dir.is_absolute() || dir.parent() != Some(self.root.as_path()) {
            anyhow::bail!(
                "gallery outbox work_dir 不在根目录直属子目录: {}",
                dir.display()
            );
        }
        let name = dir
            .file_name()
            .context("gallery outbox work_dir 缺少目录名")?;
        if Path::new(name).components().count() != 1 {
            anyhow::bail!("gallery outbox work_dir 目录名非法");
        }
        let metadata = std::fs::symlink_metadata(dir)
            .with_context(|| format!("读取 gallery outbox work_dir 失败: {}", dir.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            anyhow::bail!("gallery outbox work_dir 不是普通目录: {}", dir.display());
        }
        let canonical = std::fs::canonicalize(dir)?;
        if canonical.parent() != Some(self.root.as_path()) {
            anyhow::bail!("gallery outbox work_dir canonical 路径越界");
        }
        Ok(canonical)
    }

    fn validate_file(&self, work_dir: &Path, file: &Path) -> Result<PathBuf> {
        if !file.is_absolute() || file.parent() != Some(work_dir) {
            anyhow::bail!(
                "gallery outbox 文件不在 work_dir 直属目录: {}",
                file.display()
            );
        }
        let name = file.file_name().context("gallery outbox 文件缺少文件名")?;
        if Path::new(name).components().count() != 1 {
            anyhow::bail!("gallery outbox 文件名非法");
        }
        let metadata = std::fs::symlink_metadata(file)
            .with_context(|| format!("读取 gallery outbox 文件失败: {}", file.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            anyhow::bail!("gallery outbox 文件不是普通文件: {}", file.display());
        }
        let canonical = std::fs::canonicalize(file)?;
        if canonical.parent() != Some(work_dir) {
            anyhow::bail!("gallery outbox 文件 canonical 路径越界");
        }
        Ok(canonical)
    }

    fn validate_queued(&self, mut queued: QueuedGalleryWork) -> Result<QueuedGalleryWork> {
        let work_dir = self.validate_work_dir(&queued.work_dir)?;
        if queued.files.is_empty() {
            anyhow::bail!("gallery outbox DB files 为空");
        }
        queued.files = queued
            .files
            .iter()
            .map(|file| self.validate_file(&work_dir, file))
            .collect::<Result<_>>()?;
        queued.work_dir = work_dir;
        Ok(queued)
    }

    pub fn query_queued(
        &self,
        source_kind: &str,
        source_id: &str,
    ) -> Result<Option<QueuedGalleryWork>> {
        let conn = open_connection(&self.db_path)?;
        self.query_queued_on(&conn, source_kind, source_id)
    }

    fn query_queued_on(
        &self,
        conn: &rusqlite::Connection,
        source_kind: &str,
        source_id: &str,
    ) -> Result<Option<QueuedGalleryWork>> {
        query_queued_raw(conn, source_kind, source_id)?
            .map(|queued| self.validate_queued(queued))
            .transpose()
    }

    /// 初次入库已经成功时移除预先 staging 的队列项及副本；不会调用上传器。
    pub fn resolve_without_retry(&self, source_kind: &str, source_id: &str) -> Result<bool> {
        let conn = open_connection(&self.db_path)?;
        let row: Option<(i64, String)> = conn
            .query_row(
                "SELECT id,work_dir FROM gallery_outbox
                 WHERE source_kind=?1 AND source_id=?2 AND state IN ('pending','staging')",
                rusqlite::params![source_kind, source_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((id, work_dir)) = row else {
            return Ok(false);
        };
        self.validate_work_dir(Path::new(&work_dir))?;
        let changed = conn.execute(
            "UPDATE gallery_outbox SET state='completed',claimed_at=0
             WHERE id=?1 AND state IN ('pending','staging')",
            [id],
        )?;
        if changed != 1 {
            return Ok(false);
        }
        self.remove_completed(&conn, id, Path::new(&work_dir))?;
        Ok(true)
    }

    pub fn enqueue(
        &self,
        item: &MediaItem,
        files: &[impl AsRef<Path>],
        initial_error: &str,
    ) -> Result<QueuedGalleryWork> {
        self.enqueue_with_publication(item, files, initial_error, None)
    }

    pub fn enqueue_with_publication(
        &self,
        item: &MediaItem,
        files: &[impl AsRef<Path>],
        initial_error: &str,
        publication: Option<GalleryPublication>,
    ) -> Result<QueuedGalleryWork> {
        self.store(item, files, "pending", initial_error, publication.as_ref())
    }

    pub fn enqueue_classified(
        &self,
        item: &MediaItem,
        files: &[impl AsRef<Path>],
        error: &str,
        retryable: bool,
        publication: Option<GalleryPublication>,
    ) -> Result<QueuedGalleryWork> {
        let queued = self.store(item, files, "pending", error, publication.as_ref())?;
        if retryable {
            return Ok(queued);
        }
        self.update_manifest(&queued.work_dir, |manifest| {
            manifest.state = "failed".to_string();
            manifest.last_error = error_summary(error);
        })?;
        let conn = open_connection(&self.db_path)?;
        conn.execute(
            "UPDATE gallery_outbox
             SET state='failed',last_error=?1,claimed_at=0
             WHERE source_kind=?2 AND source_id=?3 AND state='pending'",
            rusqlite::params![error_summary(error), item.source.as_str(), item.source_id],
        )?;
        Ok(queued)
    }

    /// 仅预复制图片；在 Telegram 确认至少发出一条消息前，worker 永远不能 claim。
    pub fn stage(&self, item: &MediaItem, files: &[impl AsRef<Path>]) -> Result<QueuedGalleryWork> {
        self.store(item, files, "staging", "awaiting Telegram publish", None)
    }

    /// Persist a captured Telegram mapping onto an existing queued work.
    pub fn set_publication(
        &self,
        source_kind: &str,
        source_id: &str,
        publication: &GalleryPublication,
    ) -> Result<bool> {
        let conn = open_connection(&self.db_path)?;
        let Some(existing) = self.query_queued_on(&conn, source_kind, source_id)? else {
            return Ok(false);
        };
        self.update_manifest(&existing.work_dir, |manifest| {
            manifest.publication = Some(publication.clone());
        })?;
        let changed = conn.execute(
            "UPDATE gallery_outbox SET publication_json=?1
             WHERE source_kind=?2 AND source_id=?3 AND state <> 'completed'",
            rusqlite::params![serde_json::to_string(publication)?, source_kind, source_id],
        )?;
        Ok(changed == 1)
    }

    /// Telegram 已确认存在频道消息后，把预复制记录激活为可重试 pending。
    pub fn activate(&self, source_kind: &str, source_id: &str) -> Result<bool> {
        let conn = open_connection(&self.db_path)?;
        let existing = self.query_queued_on(&conn, source_kind, source_id)?;
        let Some(existing) = existing else {
            return Ok(false);
        };
        if existing.state != "staging" {
            return Ok(false);
        }
        self.update_manifest(&existing.work_dir, |manifest| {
            manifest.state = "pending".to_string();
        })?;
        let changed = conn.execute(
            "UPDATE gallery_outbox SET state='pending', next_attempt_at=?1
             WHERE source_kind=?2 AND source_id=?3 AND state='staging'",
            rusqlite::params![now_secs() + RETRY_BASE_SECS, source_kind, source_id],
        )?;
        Ok(changed == 1)
    }

    fn store(
        &self,
        item: &MediaItem,
        files: &[impl AsRef<Path>],
        desired_state: &str,
        initial_error: &str,
        publication: Option<&GalleryPublication>,
    ) -> Result<QueuedGalleryWork> {
        if files.is_empty() {
            anyhow::bail!("无文件可加入 gallery outbox");
        }
        let source_kind = item.source.as_str();
        let dir_name = format!("{}_{}", component(source_kind), component(&item.source_id));
        let final_dir = self.root.join(&dir_name);
        let conn = open_connection(&self.db_path)?;
        if let Some(mut existing) = self.query_queued_on(&conn, source_kind, &item.source_id)? {
            if desired_state == "staging" && existing.state != "staging" {
                anyhow::bail!("该作品已有激活的 gallery outbox 任务");
            }
            if desired_state == "pending" {
                self.update_manifest(&existing.work_dir, |manifest| {
                    manifest.state = "pending".to_string();
                    manifest.last_error = error_summary(initial_error);
                    if publication.is_some() {
                        manifest.publication = publication.cloned();
                    }
                })?;
                if let Some(publication) = publication {
                    let publication_json = serde_json::to_string(publication)?;
                    conn.execute(
                    "UPDATE gallery_outbox
                     SET state='pending',last_error=?1,next_attempt_at=?2,publication_json=?3
                     WHERE source_kind=?4 AND source_id=?5 AND state IN ('pending','staging','failed')",
                    rusqlite::params![
                        error_summary(initial_error),
                        now_secs() + RETRY_BASE_SECS,
                        publication_json,
                        source_kind,
                        item.source_id,
                    ],
                    )?;
                    existing.publication = Some(publication.clone());
                } else {
                    conn.execute(
                    "UPDATE gallery_outbox
                     SET state='pending',last_error=?1,next_attempt_at=?2
                     WHERE source_kind=?3 AND source_id=?4 AND state IN ('pending','staging','failed')",
                    rusqlite::params![
                        error_summary(initial_error),
                        now_secs() + RETRY_BASE_SECS,
                        source_kind,
                        item.source_id,
                    ],
                    )?;
                }
            }
            return Ok(existing);
        }

        if std::fs::symlink_metadata(&final_dir).is_err() {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let staging = self
                .root
                .join(format!(".{dir_name}.{}.{}", std::process::id(), unique));
            std::fs::create_dir(&staging).context("创建 gallery outbox 临时目录失败")?;
            crate::util::restrict_dir(&staging);
            let copy_result: Result<Vec<String>> = files
                .iter()
                .enumerate()
                .map(|(index, source)| {
                    let source = source.as_ref();
                    if !source.is_file() {
                        anyhow::bail!("gallery outbox 源文件不存在: {}", source.display());
                    }
                    let extension = source
                        .extension()
                        .and_then(|part| part.to_str())
                        .filter(|part| part.bytes().all(|b| b.is_ascii_alphanumeric()))
                        .unwrap_or("jpg");
                    let name = format!("{index:03}.{extension}");
                    std::fs::copy(source, staging.join(&name)).with_context(|| {
                        format!("复制 gallery outbox 文件失败: {}", source.display())
                    })?;
                    Ok(name)
                })
                .collect();
            let names = match copy_result {
                Ok(names) => names,
                Err(error) => {
                    let _ = self.remove_private_dir(&staging);
                    return Err(error);
                }
            };
            let manifest = Manifest {
                item: item.clone(),
                files: names,
                created_at: now_secs(),
                last_error: error_summary(initial_error),
                state: desired_state.to_string(),
                publication: publication.cloned(),
            };
            std::fs::write(staging.join(MANIFEST_NAME), serde_json::to_vec(&manifest)?)
                .context("写 gallery outbox manifest 失败")?;
            match std::fs::rename(&staging, &final_dir) {
                Ok(()) => {}
                Err(error) if final_dir.exists() => {
                    let _ = self.remove_private_dir(&staging);
                    tracing::debug!(error = %error, "gallery outbox 目录已由并发任务创建");
                }
                Err(error) => {
                    let _ = self.remove_private_dir(&staging);
                    return Err(error).context("提交 gallery outbox 目录失败");
                }
            }
        }

        self.validate_work_dir(&final_dir)?;

        self.register_manifest(&conn, &final_dir)?;
        let queued = self
            .query_queued_on(&conn, source_kind, &item.source_id)?
            .context("gallery outbox manifest 已落盘但数据库登记未找到")?;
        if desired_state == "staging" && queued.state != "staging" {
            anyhow::bail!("该作品已有激活的 gallery outbox 任务");
        }
        Ok(queued)
    }

    pub async fn retry_source_now<U: GalleryUploader + ?Sized>(
        &self,
        uploader: &U,
        source_kind: &str,
        source_id: &str,
    ) -> Result<RetryOutcome> {
        match self.claim_source(source_kind, source_id)? {
            ClaimResult::Claimed(work) => self.run_claimed(uploader, *work).await,
            ClaimResult::Rejected => Ok(RetryOutcome::Failed),
            ClaimResult::Missing => Ok(RetryOutcome::Missing),
        }
    }

    pub async fn retry_due_at<U: GalleryUploader + ?Sized>(
        &self,
        uploader: &U,
        current_time: i64,
        limit: usize,
    ) -> Result<RetrySummary> {
        let mut summary = RetrySummary::default();
        for _ in 0..limit {
            let outcome = match self.claim_due(current_time)? {
                ClaimResult::Claimed(work) => self.run_claimed(uploader, *work).await?,
                ClaimResult::Rejected => RetryOutcome::Failed,
                ClaimResult::Missing => break,
            };
            match outcome {
                RetryOutcome::Succeeded => summary.succeeded += 1,
                RetryOutcome::Failed => summary.failed += 1,
                RetryOutcome::Missing => {}
            }
        }
        Ok(summary)
    }

    async fn run_claimed<U: GalleryUploader + ?Sized>(
        &self,
        uploader: &U,
        work: ClaimedWork,
    ) -> Result<RetryOutcome> {
        let work_dir = self.validate_work_dir(&work.work_dir);
        let files = work_dir.and_then(|work_dir| {
            work.files
                .iter()
                .map(|file| self.validate_file(&work_dir, file))
                .collect::<Result<Vec<_>>>()
        });
        let result = match files {
            Ok(files) => {
                uploader
                    .ingest_paths(&work.item, &files, work.publication.as_ref())
                    .await
            }
            Err(error) => Err(GalleryUploadFailure::permanent(error.to_string())),
        };
        match result {
            Ok(()) => {
                self.complete_success(work.id, work.attempts)?;
                Ok(RetryOutcome::Succeeded)
            }
            Err(error) => {
                if error.retryable {
                    self.complete_failure(work.id, work.attempts, &error.to_string())?;
                    tracing::warn!(
                        source = work.item.source.as_str(),
                        id = %work.item.source_id,
                        error = %error,
                        "图库补偿重试失败,队列与副本已保留"
                    );
                } else {
                    self.complete_dead_letter(work.id, work.attempts, &error.to_string())?;
                    tracing::error!(
                        source = work.item.source.as_str(),
                        id = %work.item.source_id,
                        error = %error,
                        "图库补偿永久失败,已转 dead-letter 并保留副本"
                    );
                }
                Ok(RetryOutcome::Failed)
            }
        }
    }

    fn claim_source(&self, source_kind: &str, source_id: &str) -> Result<ClaimResult> {
        let mut conn = open_connection(&self.db_path)?;
        let tx = conn.transaction()?;
        let id: Option<i64> = tx
            .query_row(
                "SELECT id FROM gallery_outbox
                 WHERE source_kind=?1 AND source_id=?2 AND state='pending'",
                rusqlite::params![source_kind, source_id],
                |row| row.get(0),
            )
            .optional()?;
        self.claim_id(tx, id, now_secs())
    }

    fn claim_due(&self, current_time: i64) -> Result<ClaimResult> {
        let mut conn = open_connection(&self.db_path)?;
        let tx = conn.transaction()?;
        let id: Option<i64> = tx
            .query_row(
                "SELECT id FROM gallery_outbox
                 WHERE state='pending' AND next_attempt_at<=?1
                 ORDER BY next_attempt_at,id LIMIT 1",
                [current_time],
                |row| row.get(0),
            )
            .optional()?;
        self.claim_id(tx, id, now_secs())
    }

    fn claim_id(
        &self,
        tx: Transaction<'_>,
        id: Option<i64>,
        claimed_at: i64,
    ) -> Result<ClaimResult> {
        let Some(id) = id else {
            tx.commit()?;
            return Ok(ClaimResult::Missing);
        };
        let (item_meta, files_json, attempts, work_dir, publication_json): (
            String,
            String,
            i64,
            String,
            String,
        ) = tx.query_row(
            "SELECT item_meta,files,attempts,work_dir,publication_json FROM gallery_outbox WHERE id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )?;
        let file_names = match serde_json::from_str::<Vec<String>>(&files_json) {
            Ok(files) => files,
            Err(error) => {
                return Self::reject_claim(
                    tx,
                    id,
                    &format!("解析 gallery outbox files 失败: {error}"),
                );
            }
        };
        let files: Vec<PathBuf> = file_names.into_iter().map(PathBuf::from).collect();
        let item: MediaItem = match serde_json::from_str(&item_meta) {
            Ok(item) => item,
            Err(error) => {
                return Self::reject_claim(
                    tx,
                    id,
                    &format!("解析 gallery outbox item_meta 失败: {error}"),
                );
            }
        };
        let publication = match parse_publication_json(&publication_json) {
            Ok(publication) => publication,
            Err(error) => {
                return Self::reject_claim(
                    tx,
                    id,
                    &format!("解析 gallery outbox publication_json 失败: {error}"),
                );
            }
        };
        let queued = QueuedGalleryWork {
            source_kind: item.source.as_str().to_string(),
            source_id: item.source_id.clone(),
            files: files.clone(),
            publication: publication.clone(),
            state: "pending".to_string(),
            work_dir: PathBuf::from(&work_dir),
        };
        if let Err(error) = self.validate_queued(queued) {
            return Self::reject_claim(tx, id, &error.to_string());
        }
        let changed = tx.execute(
            "UPDATE gallery_outbox
             SET state='uploading',attempts=attempts+1,claimed_at=?1
             WHERE id=?2 AND state='pending'",
            rusqlite::params![claimed_at, id],
        )?;
        if changed != 1 {
            tx.commit()?;
            return Ok(ClaimResult::Missing);
        }
        tx.commit()?;
        Ok(ClaimResult::Claimed(Box::new(ClaimedWork {
            id,
            item,
            files,
            work_dir: PathBuf::from(work_dir),
            attempts: attempts + 1,
            publication,
        })))
    }

    fn reject_claim(tx: Transaction<'_>, id: i64, error: &str) -> Result<ClaimResult> {
        let changed = tx.execute(
            "UPDATE gallery_outbox
             SET state='failed',last_error=?1,claimed_at=0
             WHERE id=?2 AND state='pending'",
            rusqlite::params![error_summary(error), id],
        )?;
        tx.commit()?;
        if changed != 1 {
            return Ok(ClaimResult::Missing);
        }
        tracing::error!(id, error, "图库补偿任务校验失败,已转 dead-letter");
        Ok(ClaimResult::Rejected)
    }

    fn complete_failure(&self, id: i64, attempts: i64, error: &str) -> Result<()> {
        let mut conn = open_connection(&self.db_path)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let work_dir: Option<String> = tx
            .query_row(
                "SELECT work_dir FROM gallery_outbox
                 WHERE id=?1 AND state='uploading' AND attempts=?2",
                rusqlite::params![id, attempts],
                |row| row.get(0),
            )
            .optional()?;
        let Some(work_dir) = work_dir else {
            anyhow::bail!("gallery outbox 失败提交已过期");
        };
        self.update_manifest(Path::new(&work_dir), |manifest| {
            manifest.state = "pending".to_string();
            manifest.last_error = error_summary(error);
        })?;
        let changed = tx.execute(
            "UPDATE gallery_outbox
             SET state='pending', next_attempt_at=?1, last_error=?2
                 ,claimed_at=0
             WHERE id=?3 AND state='uploading' AND attempts=?4",
            rusqlite::params![
                now_secs() + retry_delay(attempts),
                error_summary(error),
                id,
                attempts
            ],
        )?;
        if changed != 1 {
            anyhow::bail!("gallery outbox 失败提交已过期");
        }
        tx.commit()?;
        Ok(())
    }

    fn complete_dead_letter(&self, id: i64, attempts: i64, error: &str) -> Result<()> {
        let mut conn = open_connection(&self.db_path)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let work_dir: Option<String> = tx
            .query_row(
                "SELECT work_dir FROM gallery_outbox
                 WHERE id=?1 AND state='uploading' AND attempts=?2",
                rusqlite::params![id, attempts],
                |row| row.get(0),
            )
            .optional()?;
        let Some(work_dir) = work_dir else {
            anyhow::bail!("gallery outbox dead-letter 提交已过期");
        };
        self.update_manifest(Path::new(&work_dir), |manifest| {
            manifest.state = "failed".to_string();
            manifest.last_error = error_summary(error);
        })?;
        let changed = tx.execute(
            "UPDATE gallery_outbox
             SET state='failed',last_error=?1,claimed_at=0
             WHERE id=?2 AND state='uploading' AND attempts=?3",
            rusqlite::params![error_summary(error), id, attempts],
        )?;
        if changed != 1 {
            anyhow::bail!("gallery outbox dead-letter 提交已过期");
        }
        tx.commit()?;
        Ok(())
    }

    fn complete_success(&self, id: i64, attempts: i64) -> Result<()> {
        let mut conn = open_connection(&self.db_path)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let work_dir: Option<String> = tx
            .query_row(
                "SELECT work_dir FROM gallery_outbox
                 WHERE id=?1 AND state='uploading' AND attempts=?2",
                rusqlite::params![id, attempts],
                |row| row.get(0),
            )
            .optional()?;
        let Some(work_dir) = work_dir else {
            anyhow::bail!("gallery outbox 成功状态提交失败");
        };
        self.validate_work_dir(Path::new(&work_dir))?;
        let changed = tx.execute(
            "UPDATE gallery_outbox SET state='completed',claimed_at=0
             WHERE id=?1 AND state='uploading' AND attempts=?2",
            rusqlite::params![id, attempts],
        )?;
        if changed != 1 {
            anyhow::bail!("gallery outbox 成功状态提交失败");
        }
        tx.commit()?;
        self.remove_completed(&conn, id, Path::new(&work_dir))?;
        Ok(())
    }

    fn cleanup_completed(&self, conn: &rusqlite::Connection) -> Result<()> {
        let rows: Vec<(i64, String)> = {
            let mut stmt = conn.prepare(
                "SELECT id,work_dir FROM gallery_outbox WHERE state='completed' ORDER BY id",
            )?;
            let rows = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            rows
        };
        for (id, dir) in rows {
            self.remove_completed(conn, id, Path::new(&dir))?;
        }
        Ok(())
    }

    fn recover_manifests(&self, conn: &rusqlite::Connection) -> Result<()> {
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                let manifest_path = path.join(MANIFEST_NAME);
                let has_regular_manifest = std::fs::symlink_metadata(&manifest_path)
                    .map(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
                    .unwrap_or(false);
                if !has_regular_manifest {
                    continue;
                }
                if let Err(error) = self.register_manifest(conn, &path) {
                    tracing::warn!(dir = %path.display(), error = %error, "恢复 gallery outbox manifest 失败");
                }
            }
        }
        Ok(())
    }

    fn register_manifest(&self, conn: &rusqlite::Connection, dir: &Path) -> Result<()> {
        let dir = self.validate_work_dir(dir)?;
        let manifest_path = dir.join(MANIFEST_NAME);
        self.validate_file(&dir, &manifest_path)?;
        let manifest: Manifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).context("读取 manifest 失败")?)
                .context("解析 manifest 失败")?;
        let files: Vec<PathBuf> = manifest
            .files
            .iter()
            .map(|name| {
                let relative = Path::new(name);
                if relative.is_absolute()
                    || !matches!(
                        relative.components().collect::<Vec<_>>().as_slice(),
                        [Component::Normal(_)]
                    )
                {
                    anyhow::bail!("manifest 文件名必须是单个相对普通组件: {name}");
                }
                self.validate_file(&dir, &dir.join(relative))
            })
            .collect::<Result<_>>()?;
        if files.is_empty() {
            anyhow::bail!("manifest 引用的持久副本不完整");
        }
        if manifest.state != "pending" && manifest.state != "staging" && manifest.state != "failed"
        {
            anyhow::bail!("manifest state 非法: {}", manifest.state);
        }
        let item_meta = serde_json::to_string(&manifest.item)?;
        let files_json = serde_json::to_string(
            &files
                .iter()
                .map(|path| path.to_string_lossy())
                .collect::<Vec<_>>(),
        )?;
        let tx = conn.unchecked_transaction()?;
        let publication_json = match &manifest.publication {
            Some(publication) => serde_json::to_string(publication)?,
            None => "null".to_string(),
        };
        tx.execute(
            "INSERT OR IGNORE INTO gallery_outbox(
                source_kind,source_id,item_meta,files,work_dir,created_at,
                next_attempt_at,attempts,state,last_error,publication_json
             ) VALUES(?1,?2,?3,?4,?5,?6,?7,0,?8,?9,?10)",
            rusqlite::params![
                manifest.item.source.as_str(),
                manifest.item.source_id,
                item_meta,
                files_json,
                dir.to_string_lossy(),
                manifest.created_at,
                manifest.created_at + RETRY_BASE_SECS,
                manifest.state,
                manifest.last_error,
                publication_json,
            ],
        )?;
        // 同一 DB/outbox 迁到新根目录时，manifest 是当前副本位置的事实来源。
        // 即使 UNIQUE 行已存在，也必须覆盖旧绝对 files/work_dir。
        tx.execute(
            "UPDATE gallery_outbox SET item_meta=?1,files=?2,work_dir=?3,publication_json=?4
             WHERE source_kind=?5 AND source_id=?6",
            rusqlite::params![
                item_meta,
                files_json,
                dir.to_string_lossy(),
                publication_json,
                manifest.item.source.as_str(),
                manifest.item.source_id,
            ],
        )?;
        // activate 先原子更新 manifest，再更新 SQLite。若恰在两步之间崩溃，
        // 下次启动以 manifest 的 pending 事实补齐数据库；staging 绝不自动升级。
        if manifest.state == "pending" {
            tx.execute(
                "UPDATE gallery_outbox SET state='pending',next_attempt_at=?1
                 WHERE source_kind=?2 AND source_id=?3 AND state='staging'",
                rusqlite::params![
                    now_secs() + RETRY_BASE_SECS,
                    manifest.item.source.as_str(),
                    manifest.item.source_id,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn update_manifest(&self, dir: &Path, update: impl FnOnce(&mut Manifest)) -> Result<()> {
        let dir = self.validate_work_dir(dir)?;
        let path = dir.join(MANIFEST_NAME);
        self.validate_file(&dir, &path)?;
        let mut manifest: Manifest = serde_json::from_slice(&std::fs::read(&path)?)?;
        update(&mut manifest);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let temp = dir.join(format!(
            ".{MANIFEST_NAME}.tmp.{}.{}",
            std::process::id(),
            unique
        ));
        let bytes = serde_json::to_vec(&manifest)?;
        let update_result = (|| -> Result<()> {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temp, &path)?;
            Ok(())
        })();
        if update_result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        update_result
    }

    fn remove_completed(
        &self,
        conn: &rusqlite::Connection,
        id: i64,
        work_dir: &Path,
    ) -> Result<()> {
        if !work_dir.is_absolute() || work_dir.parent() != Some(self.root.as_path()) {
            anyhow::bail!(
                "拒绝清理 gallery outbox 根目录外 work_dir: {}",
                work_dir.display()
            );
        }
        match std::fs::symlink_metadata(work_dir) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                conn.execute(
                    "DELETE FROM gallery_outbox WHERE id=?1 AND state='completed'",
                    [id],
                )?;
            }
            Err(error) => return Err(error.into()),
            Ok(_) => {
                let validated = self.validate_work_dir(work_dir)?;
                std::fs::remove_dir_all(&validated)?;
                conn.execute(
                    "DELETE FROM gallery_outbox WHERE id=?1 AND state='completed'",
                    [id],
                )?;
            }
        }
        Ok(())
    }

    fn remove_private_dir(&self, work_dir: &Path) -> Result<()> {
        let validated = self.validate_work_dir(work_dir)?;
        std::fs::remove_dir_all(validated)?;
        Ok(())
    }
}

fn parse_publication_json(raw: &str) -> Result<Option<GalleryPublication>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(None);
    }
    Ok(serde_json::from_str(trimmed)?)
}

fn query_queued_raw(
    conn: &rusqlite::Connection,
    source_kind: &str,
    source_id: &str,
) -> Result<Option<QueuedGalleryWork>> {
    let row: Option<(String, String, String, String, String, String)> = conn
        .query_row(
            "SELECT source_kind,source_id,files,state,work_dir,publication_json FROM gallery_outbox
             WHERE source_kind=?1 AND source_id=?2 AND state <> 'completed'",
            rusqlite::params![source_kind, source_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?;
    row.map(
        |(source_kind, source_id, files, state, work_dir, publication_json)| {
            Ok(QueuedGalleryWork {
                source_kind,
                source_id,
                files: serde_json::from_str::<Vec<String>>(&files)?
                    .into_iter()
                    .map(PathBuf::from)
                    .collect(),
                publication: parse_publication_json(&publication_json)?,
                state,
                work_dir: PathBuf::from(work_dir),
            })
        },
    )
    .transpose()
}

pub async fn run_retry_loop(outbox: GalleryOutbox, gallery: GalleryClient) {
    loop {
        match outbox.retry_due_at(&gallery, now_secs(), 2).await {
            Ok(summary) if summary.succeeded > 0 => {
                tracing::info!(count = summary.succeeded, "图库补偿重试成功");
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(error = %error, "图库补偿队列轮询失败"),
        }
        tokio::time::sleep(Duration::from_secs(WORKER_INTERVAL_SECS)).await;
    }
}
