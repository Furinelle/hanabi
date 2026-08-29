use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use rusqlite::OptionalExtension;
use teloxide::prelude::*;
use teloxide::types::{
    AllowedUpdate, BotCommand, CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup,
    InputFile, InputMedia, InputMediaDocument, InputMediaPhoto, MessageId, MessageOrigin,
    ParseMode, Recipient, ReplyParameters, UpdateKind,
};
use tokio::sync::Mutex;

use crate::gallery::{GalleryClient, GalleryIngestError};
use crate::gallery_outbox::{GalleryOutbox, QueuedGalleryWork};
use crate::image_dedup::{
    evaluate_work, init_schema as init_image_dedup_schema, inspect_image, mark_work_status,
    record_work, remove_work, remove_work_key, render_review_notice, ExactAction, ImageFingerprint,
    WorkStatus, WorkSummary,
};
use crate::model::MediaItem;
use crate::similar_review::{
    claim_review as claim_similar_review, finish_review as finish_similar_review,
    init_schema as init_similar_review_schema, load_review as load_similar_review,
    parse_callback as parse_similar_callback, restore_review as restore_similar_review,
    review_messages as similar_review_messages, SimilarDecision, SimilarReviewGroup,
};
use crate::sink::{needs_downscale, render_caption, Sink};

/// Telegram photo 缩放目标边长上限(超限按比例缩到此框内)。
const MAX_DIMENSION: u32 = 4096;
/// pending 保留时长上限(秒);超期未审批自动清理(删消息+文件+记录)。
const PENDING_TTL_SECS: i64 = 7 * 24 * 3600;
/// 最近一次丢弃可撤销的保留时间。与 pending 一致，避免误丢弃文件永久占盘。
const UNDO_TTL_SECS: i64 = PENDING_TTL_SECS;

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 手动链接任务:URL + 用户链接消息 id + "抓取中"提示消息 id。
/// 发布成功后删这两条,保持审批私聊干净。
pub struct LinkJob {
    pub url: String,
    pub user_msg_id: i32,
    pub notice_msg_id: i32,
}

/// 直发结果:Full=全部批次发出;Partial=部分批次发出后失败(频道帖已存在但缺图,
/// 重发会重复,调用方须 mark_pushed 并提示人工补图)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    Full,
    Partial,
}

/// 待投递评论区的原图任务:发布到频道后登记,等讨论组 auto-forward 到来时
/// 把原画质 document reply 进该帖评论区。temp_dir 在投递完成或超时后清理。
struct CommentJob {
    originals: Vec<PathBuf>,
    temp_dir: PathBuf,
    created_at: i64,
}

/// 评论区原图任务的兜底保留时长:超时仍未等到 auto-forward 则清临时目录,避免泄漏。
const COMMENT_TTL_SECS: i64 = 120;
/// 孤儿目录最小年龄:比它新的 hanabi_* 目录可能是在途下载/发送(pending 行要等
/// 审批消息全部发出后才写入,窗口可达数分钟),清理时按修改时间避让。
const ORPHAN_MIN_AGE_SECS: u64 = 3600;

/// 审批状态:由 `TelegramSink`(发审批消息)与 callback 轮询任务共享。
/// pending 持久化到 sqlite,bot 重启后旧审批消息的按钮仍有效。
pub struct ReviewState {
    bot: Bot,
    review_chat: Recipient,     // 审批私聊
    owner: i64,                 // 审批私聊数字 id;仅响应本人的命令/链接
    publish_channel: Recipient, // 批准后发布频道
    db: Mutex<rusqlite::Connection>,
    counter: AtomicU64,
    // 频道帖首条 msg_id → 待投递评论区的原图任务。
    pending_comments: Mutex<std::collections::HashMap<i32, CommentJob>>,
    /// 可选图库入库客户端(Vitrine)。None 时不显示「发送并入库」。
    gallery: Option<GalleryClient>,
    /// 图库失败补偿队列。与 gallery 同时启用，副本独立于 pending/评论目录。
    gallery_outbox: Option<GalleryOutbox>,
}

impl ReviewState {
    fn next_token(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }
}

pub struct TelegramSink {
    state: Arc<ReviewState>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct PublishPendingSummary {
    pub requested: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub missing: usize,
    pub busy: usize,
    pub archive_failed: usize,
}

impl TelegramSink {
    pub fn new(
        token: String,
        review_chat_id: String,
        publish_channel_id: String,
        db_path: &str,
        gallery: Option<GalleryClient>,
    ) -> Result<Self> {
        let owner: i64 = match parse_owner(&review_chat_id) {
            Some(n) => n,
            None => {
                tracing::error!(
                    channel_id = %review_chat_id,
                    "channel_id 非数字 id, 命令/链接功能将无法响应(owner 校验恒不匹配); 请改用数字私聊 id"
                );
                0
            }
        };
        if publish_channel_id.is_empty() {
            tracing::error!(
                "telegram.publish_channel 未配置,批准/直发将无法发布到频道;请在 config.toml 填写"
            );
        }
        let conn = rusqlite::Connection::open(db_path).context("打开 pending 数据库失败")?;
        // busy_timeout 与 store.rs 对齐:抓取循环 Store 连接、handle_link 自开连接与
        // 本连接并发写同一 hanabi.db,SQLite 默认无 busy handler,写-写竞争会立即
        // 报 SQLITE_BUSY(WAL 只解决读写并发,解决不了写写)。
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS pending(
                token      INTEGER PRIMARY KEY,
                files      TEXT NOT NULL,
                caption    TEXT NOT NULL,
                msg_ids    TEXT NOT NULL,
                originals  TEXT NOT NULL DEFAULT '[]',
                created_at INTEGER NOT NULL DEFAULT 0,
                state      TEXT NOT NULL DEFAULT 'pending',
                is_r18     INTEGER NOT NULL DEFAULT 0,
                item_meta  TEXT NOT NULL DEFAULT '{}'
             );
             CREATE TABLE IF NOT EXISTS review_actions(
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                action     TEXT NOT NULL,
                token      INTEGER NOT NULL,
                files      TEXT NOT NULL DEFAULT '[]',
                caption    TEXT NOT NULL DEFAULT '',
                originals  TEXT NOT NULL DEFAULT '[]',
                is_r18     INTEGER NOT NULL DEFAULT 0,
                item_meta  TEXT NOT NULL DEFAULT '{}',
                acted_at   INTEGER NOT NULL,
                state      TEXT NOT NULL DEFAULT 'available'
             );",
        )
        .context("初始化 pending 表失败")?;
        init_image_dedup_schema(&conn).context("初始化图片去重表失败")?;
        init_similar_review_schema(&conn).context("初始化相似图审批表失败")?;
        // 兼容旧库:补列,已存在则忽略报错。
        let _ = conn.execute(
            "ALTER TABLE pending ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0",
            [],
        );
        // originals: 原始(缩放前)文件路径, 审批通过后把原图发进频道帖评论区。
        let _ = conn.execute(
            "ALTER TABLE pending ADD COLUMN originals TEXT NOT NULL DEFAULT '[]'",
            [],
        );
        // state: pending → publishing。用条件 UPDATE 原子抢占,防止同一审批按钮被连点后
        // 并发发送多次。旧库新增列后默认都是 pending。
        let _ = conn.execute(
            "ALTER TABLE pending ADD COLUMN state TEXT NOT NULL DEFAULT 'pending'",
            [],
        );
        // is_r18: 结构化存 R18 标记,发布时据此打剧透遮罩。此前从 caption 文本
        // contains("🔞 R18") 反推,与渲染格式隐式耦合且标题含该字样会误判。
        // 回填仅在列刚补建时执行一次(ALTER 成功=旧库):新写入的行自带准确值,
        // 每次启动都回填会把"标题恰含该字样"的非 R18 行重新误标。
        if conn
            .execute(
                "ALTER TABLE pending ADD COLUMN is_r18 INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .is_ok()
        {
            let _ = conn.execute(
                "UPDATE pending SET is_r18=1 WHERE caption LIKE '%🔞 R18%'",
                [],
            );
        }
        // item_meta: MediaItem JSON,审批「发送并入库」时带标签/来源入库。
        let _ = conn.execute(
            "ALTER TABLE pending ADD COLUMN item_meta TEXT NOT NULL DEFAULT '{}'",
            [],
        );
        // 进程崩溃时未完成的 publishing 没有活着的发送任务,启动后恢复为可重试状态。
        conn.execute(
            "UPDATE pending SET state='pending' WHERE state='publishing'",
            [],
        )
        .context("恢复中断的审批发布失败")?;
        conn.execute(
            "UPDATE review_actions SET state='available' WHERE state='restoring'",
            [],
        )
        .context("恢复中断的撤销操作失败")?;
        let backfilled = backfill_pending_image_catalog(&conn)?;
        if backfilled > 0 {
            tracing::info!(backfilled, "已为现有待审作品补建图片指纹");
        }
        // counter 从已有最大 token 续上,避免重启后 token 与旧记录冲突。
        let max_token: i64 = conn
            .query_row("SELECT COALESCE(MAX(token), 0) FROM pending", [], |r| {
                r.get(0)
            })
            .unwrap_or(0);
        // 自定义 client:
        // - timeout(300):整体超时。orig 4K 大图(几 MB)单张需数十秒、多图一次
        //   sendMediaGroup 可达 2-3 分钟,给足 5 分钟避免超时。
        // - connect_timeout(15):连接阶段超时,短一些好快速失败重试。
        // - trust_dns(true):纯 Rust DNS,避开 musl 静态二进制 getaddrinfo 解析失败
        //   (reqwest 0.11 光开 feature 不够,必须显式调用此方法)。
        // trust_dns 已 deprecated 但保留:它是 musl 静态二进制 DNS 解析的命脉
        // (reqwest 0.11 仅开 feature 不够,必须显式调用),不为消 lint 冒险换 hickory_dns。
        // - http1_only():审批一条挨一条点、不等上一条上传完就点下一条时,多张大图
        //   会在同一条 h2 连接上并发多路复用,踩中 h2 0.3.x 的内部状态机 bug(报
        //   "http2 error: stream error sent by user: unexpected internal error
        //   encountered",实际不是 Telegram 限流)。强制 HTTP/1.1 后并发请求走独立
        //   连接,规避这个 bug。
        #[allow(deprecated)]
        let client = teloxide::net::default_reqwest_settings()
            .timeout(std::time::Duration::from_secs(300))
            .connect_timeout(std::time::Duration::from_secs(15))
            .trust_dns(true)
            .http1_only()
            .build()
            .context("构造 reqwest client 失败")?;
        let gallery_outbox = if gallery.is_some() {
            Some(GalleryOutbox::for_database(db_path)?)
        } else {
            None
        };
        Ok(Self {
            state: Arc::new(ReviewState {
                bot: Bot::with_client(token, client),
                review_chat: to_recipient(review_chat_id),
                owner,
                publish_channel: to_recipient(publish_channel_id),
                db: Mutex::new(conn),
                counter: AtomicU64::new(max_token as u64 + 1),
                pending_comments: Mutex::new(std::collections::HashMap::new()),
                gallery,
                gallery_outbox,
            }),
        })
    }

    /// 供 main 启动 callback 轮询任务(与抓取循环并发)。
    pub fn state(&self) -> Arc<ReviewState> {
        self.state.clone()
    }

    /// 直接发布到频道(跳过审批):用于手动发来的链接,作品即时发布。
    ///
    /// 返回 [`PublishOutcome`]:`Partial` 表示部分批次已进频道后失败——频道帖已存在,
    /// 整条重发会重复,调用方应与完整成功一样 mark_pushed 并提示人工补图
    /// (与 finish_claimed 对部分成功"按已发布收尾"的策略一致)。
    pub async fn publish_direct(
        &self,
        item: &MediaItem,
        files: &[PathBuf],
    ) -> Result<PublishOutcome> {
        if files.is_empty() {
            anyhow::bail!("无图片可发: {}", item.source_id);
        }
        let fingerprints = inspect_images(files).await?;
        let caption = render_caption(item);
        let files_owned: Vec<PathBuf> = files.to_vec();
        let prepared = tokio::task::spawn_blocking(move || prepare_all(&files_owned)).await??;
        // 在任何频道副作用前准备独立图库副本，避免 auto-forward 评论任务先清理原目录。
        let gallery_staged = self.stage_gallery(item, files);
        let mut sent: Vec<MessageId> = Vec::new();
        let mut activate_on_first = || {
            if gallery_staged.is_some() {
                activate_gallery_staging(
                    self.state.gallery_outbox.as_ref(),
                    item.source.as_str(),
                    &item.source_id,
                );
            }
        };
        let send_result = send_group(
            &self.state.bot,
            &self.state.publish_channel,
            &prepared,
            &caption,
            item.is_r18, // R18 → 频道帖打剧透遮罩
            &mut sent,
            &mut activate_on_first,
        )
        .await;
        let mut send_error = send_result.err();
        if sent.is_empty() {
            if gallery_staged.is_some() {
                resolve_gallery_staging(
                    self.state.gallery_outbox.as_ref(),
                    item.source.as_str(),
                    &item.source_id,
                );
            }
            cleanup(files);
            if let Some(error) = send_error.take() {
                return Err(error);
            }
        }
        // 登记原图评论任务,等讨论组 auto-forward 到来再投递;登记则延后清理临时目录。
        // 部分批次已发出时同样登记(频道帖已存在,评论区原图仍有意义)。
        if let Some(mid) = sent.first() {
            register_comment(&self.state, mid.0, files).await;
        }
        if let Some(e) = send_error {
            // 部分成功:整条重发会造成频道重复内容,按已发布收尾,转人工检查。
            tracing::warn!(error = %e, id = %item.source_id, "直发部分成功,后续批次失败");
            // 部分成功也尝试入库(已有文件仍有意义)
            self.maybe_ingest(item, files, gallery_staged.as_ref())
                .await;
            let db = self.state.db.lock().await;
            if let Err(error) = record_work(&db, item, &fingerprints, WorkStatus::Published) {
                tracing::warn!(error = %error, id = %item.source_id, "登记直发图片指纹失败");
            }
            return Ok(PublishOutcome::Partial);
        }
        // 手动链接直发成功:同步入库(帖子自带标签)
        self.maybe_ingest(item, files, gallery_staged.as_ref())
            .await;
        let db = self.state.db.lock().await;
        if let Err(error) = record_work(&db, item, &fingerprints, WorkStatus::Published) {
            tracing::warn!(error = %error, id = %item.source_id, "登记直发图片指纹失败");
        }
        Ok(PublishOutcome::Full)
    }

    /// 维护入口：只处理显式指定的待审 token，复用正常审批的原子抢占、频道发布、
    /// 图库入库、评论区原图和审批消息清理流程。不会触碰未列出的 pending。
    pub async fn publish_pending_tokens(
        &self,
        tokens: &[i64],
        archive: bool,
    ) -> PublishPendingSummary {
        let mut summary = PublishPendingSummary {
            requested: tokens.len(),
            ..PublishPendingSummary::default()
        };
        for &token in tokens {
            let claim = {
                let db = self.state.db.lock().await;
                claim_pending(&db, token)
            };
            let row = match claim {
                Ok(PendingClaim::Claimed(row)) => row,
                Ok(PendingClaim::Missing) => {
                    summary.missing += 1;
                    continue;
                }
                Ok(PendingClaim::Publishing) => {
                    summary.busy += 1;
                    continue;
                }
                Err(e) => {
                    summary.failed += 1;
                    tracing::warn!(error = %e, token, "维护任务抢占审批失败");
                    continue;
                }
            };
            match finish_claimed(&self.state, token, row, true, archive).await {
                Ok(outcome) => {
                    summary.succeeded += 1;
                    if outcome.archive_failed {
                        summary.archive_failed += 1;
                    }
                }
                Err(e) => {
                    summary.failed += 1;
                    tracing::warn!(error = %e, token, "维护任务发布审批失败");
                }
            }
        }
        let text = if archive {
            format!(
                "✅ 恢复审批处理完成：请求 {} 条，发布成功 {} 条，发布失败 {} 条，图库失败 {} 条，已失效 {} 条，处理中 {} 条",
                summary.requested,
                summary.succeeded,
                summary.failed,
                summary.archive_failed,
                summary.missing,
                summary.busy
            )
        } else {
            format!(
                "✅ 恢复审批处理完成：请求 {} 条，发布成功 {} 条，发布失败 {} 条，已失效 {} 条，处理中 {} 条",
                summary.requested,
                summary.succeeded,
                summary.failed,
                summary.missing,
                summary.busy
            )
        };
        let _ = self
            .state
            .bot
            .send_message(self.state.review_chat.clone(), text)
            .await;
        summary
    }

    fn stage_gallery(&self, item: &MediaItem, files: &[PathBuf]) -> Option<QueuedGalleryWork> {
        self.state.gallery.as_ref()?;
        stage_gallery_work(self.state.gallery_outbox.as_ref(), item, files)
    }

    async fn maybe_ingest(
        &self,
        item: &MediaItem,
        files: &[PathBuf],
        staged: Option<&QueuedGalleryWork>,
    ) {
        let Some(gallery) = self.state.gallery.as_ref() else {
            return;
        };
        // 预备份成功后立即改用持久副本，避免评论任务并发清理原图。
        let ingest_files = staged.map_or(files, |queued| queued.files.as_slice());
        match gallery.ingest(item, ingest_files).await {
            Ok(()) => {
                if staged.is_some() {
                    resolve_gallery_staging(
                        self.state.gallery_outbox.as_ref(),
                        item.source.as_str(),
                        &item.source_id,
                    );
                }
            }
            Err(e) => {
                let queued =
                    enqueue_gallery_failure(self.state.gallery_outbox.as_ref(), item, files, &e);
                tracing::warn!(
                    id = %item.source_id,
                    error = %e,
                    queued,
                    "直发后图库入库失败"
                );
                let suffix = if queued && e.is_retryable() {
                    "；已加入自动补偿队列"
                } else if queued {
                    "；已保留持久副本并停止自动重试，请人工检查"
                } else {
                    "；补偿队列登记失败，请人工处理"
                };
                let _ = self
                    .state
                    .bot
                    .send_message(
                        self.state.review_chat.clone(),
                        format!("⚠️ 频道已发,但图库入库失败: {e}{suffix}"),
                    )
                    .await;
            }
        }
    }

    /// 删审批私聊里的若干消息(手动链接发布后清理:用户链接 + "抓取中"提示)。
    pub async fn delete_review_messages(&self, msg_ids: &[i32]) {
        for id in msg_ids {
            let _ = self
                .state
                .bot
                .delete_message(self.state.review_chat.clone(), MessageId(*id))
                .await;
        }
    }

    /// 编辑审批私聊里某条消息文本(把"抓取中"改成结果提示)。
    pub async fn edit_review_text(&self, msg_id: i32, text: &str) {
        let _ = self
            .state
            .bot
            .edit_message_text(
                self.state.review_chat.clone(),
                MessageId(msg_id),
                text.to_string(),
            )
            .await;
    }
}

/// 距上次清理是否已超 interval(秒)。
fn cleanup_due(last_secs: i64, now_secs: i64, interval_secs: i64) -> bool {
    now_secs - last_secs >= interval_secs
}

/// 解析审批私聊数字 id。非数字(如 @username)返回 None —— 命令/链接功能要求数字 id。
fn parse_owner(review_chat_id: &str) -> Option<i64> {
    review_chat_id.parse::<i64>().ok()
}

fn to_recipient(id: String) -> Recipient {
    match id.parse::<i64>() {
        Ok(n) => Recipient::Id(ChatId(n)),
        Err(_) => Recipient::ChannelUsername(id),
    }
}

type PendingRow = (String, String, String, String, bool, String);

/// 审批记录的原子抢占结果。只有从 pending 成功切到 publishing 的回调能真正发图。
enum PendingClaim {
    Claimed(PendingRow),
    Missing,
    Publishing,
}

struct UndoAction {
    id: i64,
    row: PendingRow,
}

enum UndoClaim {
    Claimed(UndoAction),
    Empty,
    Irreversible(String),
}

/// 原子完成一条审批并登记为“最近一次动作”。只保留最新 available 动作；若旧动作
/// 是可撤销丢弃，返回其文件供事务提交后清理。正在 restoring 的旧动作由对应 /undo
/// 任务持有，不能在这里删除或清文件。
fn complete_and_record_action(
    db: &mut rusqlite::Connection,
    token: i64,
    action: &str,
    row: Option<&PendingRow>,
) -> rusqlite::Result<Vec<Vec<PathBuf>>> {
    let tx = db.transaction()?;
    let changed = tx.execute(
        "DELETE FROM pending WHERE token=?1 AND state='publishing'",
        [token],
    )?;
    if changed != 1 {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }

    let (files, caption, originals, is_r18, item_meta) = match row {
        Some((files, caption, _, originals, is_r18, item_meta)) => (
            files.as_str(),
            caption.as_str(),
            originals.as_str(),
            *is_r18,
            item_meta.as_str(),
        ),
        None => ("[]", "", "[]", false, "{}"),
    };
    tx.execute(
        "INSERT INTO review_actions(action,token,files,caption,originals,is_r18,item_meta,acted_at,state)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'available')",
        rusqlite::params![
            action,
            token,
            files,
            caption,
            originals,
            is_r18,
            item_meta,
            now_secs()
        ],
    )?;
    let new_id = tx.last_insert_rowid();

    let old_discards: Vec<(String, String)> = {
        let mut stmt = tx.prepare(
            "SELECT files, originals FROM review_actions
             WHERE id<>?1 AND state='available' AND action='discard'",
        )?;
        let rows = stmt
            .query_map([new_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        rows
    };
    tx.execute(
        "DELETE FROM review_actions WHERE id<>?1 AND state='available'",
        [new_id],
    )?;
    tx.commit()?;

    Ok(old_discards
        .into_iter()
        .filter_map(|(files, originals)| {
            let raw = serde_json::from_str::<Vec<String>>(&originals)
                .ok()
                .filter(|v| !v.is_empty())
                .or_else(|| serde_json::from_str::<Vec<String>>(&files).ok())?;
            Some(raw.into_iter().map(PathBuf::from).collect())
        })
        .collect())
}

/// 抢占调用时的最近一次动作。只有最近动作本身是丢弃才可恢复，避免在一次发布之后
/// 错把更早的丢弃当作“最后一次动作”撤销。
fn claim_latest_undo(db: &mut rusqlite::Connection) -> rusqlite::Result<UndoClaim> {
    let tx = db.transaction()?;
    let action = tx
        .query_row(
            "SELECT id,action,token,files,caption,originals,is_r18,item_meta
             FROM review_actions WHERE state='available' ORDER BY id DESC LIMIT 1",
            [],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, bool>(6)?,
                    r.get::<_, String>(7)?,
                ))
            },
        )
        .optional()?;
    let Some((id, action, _token, files, caption, originals, is_r18, item_meta)) = action else {
        tx.commit()?;
        return Ok(UndoClaim::Empty);
    };
    if action != "discard" {
        tx.commit()?;
        return Ok(UndoClaim::Irreversible(action));
    }
    let changed = tx.execute(
        "UPDATE review_actions SET state='restoring' WHERE id=?1 AND state='available'",
        [id],
    )?;
    if changed != 1 {
        tx.commit()?;
        return Ok(UndoClaim::Empty);
    }
    tx.commit()?;
    Ok(UndoClaim::Claimed(UndoAction {
        id,
        row: (files, caption, "[]".into(), originals, is_r18, item_meta),
    }))
}

fn finish_undo(db: &rusqlite::Connection, id: i64) -> rusqlite::Result<usize> {
    db.execute(
        "DELETE FROM review_actions WHERE id=?1 AND state='restoring'",
        [id],
    )
}

fn restore_undo(db: &rusqlite::Connection, id: i64) -> rusqlite::Result<usize> {
    db.execute(
        "UPDATE review_actions SET state='available' WHERE id=?1 AND state='restoring'",
        [id],
    )
}

/// 原子把待审记录抢为发布中，并取出其发送数据。
///
/// 两次 callback 都可能同时到达；条件 UPDATE 让数据库只允许一个回调从 pending
/// 转为 publishing，另一个回调只会拿到 Publishing，绝不会再启动第二次上传。
fn claim_pending(db: &rusqlite::Connection, token: i64) -> rusqlite::Result<PendingClaim> {
    let changed = db.execute(
        "UPDATE pending SET state='publishing' WHERE token=?1 AND state='pending'",
        [token],
    )?;
    if changed == 0 {
        let exists = db
            .query_row("SELECT 1 FROM pending WHERE token=?1", [token], |_| Ok(()))
            .optional()?
            .is_some();
        return Ok(if exists {
            PendingClaim::Publishing
        } else {
            PendingClaim::Missing
        });
    }
    let row = db.query_row(
        "SELECT files, caption, msg_ids, originals, is_r18, COALESCE(item_meta, '{}') FROM pending WHERE token=?1",
        [token],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
    )?;
    Ok(PendingClaim::Claimed(row))
}

/// 发布失败时放弃抢占，让原审批按钮可以重试。
fn restore_pending(db: &rusqlite::Connection, token: i64) -> rusqlite::Result<usize> {
    db.execute(
        "UPDATE pending SET state='pending' WHERE token=?1 AND state='publishing'",
        [token],
    )
}

/// 原子抢占当前所有仍为 pending 的审批记录，按进入审批的先后顺序返回。
///
/// 已点丢弃/单独批准的记录会先进入 publishing，因此不会被 `/approve` 再次选中；
/// 重复发送 `/approve` 也只会由第一次命令拿到这些记录。
fn claim_all_pending(db: &mut rusqlite::Connection) -> rusqlite::Result<Vec<(i64, PendingRow)>> {
    let tx = db.transaction()?;
    let tokens: Vec<i64> = {
        let mut stmt = tx.prepare(
            "SELECT token FROM pending WHERE state='pending' ORDER BY created_at, token",
        )?;
        let rows = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        rows
    };
    let mut claimed = Vec::with_capacity(tokens.len());
    for token in tokens {
        let changed = tx.execute(
            "UPDATE pending SET state='publishing' WHERE token=?1 AND state='pending'",
            [token],
        )?;
        if changed == 1 {
            let row = tx.query_row(
                "SELECT files, caption, msg_ids, originals, is_r18, COALESCE(item_meta, '{}') FROM pending WHERE token=?1",
                [token],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )?;
            claimed.push((token, row));
        }
    }
    tx.commit()?;
    Ok(claimed)
}

fn command_menu(gallery_enabled: bool) -> Vec<BotCommand> {
    let mut commands = vec![
        BotCommand::new("run", "立即抓取一轮"),
        BotCommand::new("approve", "批准并发布全部剩余待审"),
    ];
    if gallery_enabled {
        commands.push(BotCommand::new(
            "approve_archive",
            "批准、发布并入库全部剩余待审",
        ));
    }
    commands.extend([
        BotCommand::new("undo", "撤销最近一次误丢弃"),
        BotCommand::new("status", "查看待审数和运行状态"),
        BotCommand::new("ping", "存活测试"),
        BotCommand::new("help", "查看命令列表"),
    ]);
    commands
}

/// 若 msg 是 `publish_channel` 帖子自动转发到讨论组的那条(评论锚点),返回被转发的
/// 频道帖 msg_id。频道来源为 `MessageOrigin::Channel`(便捷访问器 forward_from_chat
/// 只认 Chat 变体、对 Channel 返回 None,故此处直接 match Channel 取 .chat)。
fn match_auto_forward(msg: &Message, publish_channel: &Recipient) -> Option<i32> {
    if !msg.is_automatic_forward() {
        return None;
    }
    let (from_chat, msg_id) = match msg.forward_origin()? {
        MessageOrigin::Channel {
            chat, message_id, ..
        } => (chat, message_id),
        _ => return None,
    };
    let matches_channel = match publish_channel {
        Recipient::Id(id) => from_chat.id == *id,
        Recipient::ChannelUsername(name) => {
            from_chat.username().map(|u| format!("@{u}")).as_deref() == Some(name.as_str())
        }
    };
    if matches_channel {
        Some(msg_id.0)
    } else {
        None
    }
}

/// 登记原图评论任务:发布到频道后调用,等讨论组 auto-forward 到来再投递。
async fn register_comment(state: &Arc<ReviewState>, first_msg_id: i32, originals: &[PathBuf]) {
    let temp_dir = match originals.first().and_then(|p| p.parent()) {
        Some(d) => d.to_path_buf(),
        None => return,
    };
    state.pending_comments.lock().await.insert(
        first_msg_id,
        CommentJob {
            originals: originals.to_vec(),
            temp_dir,
            created_at: now_secs(),
        },
    );
}

/// 构造 document 图组(原画质,不压缩)。
fn build_documents(files: &[PathBuf]) -> Vec<InputMedia> {
    files
        .iter()
        .map(|p| InputMedia::Document(InputMediaDocument::new(InputFile::file(p))))
        .collect()
}

/// 构造 document 图组并挂 caption(第一张挂,其余无)。用于超限退化发送场景,
/// 需要保留原 caption 但不再走 sendPhoto。
fn build_documents_with_caption(files: &[PathBuf], caption: &str) -> Vec<InputMedia> {
    files
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let mut doc = InputMediaDocument::new(InputFile::file(p));
            if i == 0 && !caption.is_empty() {
                doc = doc.caption(caption.to_string()).parse_mode(ParseMode::Html);
            }
            InputMedia::Document(doc)
        })
        .collect()
}

/// 组内是否有文件超过 Telegram photo 硬上限。缩放(prepare)后仍可能超限的高细节图,
/// sendPhoto 必定被拒,需整组退化为 sendDocument(album 不允许 photo/document 混投)。
fn any_oversized_for_photo(files: &[PathBuf]) -> bool {
    files.iter().any(|p| {
        std::fs::metadata(p)
            .map(|m| m.len() > crate::sink::PHOTO_HARD_LIMIT_BYTES)
            .unwrap_or(false)
    })
}

/// 把原图作为 document 组 reply 到讨论组那条 auto-forward 上(即帖子评论区)。
/// sendMediaGroup 限 10,超出按 10 分批,每批都 reply 到锚点。
async fn send_documents_reply(
    bot: &Bot,
    chat: &Recipient,
    files: &[PathBuf],
    reply_to: MessageId,
) -> Result<()> {
    for chunk in files.chunks(10) {
        if chunk.len() == 1 {
            tg_retry(|| {
                bot.send_document(chat.clone(), InputFile::file(&chunk[0]))
                    .reply_parameters(ReplyParameters::new(reply_to))
            })
            .await?;
        } else {
            tg_retry(|| {
                bot.send_media_group(chat.clone(), build_documents(chunk))
                    .reply_parameters(ReplyParameters::new(reply_to))
            })
            .await?;
        }
    }
    Ok(())
}

/// 收到匹配的 auto-forward 后投递原图到评论区。取出任务、发送、清临时目录。
/// 失败也清目录(频道帖 photo 已发出,评论区只是增益,不重试以免泄漏)。
async fn deliver_comment(state: &Arc<ReviewState>, anchor: &Message, chan_msg_id: i32) {
    let job = state.pending_comments.lock().await.remove(&chan_msg_id);
    let Some(job) = job else {
        return;
    };
    let chat = Recipient::Id(anchor.chat.id);
    if let Err(e) = send_documents_reply(&state.bot, &chat, &job.originals, anchor.id).await {
        tracing::warn!(error = %e, chan_msg_id, "原图投递评论区失败");
    } else {
        tracing::info!(
            chan_msg_id,
            n = job.originals.len(),
            "原图已投递到帖子评论区"
        );
    }
    remove_dir_all_bg(job.temp_dir);
}

/// 兜底:清理超时仍未等到 auto-forward 的评论任务(频道没绑讨论组/转发丢失)。
async fn sweep_expired_comments(state: &Arc<ReviewState>) {
    let now = now_secs();
    let expired: Vec<CommentJob> = {
        let mut map = state.pending_comments.lock().await;
        let keys: Vec<i32> = map
            .iter()
            .filter(|(_, j)| now - j.created_at > COMMENT_TTL_SECS)
            .map(|(k, _)| *k)
            .collect();
        keys.iter().filter_map(|k| map.remove(k)).collect()
    };
    for job in &expired {
        remove_dir_all_bg(job.temp_dir.clone());
    }
    if !expired.is_empty() {
        tracing::info!(count = expired.len(), "清理超时未投递的评论任务临时目录");
    }
}

/// 包装 Telegram 请求:遇限流 `RetryAfter` 自动等待后重试(最多 5 次)。
async fn tg_retry<F, R, T>(f: F) -> std::result::Result<T, teloxide::RequestError>
where
    F: Fn() -> R,
    R: std::future::IntoFuture<Output = std::result::Result<T, teloxide::RequestError>>,
{
    let mut tries = 0u32;
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(teloxide::RequestError::RetryAfter(after)) if tries < 5 => {
                tries += 1;
                let wait = after.duration() + std::time::Duration::from_secs(1);
                tracing::warn!(?wait, "Telegram 限流,等待后重试");
                tokio::time::sleep(wait).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// 超限则缩放到限制内,返回最终发送路径(可能是缩放后的临时文件)。
fn prepare(path: &Path) -> Result<PathBuf> {
    let bytes = std::fs::metadata(path)?.len();
    let (w, h) = image::image_dimensions(path).unwrap_or((0, 0));
    if !needs_downscale(bytes, w, h) {
        return Ok(path.to_path_buf());
    }
    let dyn_img = image::open(path).context("打开图片失败")?;
    let scaled = dyn_img.resize(
        MAX_DIMENSION,
        MAX_DIMENSION,
        image::imageops::FilterType::Lanczos3,
    );
    // 保留原格式: PNG 缩放后仍是 PNG(保 alpha), JPG 仍是 JPG。
    // DynamicImage::save 按扩展名推断编码,RGBA 透明通道得以保留。
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("jpg");
    let out = path.with_extension(format!("scaled.{ext}"));
    scaled.save(&out).context("保存缩放图失败")?;
    Ok(out)
}

/// 供维护工具在恢复待审原图后重建与正常审批一致的发送文件。
pub fn prepare_all(files: &[PathBuf]) -> Result<Vec<PathBuf>> {
    files.iter().map(|p| prepare(p)).collect()
}

/// 构造图组:第一张挂 caption,其余无。spoiler=true 时整组打剧透遮罩(R18)。
fn build_media(prepared: &[PathBuf], caption: &str, spoiler: bool) -> Vec<InputMedia> {
    prepared
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let mut photo = InputMediaPhoto::new(InputFile::file(p));
            if spoiler {
                photo = photo.spoiler();
            }
            if i == 0 && !caption.is_empty() {
                photo = photo
                    .caption(caption.to_string())
                    .parse_mode(ParseMode::Html);
            }
            InputMedia::Photo(photo)
        })
        .collect()
}

/// 发送模式:photo(可带 R18 剧透遮罩)或 document(原画质/超限退化)。
enum MediaMode {
    Photo { spoiler: bool },
    Document,
}

/// 发一组图到指定 chat(用于发布到频道)。组内任一文件超 Telegram photo 硬上限时,
/// 整组退化为 document 发送(album 不允许 photo/document 混投,故不能只降级单张;
/// R18 剧透遮罩仅 photo 支持,随退化自然丢失)。
async fn send_group(
    bot: &Bot,
    chat: &Recipient,
    prepared: &[PathBuf],
    caption: &str,
    spoiler: bool,
    sent: &mut Vec<MessageId>,
    on_first_sent: &mut impl FnMut(),
) -> Result<()> {
    if prepared.is_empty() {
        anyhow::bail!("无图可发");
    }
    let mode = if any_oversized_for_photo(prepared) {
        MediaMode::Document
    } else {
        MediaMode::Photo { spoiler }
    };
    send_batches(bot, chat, prepared, caption, &mode, sent, on_first_sent).await
}

fn record_sent(
    sent: &mut Vec<MessageId>,
    ids: impl IntoIterator<Item = MessageId>,
    on_first_sent: &mut impl FnMut(),
) {
    let was_empty = sent.is_empty();
    sent.extend(ids);
    if was_empty && !sent.is_empty() {
        on_first_sent();
    }
}

/// 分批发送核心:sendMediaGroup 限 2–10,按 10 分批,余数 1 张退化为单发。
/// caption 仅挂第一批第一张。每个请求带限流重试。已发出的消息 id 实时推入 `sent`
/// (首元素即频道帖锚点),中途失败时调用方据 `sent` 是否非空判断"部分批次已进
/// 频道"——此时盲目恢复重试会造成频道重复内容。
async fn send_batches(
    bot: &Bot,
    chat: &Recipient,
    prepared: &[PathBuf],
    caption: &str,
    mode: &MediaMode,
    sent: &mut Vec<MessageId>,
    on_first_sent: &mut impl FnMut(),
) -> Result<()> {
    for (ci, chunk) in prepared.chunks(10).enumerate() {
        let cap = if ci == 0 { caption } else { "" };
        if chunk.len() == 1 {
            let m = match mode {
                MediaMode::Photo { spoiler } => {
                    tg_retry(|| {
                        let req = bot
                            .send_photo(chat.clone(), InputFile::file(&chunk[0]))
                            .has_spoiler(*spoiler);
                        if cap.is_empty() {
                            req
                        } else {
                            req.caption(cap.to_string()).parse_mode(ParseMode::Html)
                        }
                    })
                    .await?
                }
                MediaMode::Document => {
                    tg_retry(|| {
                        let req = bot.send_document(chat.clone(), InputFile::file(&chunk[0]));
                        if cap.is_empty() {
                            req
                        } else {
                            req.caption(cap.to_string()).parse_mode(ParseMode::Html)
                        }
                    })
                    .await?
                }
            };
            record_sent(sent, [m.id], on_first_sent);
        } else {
            let msgs = tg_retry(|| {
                let media = match mode {
                    MediaMode::Photo { spoiler } => build_media(chunk, cap, *spoiler),
                    MediaMode::Document => build_documents_with_caption(chunk, cap),
                };
                bot.send_media_group(chat.clone(), media)
            })
            .await?;
            record_sent(sent, msgs.iter().map(|m| m.id), on_first_sent);
        }
    }
    Ok(())
}

/// 后台删除目录:同步 remove_dir_all 移到 blocking 线程,不占用 async 任务
/// (慢盘上删含多张几 MB 图片的目录会卡顿轮询)。先原子改名再删:目录名是确定性的
/// (hanabi_<源>_<id>),失败后立即重发会对同名目录重新写入,与在途删除并发会误删
/// 新文件;改名后删的是"退役"目录,新下载互不相干。fire-and-forget,失败仅意味着
/// 目录残留,由孤儿清理兜底(改名保留 hanabi_ 前缀,仍在其扫描范围内)。
fn remove_dir_all_bg(dir: PathBuf) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let doomed = dir.with_file_name(format!(
        "{}.del{ts}",
        dir.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("hanabi_tmp")
    ));
    let target = if std::fs::rename(&dir, &doomed).is_ok() {
        doomed
    } else {
        dir
    };
    tokio::task::spawn_blocking(move || {
        let _ = std::fs::remove_dir_all(&target);
    });
}

/// 清理某作品的临时目录(原图 + 缩放图同处一目录)。
fn cleanup(files: &[PathBuf]) {
    if let Some(parent) = files.first().and_then(|p| p.parent()) {
        remove_dir_all_bg(parent.to_path_buf());
    }
}

/// 启动清理:① 删超期未审 pending(消息+文件+记录);② 删持久待审目录中
/// 不被任何 pending 引用的孤儿目录(多为旧版本/重启遗留)。
async fn cleanup_stale(state: &Arc<ReviewState>) {
    // ⓪ 僵尸 publishing 兜底:DELETE/restore 因 DB 写失败未完成时,记录会永远停在
    // publishing(按钮恒回"已在发布中",TTL 清理又只认 pending)。超过 TTL+24h 的
    // publishing 不可能仍在途,恢复为 pending 交给下面的正常清理/重试。
    {
        let db = state.db.lock().await;
        let zombie_cutoff = now_secs() - PENDING_TTL_SECS - 24 * 3600;
        let _ = db.execute(
            "UPDATE pending SET state='pending' WHERE state='publishing' AND created_at > 0 AND created_at < ?1",
            [zombie_cutoff],
        );
    }
    // ⓪-b 只保留最近一个 available 动作，并让过期丢弃释放文件。restoring 由正在
    // 执行的 /undo 持有，不能在此清理。
    let undo_cutoff = now_secs() - UNDO_TTL_SECS;
    let stale_undo_files: Vec<String> = {
        let db = state.db.lock().await;
        let newest: Option<i64> = db
            .query_row(
                "SELECT MAX(id) FROM review_actions WHERE state='available'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(None);
        let mut out = Vec::new();
        if let Ok(mut stmt) = db.prepare(
            "SELECT files FROM review_actions
             WHERE state='available' AND action='discard'
               AND (acted_at < ?1 OR id <> COALESCE(?2, -1))",
        ) {
            if let Ok(rows) = stmt.query_map(rusqlite::params![undo_cutoff, newest], |r| {
                r.get::<_, String>(0)
            }) {
                out.extend(rows.flatten());
            }
        }
        let _ = db.execute(
            "DELETE FROM review_actions
             WHERE state='available' AND (acted_at < ?1 OR id <> COALESCE(?2, -1))",
            rusqlite::params![undo_cutoff, newest],
        );
        out
    };
    for files_json in stale_undo_files {
        if let Ok(files) = serde_json::from_str::<Vec<String>>(&files_json) {
            let paths: Vec<PathBuf> = files.into_iter().map(PathBuf::from).collect();
            cleanup(&paths);
        }
    }
    // ① 超期 pending。
    let cutoff = now_secs() - PENDING_TTL_SECS;
    let expired: Vec<(i64, String, String, String)> = {
        let db = state.db.lock().await;
        let mut out = Vec::new();
        if let Ok(mut stmt) = db.prepare(
            // 只清 pending:publishing 表示有发布任务在途,清了会与其互踩
            // (删其正在上传的文件、DELETE 其记录,失败后的 restore 变成空操作)。
            "SELECT token, files, msg_ids, COALESCE(item_meta, '{}') FROM pending WHERE created_at > 0 AND created_at < ?1 AND state='pending'",
        ) {
            if let Ok(rows) = stmt.query_map([cutoff], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            }) {
                out.extend(rows.flatten());
            }
        }
        out
    };
    for (token, files_json, msg_json, item_meta) in &expired {
        if let Ok(ids) = serde_json::from_str::<Vec<i32>>(msg_json) {
            for mid in ids {
                let _ = state
                    .bot
                    .delete_message(state.review_chat.clone(), MessageId(mid))
                    .await;
            }
        }
        if let Ok(files) = serde_json::from_str::<Vec<String>>(files_json) {
            let paths: Vec<PathBuf> = files.into_iter().map(PathBuf::from).collect();
            cleanup(&paths);
        }
        let db = state.db.lock().await;
        let _ = db.execute("DELETE FROM pending WHERE token=?1", [*token]);
        if let Ok(item) = serde_json::from_str::<MediaItem>(item_meta) {
            let _ = remove_work(&db, &item);
        }
    }
    if !expired.is_empty() {
        tracing::info!(count = expired.len(), "清理超期 pending");
    }

    // ② 孤儿临时目录。先把 files JSON 收集成 owned(释放 db 锁),再在锁外解析。
    let file_jsons: Vec<String> = {
        let db = state.db.lock().await;
        let mut out = Vec::new();
        if let Ok(mut stmt) = db.prepare("SELECT files FROM pending") {
            if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
                out.extend(rows.flatten());
            }
        }
        if let Ok(mut stmt) = db.prepare("SELECT files FROM review_actions WHERE action='discard'")
        {
            if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
                out.extend(rows.flatten());
            }
        }
        out
    };
    let mut referenced: HashSet<PathBuf> = HashSet::new();
    for fj in file_jsons {
        if let Ok(files) = serde_json::from_str::<Vec<String>>(&fj) {
            for f in files {
                if let Some(parent) = PathBuf::from(&f).parent() {
                    referenced.insert(parent.to_path_buf());
                }
            }
        }
    }
    // 评论任务的目录也在用(其 pending 行已删,仅存于内存 map),同样不能清。
    {
        let comments = state.pending_comments.lock().await;
        for job in comments.values() {
            referenced.insert(job.temp_dir.clone());
        }
    }
    // 扫描/删除是同步 IO,整体移到 blocking 线程。
    let _ = tokio::task::spawn_blocking(move || {
        let Ok(rd) = std::fs::read_dir(crate::util::pending_root()) else {
            return;
        };
        let mut orphans = 0;
        for e in rd.flatten() {
            let p = e.path();
            let is_hanabi = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("hanabi_"));
            if !(is_hanabi && p.is_dir()) || referenced.contains(&p) {
                continue;
            }
            // 新目录避让:正在下载/正在发审批消息的作品目录此刻还没有 pending 记录
            // (记录要等消息全部发出后才 INSERT,大图组上传可达数分钟),按修改时间
            // 跳过近一小时内活跃的目录,防止误删在用文件。
            let fresh = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|d| d.as_secs() < ORPHAN_MIN_AGE_SECS);
            if fresh {
                continue;
            }
            let _ = std::fs::remove_dir_all(&p);
            orphans += 1;
        }
        if orphans > 0 {
            tracing::info!(orphans, "清理孤儿临时目录");
        }
    })
    .await;
}

async fn inspect_images(files: &[PathBuf]) -> Result<Vec<ImageFingerprint>> {
    let files = files.to_vec();
    tokio::task::spawn_blocking(move || {
        files
            .iter()
            .map(|path| inspect_image(path))
            .collect::<Result<Vec<_>>>()
    })
    .await?
}

fn backfill_pending_image_catalog(conn: &rusqlite::Connection) -> Result<usize> {
    let rows: Vec<(String, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT COALESCE(item_meta, '{}'),originals,files FROM pending WHERE state='pending'",
        )?;
        let mapped = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        mapped.collect::<rusqlite::Result<_>>()?
    };
    let mut backfilled = 0;
    for (item_meta, originals_json, files_json) in rows {
        let Ok(item) = serde_json::from_str::<MediaItem>(&item_meta) else {
            continue;
        };
        let (kind, source_id) = item.dedup_key();
        let exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM image_fingerprints WHERE source_kind=?1 AND source_id=?2",
            rusqlite::params![kind, source_id],
            |row| row.get(0),
        )?;
        if exists > 0 {
            continue;
        }
        let raw = serde_json::from_str::<Vec<String>>(&originals_json)
            .ok()
            .filter(|paths| !paths.is_empty())
            .or_else(|| serde_json::from_str::<Vec<String>>(&files_json).ok())
            .unwrap_or_default();
        let fingerprints = raw
            .iter()
            .map(|path| inspect_image(Path::new(path)))
            .collect::<Result<Vec<_>>>();
        match fingerprints {
            Ok(fingerprints) if !fingerprints.is_empty() => {
                record_work(conn, &item, &fingerprints, WorkStatus::Pending)?;
                backfilled += 1;
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(
                error = %error,
                source = item.source.as_str(),
                id = %item.source_id,
                "现有待审图片指纹补建失败"
            ),
        }
    }
    Ok(backfilled)
}

/// 高清严格同图到达时，只能替换仍处于 pending 的旧审批卡片。若旧卡正发布或已经
/// 发布，不能后台删除外部消息；调用方会保留旧版本并跳过当前版本以避免重复。
async fn supersede_pending_work(state: &Arc<ReviewState>, old: &WorkSummary) -> Result<bool> {
    let claimed = {
        let db = state.db.lock().await;
        let rows: Vec<(i64, PendingRow)> = {
            let mut stmt = db.prepare(
                "SELECT token,files,caption,msg_ids,originals,is_r18,COALESCE(item_meta, '{}')
                 FROM pending WHERE state='pending' ORDER BY created_at,token",
            )?;
            let mapped = stmt.query_map([], |row| {
                Ok((
                    row.get(0)?,
                    (
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ),
                ))
            })?;
            mapped.collect::<rusqlite::Result<_>>()?
        };
        let found = rows.into_iter().find(|(_, row)| {
            serde_json::from_str::<MediaItem>(&row.5)
                .is_ok_and(|item| item.source == old.source && item.source_id == old.source_id)
        });
        if let Some((token, row)) = found {
            let changed = db.execute(
                "DELETE FROM pending WHERE token=?1 AND state='pending'",
                [token],
            )?;
            if changed == 1 {
                remove_work_key(&db, old.source.as_str(), &old.source_id)?;
                Some(decode_pending(row)?)
            } else {
                None
            }
        } else {
            None
        }
    };
    let Some(work) = claimed else {
        return Ok(false);
    };
    for mid in &work.msg_ids {
        let _ = state
            .bot
            .delete_message(state.review_chat.clone(), MessageId(*mid))
            .await;
    }
    if !work.originals.is_empty() {
        cleanup(&work.originals);
    } else {
        cleanup(&work.files);
    }
    Ok(true)
}

#[async_trait]
impl Sink for TelegramSink {
    /// 发到审批私聊:**全套图**(单图=sendPhoto+按钮;多图=图组+一条带按钮的控制消息)。
    /// 发送成功后把 pending 持久化到 sqlite;文件保留到审批结束才清理。
    async fn deliver(&self, item: &MediaItem, files: &[PathBuf]) -> Result<()> {
        if files.is_empty() {
            anyhow::bail!("无图片可发: {}", item.source_id);
        }
        let fingerprints = inspect_images(files).await?;
        let evaluation = {
            let db = self.state.db.lock().await;
            evaluate_work(&db, item, &fingerprints)?
        };
        match &evaluation.exact_action {
            ExactAction::SkipCurrent(old) => {
                tracing::info!(
                    id = %item.source_id,
                    kept_source = old.source.as_str(),
                    kept_id = %old.source_id,
                    status = ?old.status,
                    "严格同图自动去重,保留已有版本"
                );
                cleanup(files);
                return Ok(());
            }
            ExactAction::ReplacePending(old) => {
                if !supersede_pending_work(&self.state, old).await? {
                    tracing::warn!(
                        id = %item.source_id,
                        old_source = old.source.as_str(),
                        old_id = %old.source_id,
                        "高清严格同图到达时旧审批已不再可替换,保留旧版本"
                    );
                    cleanup(files);
                    return Ok(());
                }
                tracing::info!(
                    id = %item.source_id,
                    replaced_source = old.source.as_str(),
                    replaced_id = %old.source_id,
                    "严格同图自动去重,用高清版本替换旧审批"
                );
            }
            ExactAction::None => {}
        }

        let dropped: HashSet<usize> = evaluation.drop_current_indices.iter().copied().collect();
        let originals: Vec<PathBuf> = files
            .iter()
            .enumerate()
            .filter(|(index, _)| !dropped.contains(index))
            .map(|(_, path)| path.clone())
            .collect();
        let kept_fingerprints: Vec<ImageFingerprint> = fingerprints
            .into_iter()
            .enumerate()
            .filter(|(index, _)| !dropped.contains(index))
            .map(|(_, fingerprint)| fingerprint)
            .collect();
        if originals.is_empty() {
            tracing::info!(id = %item.source_id, "作品内图片均为严格同图,自动跳过审批");
            cleanup(files);
            return Ok(());
        }
        for index in &evaluation.drop_current_indices {
            if let Some(path) = files.get(*index) {
                let _ = std::fs::remove_file(path);
            }
        }

        let mut effective_item = item.clone();
        if effective_item.images.len() == files.len() {
            effective_item.images = effective_item
                .images
                .into_iter()
                .enumerate()
                .filter(|(index, _)| !dropped.contains(index))
                .map(|(_, image)| image)
                .collect();
            effective_item.page_count = effective_item.images.len() as u32;
        }
        let caption = format!(
            "{}{}",
            render_caption(&effective_item),
            render_review_notice(&evaluation.similar)
        );
        let files_owned = originals.clone();
        // 原始(缩放前)路径:审批通过后发原画质 document 进频道帖评论区(发 photo 用缩放版)。
        // 全套图缩放(CPU 阻塞,放 blocking 线程);审批需要看到全部,批准后直接复用。
        let prepared = tokio::task::spawn_blocking(move || prepare_all(&files_owned)).await??;

        let token = self.state.next_token();
        let keyboard = if self.state.gallery.is_some() {
            InlineKeyboardMarkup::new(vec![
                vec![
                    InlineKeyboardButton::callback("✅ 发送到频道", format!("ok:{token}")),
                    InlineKeyboardButton::callback("📦 发送并入库", format!("ok_lib:{token}")),
                ],
                vec![InlineKeyboardButton::callback(
                    "❌ 丢弃",
                    format!("no:{token}"),
                )],
            ])
        } else {
            InlineKeyboardMarkup::new(vec![vec![
                InlineKeyboardButton::callback("✅ 发送到频道", format!("ok:{token}")),
                InlineKeyboardButton::callback("❌ 丢弃", format!("no:{token}")),
            ]])
        };

        let n = prepared.len();
        let bot = &self.state.bot;
        let chat = self.state.review_chat.clone();
        let mut review_ids: Vec<MessageId> = Vec::new();
        // 缩放后仍超 Telegram photo 硬上限:审批消息也退化为 document,否则 sendPhoto 直接
        // 报错、投递卡死重试(批准后 send_group 复用同一批 prepared 文件,同样会退化,一致)。
        let oversized = any_oversized_for_photo(&prepared);

        let send_result: Result<()> = async {
            if n == 1 {
                let msg = if oversized {
                    tg_retry(|| {
                        bot.send_document(chat.clone(), InputFile::file(&prepared[0]))
                            .caption(format!("【待审】\n{caption}"))
                            .parse_mode(ParseMode::Html)
                            .reply_markup(keyboard.clone())
                    })
                    .await?
                } else {
                    tg_retry(|| {
                        bot.send_photo(chat.clone(), InputFile::file(&prepared[0]))
                            .caption(format!("【待审】\n{caption}"))
                            .parse_mode(ParseMode::Html)
                            .reply_markup(keyboard.clone())
                    })
                    .await?
                };
                review_ids.push(msg.id);
            } else {
                // 与发布侧同一套分批逻辑:sendMediaGroup 硬限 10,超 10 张的作品不分批
                // 必收 400 → 不入库 → 每轮重下重发的死循环。
                let first_cap = format!("【待审 · 共 {n} 张】\n{caption}");
                let mode = if oversized {
                    MediaMode::Document
                } else {
                    MediaMode::Photo { spoiler: false }
                };
                send_batches(
                    bot,
                    &chat,
                    &prepared,
                    &first_cap,
                    &mode,
                    &mut review_ids,
                    &mut || {},
                )
                .await?;
                let ctrl = tg_retry(|| {
                    bot.send_message(chat.clone(), format!("👆 上面 {n} 张,请审批"))
                        .reply_markup(keyboard.clone())
                })
                .await?;
                review_ids.push(ctrl.id);
            }
            Ok(())
        }
        .await;

        // 持久化 pending(发送成功后才写,保证按钮一定对得上)。
        let insert_result: Result<()> = if send_result.is_ok() {
            async {
                let files_str: Vec<String> = prepared
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                let msg_ids: Vec<i32> = review_ids.iter().map(|m| m.0).collect();
                let originals_str: Vec<String> = originals
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                let files_json = serde_json::to_string(&files_str)?;
                let msg_json = serde_json::to_string(&msg_ids)?;
                let originals_json = serde_json::to_string(&originals_str)?;
                let item_meta = serde_json::to_string(&effective_item)?;
                let db = self.state.db.lock().await;
                let tx = db.unchecked_transaction()?;
                tx.execute(
                    "INSERT OR REPLACE INTO pending(token, files, caption, msg_ids, originals, created_at, state, is_r18, item_meta) VALUES(?1,?2,?3,?4,?5,?6,'pending',?7,?8)",
                    rusqlite::params![token as i64, files_json, caption, msg_json, originals_json, now_secs(), effective_item.is_r18, item_meta],
                )?;
                record_work(
                    &tx,
                    &effective_item,
                    &kept_fingerprints,
                    WorkStatus::Pending,
                )?;
                tx.commit()?;
                Ok(())
            }
            .await
        } else {
            Ok(())
        };

        // 发送或写库任一步失败:补偿删除已发出的消息,否则留下无 pending 记录的
        // 孤儿审批组(按钮永远"该条已失效"),且 pipeline 不 mark_pushed、下轮重投
        // 同一作品,私聊出现重复。
        if let Err(e) = send_result.and(insert_result) {
            // 补偿也带限流重试:触发补偿的最常见原因正是严重限流(tg_retry 5 次耗尽),
            // 裸调用几乎必然再吃 429,补偿等于没做。
            for mid in &review_ids {
                let _ = tg_retry(|| bot.delete_message(chat.clone(), *mid)).await;
            }
            return Err(e);
        }
        Ok(())
    }
}

struct PendingWork {
    files: Vec<PathBuf>,
    caption: String,
    msg_ids: Vec<i32>,
    originals: Vec<PathBuf>,
    is_r18: bool,
    item_meta: String,
}

struct FinishOutcome {
    archive_failed: bool,
}

fn decode_pending(row: PendingRow) -> Result<PendingWork> {
    let (files_json, caption, msg_json, originals_json, is_r18, item_meta) = row;
    let files = serde_json::from_str::<Vec<String>>(&files_json)?
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let originals = serde_json::from_str::<Vec<String>>(&originals_json)
        .unwrap_or_default()
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let msg_ids = serde_json::from_str(&msg_json)?;
    Ok(PendingWork {
        files,
        caption,
        msg_ids,
        originals,
        is_r18,
        item_meta,
    })
}

/// 完成一条已抢占的审批。发布/解析失败会恢复为 pending，供按钮或下一次
/// `/approve` 重试；成功后删除审批消息、pending 记录并接续原图评论任务。
/// `archive=true` 时在发布成功后额外入库 Vitrine。
async fn finish_claimed(
    state: &Arc<ReviewState>,
    token: i64,
    row: PendingRow,
    publish: bool,
    archive: bool,
) -> Result<FinishOutcome> {
    let action_row = row.clone();
    let work = match decode_pending(row) {
        Ok(work) => work,
        Err(e) => {
            let db = state.db.lock().await;
            let _ = restore_pending(&db, token);
            return Err(e);
        }
    };
    let archive_staged = if publish && archive {
        stage_pending_archive(state, &work)
    } else {
        None
    };
    let mut sent: Vec<MessageId> = Vec::new();
    let mut activate_on_first = || {
        if archive_staged.is_some() {
            activate_pending_archive(state, &work);
        }
    };
    let send_result = if publish {
        send_group(
            &state.bot,
            &state.publish_channel,
            &work.files,
            &work.caption,
            work.is_r18, // 结构化字段决定剧透遮罩,不再从 caption 文本反推
            &mut sent,
            &mut activate_on_first,
        )
        .await
    } else {
        Ok(())
    };
    let first_id = sent.first().copied();
    if let Err(e) = send_result {
        if first_id.is_none() {
            if archive_staged.is_some() {
                resolve_pending_archive(state, &work);
            }
            // 一张都没发出:恢复 pending,原审批按钮/下次 /approve 可安全重试。
            let restore = {
                let db = state.db.lock().await;
                restore_pending(&db, token)
            };
            if let Err(restore_error) = restore {
                tracing::error!(error = %restore_error, token, "审批发布失败后恢复待审状态失败");
            }
            return Err(e);
        }
        // 部分批次已进频道:恢复重试会从第一批重发造成频道重复,按已发布收尾并转人工。
        tracing::warn!(error = %e, token, "发布部分成功,后续批次失败,按已发布收尾");
        let _ = state
            .bot
            .send_message(
                state.review_chat.clone(),
                "⚠️ 该条仅部分图片发布成功(后续批次失败),已按已发布处理;请检查频道帖,必要时手动补发缺图",
            )
        .await;
    }

    // 频道帖发出后立即登记评论任务。Telegram 通常会在约 1 秒内把频道帖
    // auto-forward 到讨论组；图库补偿副本已先独立落盘，所以评论任务可以安全清理原目录。
    let registered = match (publish, first_id) {
        (true, Some(mid)) if !work.originals.is_empty() => {
            register_comment(state, mid.0, &work.originals).await;
            true
        }
        _ => false,
    };

    // 发布成功(或至少部分成功)且需要入库
    let mut archive_failed = false;
    if publish && first_id.is_some() && archive {
        match archive_pending_work(state, &work, archive_staged.as_ref()).await {
            Ok(()) => {
                if archive_staged.is_some() {
                    resolve_pending_archive(state, &work);
                }
            }
            Err(e) => {
                archive_failed = true;
                let queued = queue_pending_archive(state, &work, &e);
                tracing::warn!(error = %e, token, queued, "审批发布后图库入库失败");
                let suffix = if queued && e.is_retryable() {
                    "；已加入自动补偿队列"
                } else if queued {
                    "；已保留持久副本并停止自动重试，请人工检查"
                } else {
                    "；补偿队列登记失败，请人工处理"
                };
                let _ = state
                    .bot
                    .send_message(
                        state.review_chat.clone(),
                        format!("⚠️ 频道已发,但图库入库失败: {e}{suffix}"),
                    )
                    .await;
            }
        }
    }

    // pending 删除与“最近一次动作”登记放在同一事务。丢弃时保留完整行和文件，
    // 让 /undo 能重新生成审批卡片；发布动作只登记类型，用于阻止误撤销更早丢弃。
    let action = if !publish {
        "discard"
    } else if archive && !archive_failed {
        "publish_archive"
    } else {
        "publish"
    };
    let replaced_discards = {
        let mut db = state.db.lock().await;
        complete_and_record_action(
            &mut db,
            token,
            action,
            if publish { None } else { Some(&action_row) },
        )
    };
    let replaced_discards = match replaced_discards {
        Ok(paths) => paths,
        Err(e) => {
            if !publish {
                let db = state.db.lock().await;
                let _ = restore_pending(&db, token);
            }
            return Err(e).context("登记最近审批动作失败");
        }
    };
    if let Ok(item) = serde_json::from_str::<MediaItem>(&work.item_meta) {
        let db = state.db.lock().await;
        let catalog_result = if publish {
            mark_work_status(&db, &item, WorkStatus::Published)
        } else {
            remove_work(&db, &item)
        };
        if let Err(error) = catalog_result {
            tracing::warn!(error = %error, token, "更新图片去重状态失败");
        }
    }

    for mid in &work.msg_ids {
        let _ = state
            .bot
            .delete_message(state.review_chat.clone(), MessageId(*mid))
            .await;
    }
    // 新动作覆盖旧的可撤销丢弃后，旧文件不再有恢复价值；事务提交后再清理。
    for paths in replaced_discards {
        cleanup(&paths);
    }
    // 当前丢弃的文件由 review_actions 持有；发布文件仍按原评论任务语义清理。
    if publish && !registered {
        cleanup(&work.files);
    }
    Ok(FinishOutcome { archive_failed })
}

async fn archive_pending_work(
    state: &Arc<ReviewState>,
    work: &PendingWork,
    staged: Option<&QueuedGalleryWork>,
) -> std::result::Result<(), GalleryIngestError> {
    let gallery = state
        .gallery
        .as_ref()
        .ok_or_else(|| GalleryIngestError::permanent("图库未配置"))?;
    let item: MediaItem = serde_json::from_str(&work.item_meta).map_err(|error| {
        GalleryIngestError::permanent(format!(
            "pending item_meta 无法解析为 MediaItem(旧审批请丢弃重抓): {error}"
        ))
    })?;
    // staging 是独立持久副本，不受 pending/评论临时目录清理影响。
    let paths = staged.map_or_else(
        || {
            if !work.originals.is_empty() {
                work.originals.as_slice()
            } else {
                work.files.as_slice()
            }
        },
        |queued| queued.files.as_slice(),
    );
    gallery.ingest(&item, paths).await
}

fn pending_archive_item_paths(work: &PendingWork) -> Option<(MediaItem, &[PathBuf])> {
    let item = serde_json::from_str::<MediaItem>(&work.item_meta).ok()?;
    let paths = if !work.originals.is_empty() {
        work.originals.as_slice()
    } else {
        work.files.as_slice()
    };
    Some((item, paths))
}

fn stage_pending_archive(state: &ReviewState, work: &PendingWork) -> Option<QueuedGalleryWork> {
    let (item, paths) = pending_archive_item_paths(work)?;
    stage_gallery_work(state.gallery_outbox.as_ref(), &item, paths)
}

fn queue_pending_archive(
    state: &ReviewState,
    work: &PendingWork,
    error: &GalleryIngestError,
) -> bool {
    let (Some(outbox), Ok(item)) = (
        state.gallery_outbox.as_ref(),
        serde_json::from_str::<MediaItem>(&work.item_meta),
    ) else {
        return false;
    };
    let paths = if !work.originals.is_empty() {
        work.originals.as_slice()
    } else {
        work.files.as_slice()
    };
    enqueue_gallery_failure(Some(outbox), &item, paths, error)
}

fn activate_pending_archive(state: &ReviewState, work: &PendingWork) {
    let Ok(item) = serde_json::from_str::<MediaItem>(&work.item_meta) else {
        return;
    };
    activate_gallery_staging(
        state.gallery_outbox.as_ref(),
        item.source.as_str(),
        &item.source_id,
    );
}

fn resolve_pending_archive(state: &ReviewState, work: &PendingWork) {
    let Ok(item) = serde_json::from_str::<MediaItem>(&work.item_meta) else {
        return;
    };
    resolve_gallery_staging(
        state.gallery_outbox.as_ref(),
        item.source.as_str(),
        &item.source_id,
    );
}

fn enqueue_gallery_failure(
    outbox: Option<&GalleryOutbox>,
    item: &MediaItem,
    paths: &[PathBuf],
    error: &GalleryIngestError,
) -> bool {
    let Some(outbox) = outbox else {
        return false;
    };
    match outbox.enqueue_classified(item, paths, &error.to_string(), error.is_retryable()) {
        Ok(_) => true,
        Err(queue_error) => {
            tracing::error!(
                source = item.source.as_str(),
                id = %item.source_id,
                error = %queue_error,
                "图库补偿队列登记失败"
            );
            false
        }
    }
}

fn stage_gallery_work(
    outbox: Option<&GalleryOutbox>,
    item: &MediaItem,
    paths: &[PathBuf],
) -> Option<QueuedGalleryWork> {
    let outbox = outbox?;
    match outbox.stage(item, paths) {
        Ok(queued) => Some(queued),
        Err(error) => {
            tracing::error!(
                source = item.source.as_str(),
                id = %item.source_id,
                error = %error,
                "图库补偿 staging 登记失败"
            );
            None
        }
    }
}

fn activate_gallery_staging(outbox: Option<&GalleryOutbox>, source_kind: &str, source_id: &str) {
    let Some(outbox) = outbox else {
        return;
    };
    match outbox.activate(source_kind, source_id) {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            source = source_kind,
            id = source_id,
            "频道已发布但图库补偿 staging 未激活"
        ),
        Err(error) => tracing::error!(
            source = source_kind,
            id = source_id,
            error = %error,
            "频道已发布但激活图库补偿 staging 失败"
        ),
    }
}

fn resolve_gallery_staging(outbox: Option<&GalleryOutbox>, source_kind: &str, source_id: &str) {
    let Some(outbox) = outbox else {
        return;
    };
    if let Err(error) = outbox.resolve_without_retry(source_kind, source_id) {
        tracing::warn!(
            source = source_kind,
            id = source_id,
            error = %error,
            "初次图库入库成功但清理补偿 staging 失败"
        );
    }
}

/// 顺序处理 `/approve` 或 `/approve_archive` 原子抢占到的全部记录。顺序发送避免
/// 瞬间把大量图组并发压给 Telegram；单条失败不阻断后续项，失败记录恢复为 pending。
async fn publish_all_claimed(
    state: Arc<ReviewState>,
    claimed: Vec<(i64, PendingRow)>,
    archive: bool,
) {
    let total = claimed.len();
    let mut succeeded = 0usize;
    let mut archive_failed = 0usize;
    for (token, row) in claimed {
        match finish_claimed(&state, token, row, true, archive).await {
            Ok(outcome) => {
                succeeded += 1;
                if outcome.archive_failed {
                    archive_failed += 1;
                }
            }
            Err(e) => tracing::warn!(error = %e, token, "一键批准中的单条发布失败"),
        }
    }
    let failed = total - succeeded;
    let summary = if archive && failed == 0 && archive_failed == 0 {
        format!("✅ 一键批准并入库完成：已发布并入库 {succeeded} 条")
    } else if archive {
        format!(
            "⚠️ 一键批准并入库完成：发布成功 {succeeded} 条，发布失败 {failed} 条，入库失败 {archive_failed} 条；发布失败项仍保留待审，可再次 /approve_archive"
        )
    } else if failed == 0 {
        format!("✅ 一键批准完成：已发布 {succeeded} 条")
    } else {
        format!("⚠️ 一键批准完成：成功 {succeeded} 条，失败 {failed} 条；失败项仍保留待审，可再次 /approve")
    };
    let _ = state
        .bot
        .send_message(state.review_chat.clone(), summary)
        .await;
}

/// callback 轮询:监听按钮点击与 `/` 命令/链接。批准 → 发频道 + 删私聊整组;
/// 拒绝 → 删私聊整组。失败(如限流)保留 pending 供重点。与抓取循环并发运行。
///
/// 长轮询(timeout=25):get_updates 挂起等待事件,有按钮/命令立即返回 → 即时响应,
/// 没有空轮询的固定延迟。直连 Telegram 时用此;经代理若被掐断会走 Err 分支重试。
pub async fn run_review_loop(
    state: Arc<ReviewState>,
    trigger: tokio::sync::mpsc::Sender<()>,
    link: tokio::sync::mpsc::Sender<LinkJob>,
) {
    // 补偿线程只消费 gallery_outbox，不触碰 Telegram 发布和 pushed 去重。
    if let (Some(outbox), Some(gallery)) = (state.gallery_outbox.clone(), state.gallery.clone()) {
        tokio::spawn(crate::gallery_outbox::run_retry_loop(outbox, gallery));
    }
    // 启动先清一次超期/孤儿(顺手清掉旧版本遗留的临时图)。
    cleanup_stale(&state).await;
    if let Err(e) = state
        .bot
        .set_my_commands(command_menu(state.gallery.is_some()))
        .await
    {
        tracing::warn!(error = %e, "注册 Telegram 命令菜单失败");
    }
    let mut last_cleanup = now_secs();
    const CLEANUP_INTERVAL_SECS: i64 = 6 * 3600;

    let mut offset: i32 = 0;
    loop {
        // 周期清理:常驻不重启实例也能清过期 pending 与孤儿临时目录。
        if cleanup_due(last_cleanup, now_secs(), CLEANUP_INTERVAL_SECS) {
            cleanup_stale(&state).await;
            last_cleanup = now_secs();
        }
        // 兜底:清理超时仍未等到 auto-forward 的评论任务(频道没绑讨论组/转发丢失)。
        sweep_expired_comments(&state).await;
        let updates = state
            .bot
            .get_updates()
            .offset(offset)
            .timeout(25)
            .allowed_updates(vec![AllowedUpdate::CallbackQuery, AllowedUpdate::Message])
            .await;
        match updates {
            Ok(list) => {
                for u in list {
                    offset = u.id.0 as i32 + 1;
                    match u.kind {
                        UpdateKind::CallbackQuery(q) => {
                            if let Err(e) = handle_callback(&state, q).await {
                                tracing::warn!(error = %e, "处理审批回调失败");
                            }
                        }
                        UpdateKind::Message(msg) => {
                            // 讨论组里的频道帖 auto-forward → 投递原图到评论区,不当命令处理。
                            if let Some(chan_msg_id) =
                                match_auto_forward(&msg, &state.publish_channel)
                            {
                                // 原图上传(几十 MB、分批、限流可再等数十秒)移出轮询
                                // 循环:内联 await 会让按钮 3 秒内无法应答,阻塞超
                                // 120s 时 sweep 还会把未消费的评论任务清掉。
                                let st = state.clone();
                                tokio::spawn(async move {
                                    deliver_comment(&st, &msg, chan_msg_id).await;
                                });
                            } else if let Err(e) =
                                handle_command(&state, &msg, &trigger, &link).await
                            {
                                tracing::warn!(error = %e, "处理命令失败");
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                let s = e.to_string();
                if s.contains("Conflict") || s.contains("terminated by other") {
                    tracing::error!("检测到另一个 bot 实例在抢 getUpdates,请确保只运行一个 hanabi");
                } else {
                    tracing::warn!(error = %e, "get_updates 失败");
                }
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        }
    }
}

/// 处理 `/` 命令(仅文本消息)。**仅响应审批私聊本人**(owner),陌生人忽略。
/// /run 触发抓取；/approve 批准全部剩余待审；/approve_archive 批准并入库；
/// /undo 撤销最近一次误丢弃并重新生成审批卡片；
/// /status /ping /help 即时回复；
/// 非命令的 Pixiv/X 链接交抓取循环直发频道。
async fn handle_command(
    state: &Arc<ReviewState>,
    msg: &Message,
    trigger: &tokio::sync::mpsc::Sender<()>,
    link: &tokio::sync::mpsc::Sender<LinkJob>,
) -> Result<()> {
    // 权限校验:只有审批私聊本人能发命令/链接,其他人一律忽略。
    if msg.chat.id.0 != state.owner {
        return Ok(());
    }

    let text = msg.text().unwrap_or("").trim();
    // 非命令:识别 Pixiv/X 作品链接 → 交抓取循环直发频道(跳过审批)。
    // 记下用户链接消息 id 与"抓取中"提示 id,发布成功后一并删除,保持私聊干净。
    if !text.starts_with('/') {
        if let Some(url) = extract_supported_url(text) {
            let notice = state
                .bot
                .send_message(msg.chat.id, "🔗 收到链接,抓取中…")
                .await?;
            // try_send:与 /run 同理,抓取执行期间主循环不消费 link_rx(容量 16),
            // 阻塞式 send 在通道满时会把整个 review loop 挂死。
            if link
                .try_send(LinkJob {
                    url,
                    user_msg_id: msg.id.0,
                    notice_msg_id: notice.id.0,
                })
                .is_err()
            {
                let _ = state
                    .bot
                    .edit_message_text(
                        msg.chat.id,
                        notice.id,
                        "⏳ 链接队列已满,请稍后重发".to_string(),
                    )
                    .await;
            }
        }
        return Ok(());
    }
    let cmd = text.split_whitespace().next().unwrap_or("");
    let cmd = cmd.split('@').next().unwrap_or(cmd);
    if cmd == "/undo" {
        let claim = {
            let mut db = state.db.lock().await;
            claim_latest_undo(&mut db)?
        };
        match claim {
            UndoClaim::Empty => {
                state
                    .bot
                    .send_message(msg.chat.id, "ℹ️ 当前没有可撤销的审批动作")
                    .await?;
            }
            UndoClaim::Irreversible(action) => {
                let label = match action.as_str() {
                    "publish_archive" => "发送并入库",
                    "publish" => "发送到频道",
                    _ => "已完成操作",
                };
                state
                    .bot
                    .send_message(
                        msg.chat.id,
                        format!(
                            "ℹ️ 最近一次动作是“{label}”，已产生外部发布；/undo 仅恢复最近一次误丢弃"
                        ),
                    )
                    .await?;
            }
            UndoClaim::Claimed(action) => {
                let action_id = action.id;
                let decoded = decode_pending(action.row);
                let result: Result<()> = async {
                    let work = decoded?;
                    let item: MediaItem = serde_json::from_str(&work.item_meta)
                        .context("最近丢弃记录的作品元数据无效")?;
                    let originals = if work.originals.is_empty() {
                        work.files
                    } else {
                        work.originals
                    };
                    if originals.is_empty() || originals.iter().any(|p| !p.is_file()) {
                        anyhow::bail!("最近丢弃记录的图片文件已过期");
                    }
                    TelegramSink {
                        state: state.clone(),
                    }
                    .deliver(&item, &originals)
                    .await
                }
                .await;
                match result {
                    Ok(()) => {
                        let removed = {
                            let db = state.db.lock().await;
                            finish_undo(&db, action_id)?
                        };
                        if removed != 1 {
                            tracing::error!(action_id, "撤销成功后清理动作记录失败");
                        }
                        state
                            .bot
                            .send_message(msg.chat.id, "↩️ 已撤销最近一次丢弃，审批卡片已恢复")
                            .await?;
                    }
                    Err(e) => {
                        let db = state.db.lock().await;
                        let _ = restore_undo(&db, action_id);
                        drop(db);
                        tracing::warn!(error = %e, action_id, "撤销最近丢弃失败");
                        state
                            .bot
                            .send_message(
                                msg.chat.id,
                                format!("⚠️ 撤销失败，记录已保留可重试：{e}"),
                            )
                            .await?;
                    }
                }
            }
        }
        return Ok(());
    }
    if cmd == "/approve" || cmd == "/approve_archive" {
        let archive = cmd == "/approve_archive";
        if archive && state.gallery.is_none() {
            state
                .bot
                .send_message(msg.chat.id, "⚠️ 图库未配置，未执行一键批准并入库")
                .await?;
            return Ok(());
        }
        let claimed = {
            let mut db = state.db.lock().await;
            claim_all_pending(&mut db)?
        };
        if claimed.is_empty() {
            state
                .bot
                .send_message(msg.chat.id, "✅ 当前没有剩余待审项")
                .await?;
        } else {
            let count = claimed.len();
            let tokens: Vec<i64> = claimed.iter().map(|(t, _)| *t).collect();
            let task_state = state.clone();
            tokio::spawn(async move {
                // JoinError 兜底:批量任务 panic 时把仍卡在 publishing 的记录恢复为
                // pending(restore 的条件 UPDATE 保证已完成/已恢复的不受影响)。
                let inner = tokio::spawn(publish_all_claimed(task_state.clone(), claimed, archive));
                if let Err(join_err) = inner.await {
                    tracing::error!(error = %join_err, "一键批准任务异常退出,恢复未完成项为待审");
                    let db = task_state.db.lock().await;
                    for t in tokens {
                        let _ = restore_pending(&db, t);
                    }
                }
            });
            state
                .bot
                .send_message(
                    msg.chat.id,
                    if archive {
                        format!("⏳ 正在一键批准、发布并入库 {count} 条…")
                    } else {
                        format!("⏳ 正在一键批准并发布 {count} 条…")
                    },
                )
                .await?;
        }
        return Ok(());
    }
    let reply: String = match cmd {
        // try_send:通道满(容量 8)时阻塞式 send 会把整个 review loop 挂死在这里
        // (抓取执行期间主循环不消费 trigger),按钮/命令全部无响应。
        "/run" => match trigger.try_send(()) {
            Ok(()) => "🚀 开始手动抓取一轮,有命中会发审批消息过来".to_string(),
            Err(_) => "⏳ 抓取已在排队/进行中,请稍候".to_string(),
        },
        "/status" => {
            let count: i64 = {
                let db = state.db.lock().await;
                db.query_row("SELECT COUNT(*) FROM pending", [], |r| r.get(0))
                    .unwrap_or(0)
            };
            format!("✅ 运行中\n待审: {count} 条")
        }
        "/ping" => "pong 🏓".to_string(),
        "/help" => {
            let archive_help = if state.gallery.is_some() {
                "\n/approve_archive — 批准、发布并入库全部剩余待审"
            } else {
                ""
            };
            format!(
                "命令列表:\n/run — 立即抓取一轮\n/approve — 批准并发布全部剩余待审(仅频道,不入库){archive_help}\n/undo — 撤销最近一次误丢弃并恢复审批卡片\n/status — 待审数+运行状态\n/ping — 存活测试\n/help — 本帮助\n\n💡 审批按钮:✅ 发送到频道 / 📦 发送并入库 / ❌ 丢弃\n💡 误点丢弃后立即发 /undo 可恢复；若最近动作已是发布，则不会误撤销更早的丢弃\n💡 先逐条点 ❌ 丢弃不想推送的图片，再发 /approve 或 /approve_archive 一键处理其余图片\n💡 直接发 Pixiv/X 作品链接 → 自动抓取并发布到频道(配置图库后同步入库)"
            )
        }
        _ => return Ok(()),
    };
    state.bot.send_message(msg.chat.id, reply).await?;
    Ok(())
}

/// 链接粒度:单作品(直发频道) / 多作品(主页/榜单/list,走审批流)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    Single,
    Multi,
}

/// 取 http(s) URL 的 host(小写)。无 scheme 或畸形返回 None。
fn url_host(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.split('@').next_back().unwrap_or(host); // 去掉 userinfo
    let host = host.split(':').next().unwrap_or(host); // 去掉端口
    if host.is_empty() {
        None
    } else {
        Some(host.to_lowercase())
    }
}

/// host 是否属于受支持站点(精确后缀匹配, 防 evil.com/pixiv.net 子串伪装)。
fn supported_host(url: &str) -> bool {
    matches!(url_host(url), Some(h)
        if h == "pixiv.net" || h.ends_with(".pixiv.net")
        || h == "x.com" || h.ends_with(".x.com")
        || h == "twitter.com" || h.ends_with(".twitter.com"))
}

/// 受支持站点的单/多作品分类;非受支持站点返回 None。
pub fn classify_link(url: &str) -> Option<LinkKind> {
    if !supported_host(url) {
        return None;
    }
    let is_single = url.contains("/artworks/") // pixiv 单作品
        || url.contains("/status/") // x/twitter 单推
        || url.contains("pixiv.net/i/"); // pixiv 短链单作品
    Some(if is_single {
        LinkKind::Single
    } else {
        LinkKind::Multi
    })
}

/// 从消息文本中提取首个受支持作品链接(host 精确判定)。pixiv/x/抖音。
fn extract_supported_url(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|w| {
            w.starts_with("http")
                && (classify_link(w).is_some() || crate::source::douyin::is_douyin_url(w))
        })
        .map(|s| s.to_string())
}

async fn handle_callback(state: &Arc<ReviewState>, q: CallbackQuery) -> Result<()> {
    // 权限校验:仅审批人本人可操作按钮(纵深防御,防 review_chat 被误配成可多人
    // 触达的会话时他人代批直发频道)。只在 owner 为正(私聊形态,chat_id==user_id,
    // 二者同值可比)时启用;owner 为负(群组 id)或 0(非数字降级形态)时用户 id
    // 与 owner 不可比,跳过以维持旧行为——群组形态的权限语义由部署者自担。
    if state.owner > 0 && q.from.id.0 as i64 != state.owner {
        let _ = state.bot.answer_callback_query(q.id).text("无权限").await;
        return Ok(());
    }
    let data = q.data.clone().unwrap_or_default();
    if let Some((token, decision)) = parse_similar_callback(&data) {
        return handle_similar_callback(state, q, token, decision).await;
    }
    let (action, token_str) = data.split_once(':').unwrap_or(("", ""));
    let token: i64 = token_str.parse().unwrap_or(-1);

    if action != "ok" && action != "ok_lib" && action != "no" {
        let _ = state
            .bot
            .answer_callback_query(q.id)
            .text("无效审批操作")
            .await;
        return Ok(());
    }

    // 先以条件 UPDATE 原子抢占 pending→publishing。同一 token 的第二次点击只能看到
    // Publishing，不会再启动一个 send_group。
    let claim = {
        let db = state.db.lock().await;
        claim_pending(&db, token)?
    };
    let row = match claim {
        PendingClaim::Claimed(row) => row,
        PendingClaim::Missing => {
            let _ = state
                .bot
                .answer_callback_query(q.id)
                .text("该条已失效")
                .await;
            return Ok(());
        }
        PendingClaim::Publishing => {
            let _ = state
                .bot
                .answer_callback_query(q.id)
                .text("⏳ 已在发布中…")
                .await;
            return Ok(());
        }
    };
    let publish = action == "ok" || action == "ok_lib";
    let archive = action == "ok_lib";

    // 立即应答,停止按钮转圈(必须 3 秒内,否则 callback query 过期)。
    // 发图/删消息这些耗时操作放后台,不让你盯着转圈等上传。
    let _ = state
        .bot
        .answer_callback_query(q.id)
        .text(if archive {
            "⏳ 发布并入库中…"
        } else if publish {
            "⏳ 发布中…"
        } else {
            "❌ 已丢弃"
        })
        .await;

    // 后台执行:批准→发频道(+可选入库);然后删私聊整组 + 清文件 + 删 pending。
    // 失败(如限流)恢复 pending,发提示可重试。内层再包一个 task 检查 JoinError:
    // finish_claimed panic 时该 token 会卡死在 publishing(按钮只回"已在发布中"、
    // /approve 也不选它,直到重启),此处兜底恢复为 pending。
    let state = state.clone();
    tokio::spawn(async move {
        let inner = tokio::spawn({
            let state = state.clone();
            async move { finish_claimed(&state, token, row, publish, archive).await }
        });
        match inner.await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                tracing::warn!(error = %e, token, "审批操作失败,pending 保留可重试");
                if publish {
                    let _ = state
                        .bot
                        .send_message(
                            state.review_chat.clone(),
                            "⚠️ 发布失败(可能限流),过会儿再点一次那条审批",
                        )
                        .await;
                }
            }
            Err(join_err) => {
                tracing::error!(error = %join_err, token, "审批任务异常退出,恢复待审状态");
                let db = state.db.lock().await;
                let _ = restore_pending(&db, token);
            }
        }
    });
    Ok(())
}

fn similar_review_text(token: i64, group: &SimilarReviewGroup) -> String {
    format!(
        "🔎 相似图审批 #{token}\n👆 上面 {} 张，请审批",
        group.images.len()
    )
}

fn similar_review_keyboard(token: i64, count: usize) -> InlineKeyboardMarkup {
    let mut rows = vec![vec![InlineKeyboardButton::callback(
        "✅ 全部保留",
        format!("similar:{token}:all"),
    )]];
    for index in 1..=count {
        rows.push(vec![InlineKeyboardButton::callback(
            format!("只保留 #{index}"),
            format!("similar:{token}:keep:{index}"),
        )]);
    }
    InlineKeyboardMarkup::new(rows)
}

fn similar_confirm_keyboard(token: i64, index: usize) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(
            format!("⚠️ 确认仅保留 #{index}"),
            format!("similar:{token}:confirm:{index}"),
        )],
        vec![InlineKeyboardButton::callback(
            "↩️ 返回",
            format!("similar:{token}:cancel"),
        )],
    ])
}

fn remove_pruned_fingerprints(
    conn: &rusqlite::Connection,
    group: &SimilarReviewGroup,
    keep_index: usize,
) -> Result<()> {
    for (index, image) in group.images.iter().enumerate() {
        if index + 1 == keep_index {
            continue;
        }
        let Some((work_id, image_index)) = image.image_id.rsplit_once('#') else {
            continue;
        };
        let Some((source, source_id)) = work_id.split_once(':') else {
            continue;
        };
        let Ok(image_index) = image_index.parse::<i64>() else {
            continue;
        };
        conn.execute(
            "DELETE FROM image_fingerprints WHERE source_kind=?1 AND source_id=?2 AND image_index=?3",
            rusqlite::params![source, source_id, image_index],
        )?;
    }
    Ok(())
}

async fn handle_similar_callback(
    state: &Arc<ReviewState>,
    q: CallbackQuery,
    token: i64,
    decision: SimilarDecision,
) -> Result<()> {
    match decision {
        SimilarDecision::SelectKeep(_) | SimilarDecision::Cancel => {
            let loaded = {
                let db = state.db.lock().await;
                let group = load_similar_review(&db, token)?;
                let messages = similar_review_messages(&db, token)?;
                group.zip(messages)
            };
            let Some((group, (_, Some(control_id)))) = loaded else {
                let _ = state
                    .bot
                    .answer_callback_query(q.id)
                    .text("该组已处理或失效")
                    .await;
                return Ok(());
            };
            if matches!(decision, SimilarDecision::SelectKeep(index) if index == 0 || index > group.images.len())
            {
                let _ = state
                    .bot
                    .answer_callback_query(q.id)
                    .text("无效图片序号")
                    .await;
                return Ok(());
            }
            let keyboard = match decision {
                SimilarDecision::SelectKeep(index) => similar_confirm_keyboard(token, index),
                SimilarDecision::Cancel => similar_review_keyboard(token, group.images.len()),
                _ => unreachable!(),
            };
            let _ = state.bot.answer_callback_query(q.id).await;
            state
                .bot
                .edit_message_text(
                    state.review_chat.clone(),
                    MessageId(control_id),
                    similar_review_text(token, &group),
                )
                .reply_markup(keyboard)
                .await?;
            return Ok(());
        }
        SimilarDecision::KeepAll | SimilarDecision::ConfirmKeep(_) => {}
    }

    let claimed = {
        let db = state.db.lock().await;
        let group = claim_similar_review(&db, token, decision)?;
        let messages = similar_review_messages(&db, token)?;
        group.zip(messages)
    };
    let Some((group, (_, control_id))) = claimed else {
        let _ = state
            .bot
            .answer_callback_query(q.id)
            .text("该组已处理或正在处理中")
            .await;
        return Ok(());
    };
    let _ = state
        .bot
        .answer_callback_query(q.id)
        .text(match decision {
            SimilarDecision::KeepAll => "✅ 已记录全部保留",
            SimilarDecision::ConfirmKeep(_) => "⏳ 正在备份并整理图库…",
            _ => unreachable!(),
        })
        .await;

    let state = state.clone();
    tokio::spawn(async move {
        let result: Result<String> = async {
            match decision {
                SimilarDecision::KeepAll => Ok("✅ 已审批：全部保留".into()),
                SimilarDecision::ConfirmKeep(index) => {
                    let keep = &group.images[index - 1];
                    let remove: Vec<String> = group
                        .images
                        .iter()
                        .enumerate()
                        .filter(|(position, _)| position + 1 != index)
                        .map(|(_, image)| image.r2_key.clone())
                        .collect();
                    let gallery = state
                        .gallery
                        .as_ref()
                        .context("Vitrine 未配置，不能执行相似图整理")?;
                    let removed = gallery
                        .prune_similar(&format!("hanabi-similar-{token}"), &keep.r2_key, &remove)
                        .await?;
                    let db = state.db.lock().await;
                    remove_pruned_fingerprints(&db, &group, index)?;
                    Ok(format!(
                        "✅ 已审批：仅保留 #{index}，已移除 {removed} 张（有回收备份）"
                    ))
                }
                _ => unreachable!(),
            }
        }
        .await;

        match result {
            Ok(text) => {
                {
                    let db = state.db.lock().await;
                    if let Err(error) = finish_similar_review(&db, token, decision) {
                        tracing::error!(token, error = %error, "写入相似图审批结果失败");
                        return;
                    }
                }
                if let Some(message_id) = control_id {
                    let _ = state
                        .bot
                        .edit_message_text(state.review_chat.clone(), MessageId(message_id), text)
                        .reply_markup(InlineKeyboardMarkup::new(
                            Vec::<Vec<InlineKeyboardButton>>::new(),
                        ))
                        .await;
                }
            }
            Err(error) => {
                tracing::warn!(token, error = %error, "相似图审批失败，恢复按钮");
                {
                    let db = state.db.lock().await;
                    let _ = restore_similar_review(&db, token);
                }
                let _ = state
                    .bot
                    .send_message(
                        state.review_chat.clone(),
                        format!("⚠️ 相似图审批 #{token} 失败，原图未删除，可重新操作"),
                    )
                    .await;
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similar_review_control_text_does_not_repeat_album_details() {
        let group = SimilarReviewGroup {
            group_key: "douyin-group".into(),
            images: vec![
                crate::similar_review::SimilarReviewImage {
                    image_id: "douyin:1#0".into(),
                    r2_key: "douyin/1/0.jpg".into(),
                    label: "douyin:1 p0 · 2844×1600 · 1.3 MiB · verty".into(),
                },
                crate::similar_review::SimilarReviewImage {
                    image_id: "douyin:1#1".into(),
                    r2_key: "douyin/1/1.jpg".into(),
                    label: "douyin:1 p1 · 2844×1600 · 1.6 MiB · verty".into(),
                },
            ],
        };

        let text = similar_review_text(74, &group);

        assert_eq!(text, "🔎 相似图审批 #74\n👆 上面 2 张，请审批");
        assert!(!text.contains("douyin:1"));
        assert!(!text.contains("请选择处理方式"));
    }

    #[test]
    fn similar_review_cleanup_includes_album_and_control_messages() {
        assert_eq!(
            similar_review_cleanup_message_ids(&[101, 102, 103], Some(104)),
            vec![101, 102, 103, 104]
        );
        assert_eq!(
            similar_review_cleanup_message_ids(&[201, 202], None),
            vec![201, 202]
        );
    }

    #[test]
    fn cleanup_due_after_interval() {
        assert!(!cleanup_due(1000, 1000 + 6 * 3600 - 1, 6 * 3600));
        assert!(cleanup_due(1000, 1000 + 6 * 3600, 6 * 3600));
    }

    #[test]
    fn any_oversized_detects_file_over_photo_hard_limit() {
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("small.bin");
        let big = dir.path().join("big.bin");
        std::fs::write(&small, vec![0u8; 1024]).unwrap();
        std::fs::write(
            &big,
            vec![0u8; crate::sink::PHOTO_HARD_LIMIT_BYTES as usize + 1],
        )
        .unwrap();

        assert!(!any_oversized_for_photo(std::slice::from_ref(&small)));
        assert!(any_oversized_for_photo(&[small, big]));
    }

    #[test]
    fn build_documents_with_caption_only_tags_first() {
        let files = vec![PathBuf::from("a.png"), PathBuf::from("b.png")];
        let media = build_documents_with_caption(&files, "hello");
        assert_eq!(media.len(), 2);
        let InputMedia::Document(first) = &media[0] else {
            panic!("expected document")
        };
        let InputMedia::Document(second) = &media[1] else {
            panic!("expected document")
        };
        assert_eq!(first.caption.as_deref(), Some("hello"));
        assert_eq!(second.caption, None);
    }

    #[test]
    fn build_documents_with_caption_skips_empty_caption() {
        let files = vec![PathBuf::from("a.png")];
        let media = build_documents_with_caption(&files, "");
        let InputMedia::Document(first) = &media[0] else {
            panic!("expected document")
        };
        assert_eq!(first.caption, None);
    }

    #[test]
    fn prepare_preserves_png_alpha_when_downscaling() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("big.png");
        // 9000x2000 RGBA(宽+高>10000 触发缩放), 半透明像素。
        let mut img = image::RgbaImage::new(9000, 2000);
        for p in img.pixels_mut() {
            *p = image::Rgba([10, 20, 30, 128]);
        }
        img.save(&src).unwrap();

        let out = prepare(&src).unwrap();
        assert_ne!(out, src, "应产出缩放副本");
        assert_eq!(out.extension().unwrap(), "png", "应保留 png 而非转 jpg");
        let reloaded = image::open(&out).unwrap();
        assert!(reloaded.color().has_alpha(), "应保留 alpha 通道");
    }

    #[test]
    fn parse_owner_numeric_only() {
        assert_eq!(parse_owner("-1001234567890"), Some(-1001234567890));
        assert_eq!(parse_owner("@my_channel"), None);
        assert_eq!(parse_owner(""), None);
    }

    #[test]
    fn extract_url_recognizes_pixiv_and_x() {
        assert_eq!(
            extract_supported_url("https://www.pixiv.net/artworks/123").as_deref(),
            Some("https://www.pixiv.net/artworks/123")
        );
        assert_eq!(
            extract_supported_url("看这张 https://x.com/u/status/9 不错").as_deref(),
            Some("https://x.com/u/status/9")
        );
        assert_eq!(
            extract_supported_url("https://twitter.com/u/status/7").as_deref(),
            Some("https://twitter.com/u/status/7")
        );
    }

    #[test]
    fn extract_url_ignores_commands_and_other_links() {
        assert!(extract_supported_url("/run").is_none());
        assert!(extract_supported_url("https://example.com/a").is_none());
        assert!(extract_supported_url("随便聊聊").is_none());
    }

    #[test]
    fn pending_table_has_originals_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.db");
        // 模拟升级前仅有 created_at 的旧 pending 表，启动时必须无损补齐两列。
        let legacy = rusqlite::Connection::open(&path).unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE pending(
                    token INTEGER PRIMARY KEY,
                    files TEXT NOT NULL,
                    caption TEXT NOT NULL,
                    msg_ids TEXT NOT NULL,
                    created_at INTEGER NOT NULL DEFAULT 0
                );
                INSERT INTO pending(token, files, caption, msg_ids, created_at)
                VALUES(1, '[]', 'legacy', '[]', 1);
                INSERT INTO pending(token, files, caption, msg_ids, created_at)
                VALUES(2, '[]', '🔞 R18' || char(10) || 'Title: t', '[]', 1);",
            )
            .unwrap();
        drop(legacy);
        let _sink = TelegramSink::new(
            "123:abc".into(),
            "-1001234567890".into(),
            "@chan".into(),
            path.to_str().unwrap(),
            None,
        )
        .unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        let cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('pending')")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .flatten()
            .collect();
        assert!(cols.contains(&"originals".to_string()));
        assert!(cols.contains(&"state".to_string()));
        assert!(cols.contains(&"is_r18".to_string()));
        assert!(cols.contains(&"item_meta".to_string()));
        let image_table: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='image_fingerprints'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(image_table, 1);
        let state: String = conn
            .query_row("SELECT state FROM pending WHERE token=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(state, "pending");
        // 旧库回填:caption 含旧版 R18 标记的行 is_r18 置 1,普通行保持 0。
        let r18: i64 = conn
            .query_row("SELECT is_r18 FROM pending WHERE token=2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(r18, 1);
        let plain: i64 = conn
            .query_row("SELECT is_r18 FROM pending WHERE token=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(plain, 0);
    }

    #[test]
    fn existing_pending_media_is_backfilled_into_image_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let image_path = dir.path().join("pending.png");
        image::RgbImage::from_pixel(64, 48, image::Rgb([12, 34, 56]))
            .save(&image_path)
            .unwrap();
        let item = crate::model::MediaItem {
            source: crate::model::SourceKind::X,
            source_id: "backfill-1".into(),
            author: crate::model::Author {
                name: "画师".into(),
                url: "https://x.com/a".into(),
            },
            title: Some("旧待审".into()),
            url: "https://x.com/a/status/1".into(),
            tags: vec![],
            bookmark_count: None,
            is_r18: false,
            pixiv_type: None,
            page_count: 1,
            images: vec![crate::model::ImageRef {
                url: "https://example.test/1.png".into(),
                referer: None,
                fallback_urls: vec![],
            }],
            origin: "test".into(),
        };
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE pending(
                token INTEGER PRIMARY KEY,
                files TEXT NOT NULL,
                caption TEXT NOT NULL,
                msg_ids TEXT NOT NULL,
                originals TEXT NOT NULL DEFAULT '[]',
                created_at INTEGER NOT NULL DEFAULT 0,
                state TEXT NOT NULL DEFAULT 'pending',
                is_r18 INTEGER NOT NULL DEFAULT 0,
                item_meta TEXT NOT NULL DEFAULT '{}'
            );",
        )
        .unwrap();
        init_image_dedup_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO pending(token,files,caption,msg_ids,originals,created_at,state,is_r18,item_meta)
             VALUES(1,?1,'cap','[]',?1,1,'pending',0,?2)",
            rusqlite::params![
                serde_json::to_string(&vec![image_path.to_string_lossy().into_owned()]).unwrap(),
                serde_json::to_string(&item).unwrap()
            ],
        )
        .unwrap();

        assert_eq!(backfill_pending_image_catalog(&conn).unwrap(), 1);
        assert_eq!(backfill_pending_image_catalog(&conn).unwrap(), 0);
        let stored: i64 = conn
            .query_row("SELECT COUNT(*) FROM image_fingerprints", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(stored, 1);
    }

    #[test]
    fn pending_claim_is_single_winner_and_failed_publish_can_be_retried() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE pending(
                token INTEGER PRIMARY KEY,
                files TEXT NOT NULL,
                caption TEXT NOT NULL,
                msg_ids TEXT NOT NULL,
                originals TEXT NOT NULL DEFAULT '[]',
                created_at INTEGER NOT NULL DEFAULT 0,
                state TEXT NOT NULL DEFAULT 'pending',
                is_r18 INTEGER NOT NULL DEFAULT 0,
                item_meta TEXT NOT NULL DEFAULT '{}'
            );",
        )
        .unwrap();
        db.execute(
            "INSERT INTO pending(token, files, caption, msg_ids, originals, created_at)
             VALUES(1, '[]', 'caption', '[]', '[]', 1)",
            [],
        )
        .unwrap();

        assert!(matches!(
            claim_pending(&db, 1).unwrap(),
            PendingClaim::Claimed((_, caption, _, _, _, _)) if caption == "caption"
        ));
        assert!(matches!(
            claim_pending(&db, 1).unwrap(),
            PendingClaim::Publishing
        ));
        assert_eq!(restore_pending(&db, 1).unwrap(), 1);
        assert!(matches!(
            claim_pending(&db, 1).unwrap(),
            PendingClaim::Claimed(_)
        ));
    }

    #[test]
    fn claim_all_only_takes_remaining_pending_in_order() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE pending(
                token INTEGER PRIMARY KEY,
                files TEXT NOT NULL,
                caption TEXT NOT NULL,
                msg_ids TEXT NOT NULL,
                originals TEXT NOT NULL DEFAULT '[]',
                created_at INTEGER NOT NULL DEFAULT 0,
                state TEXT NOT NULL DEFAULT 'pending',
                is_r18 INTEGER NOT NULL DEFAULT 0,
                item_meta TEXT NOT NULL DEFAULT '{}'
            );
            INSERT INTO pending VALUES(1, '[]', 'first', '[]', '[]', 20, 'pending', 0, '{}');
            INSERT INTO pending VALUES(2, '[]', 'cancelled', '[]', '[]', 10, 'publishing', 0, '{}');
            INSERT INTO pending VALUES(3, '[]', 'oldest', '[]', '[]', 10, 'pending', 1, '{}');",
        )
        .unwrap();

        let claimed = claim_all_pending(&mut db).unwrap();
        let tokens: Vec<i64> = claimed.iter().map(|(token, _)| *token).collect();
        assert_eq!(tokens, vec![3, 1]);
        assert!(claimed
            .iter()
            .all(|(_, (_, caption, _, _, _, _))| { caption == "oldest" || caption == "first" }));
        // is_r18 随行读出,发布时据此打剧透遮罩。
        assert!(claimed
            .iter()
            .any(|(t, (_, _, _, _, r18, _))| *t == 3 && *r18));
        assert!(claim_all_pending(&mut db).unwrap().is_empty());
        let cancelled_state: String = db
            .query_row("SELECT state FROM pending WHERE token=2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cancelled_state, "publishing");
    }

    #[test]
    fn decode_pending_roundtrip_and_bad_json() {
        // 正常行:JSON 数组解码为路径/消息 id,is_r18 透传。
        let work = decode_pending((
            r#"["/tmp/a.jpg","/tmp/b.jpg"]"#.into(),
            "cap".into(),
            "[10,11]".into(),
            r#"["/tmp/a.jpg"]"#.into(),
            true,
            "{}".into(),
        ))
        .unwrap();
        assert_eq!(work.files.len(), 2);
        assert_eq!(work.msg_ids, vec![10, 11]);
        assert_eq!(work.originals, vec![PathBuf::from("/tmp/a.jpg")]);
        assert!(work.is_r18);

        // originals 坏 JSON(旧版本残留/手改库)降级为空,不报错。
        let work = decode_pending((
            "[]".into(),
            "cap".into(),
            "[]".into(),
            "not-json".into(),
            false,
            "{}".into(),
        ))
        .unwrap();
        assert!(work.originals.is_empty());

        // files/msg_ids 坏 JSON 必须报错(缺了它们无法完成审批动作)。
        assert!(decode_pending((
            "not-json".into(),
            "cap".into(),
            "[]".into(),
            "[]".into(),
            false,
            "{}".into(),
        ))
        .is_err());
        assert!(decode_pending((
            "[]".into(),
            "cap".into(),
            "not-json".into(),
            "[]".into(),
            false,
            "{}".into(),
        ))
        .is_err());
    }

    #[test]
    fn command_menu_contains_one_click_approve() {
        let menu = command_menu(true);
        assert!(menu.iter().any(|cmd| cmd.command == "approve"));
        assert!(menu.iter().any(|cmd| cmd.command == "approve_archive"));
        assert!(menu.iter().any(|cmd| cmd.command == "undo"));

        let menu_without_gallery = command_menu(false);
        assert!(menu_without_gallery
            .iter()
            .all(|cmd| cmd.command != "approve_archive"));
    }

    #[test]
    fn discarded_action_can_be_claimed_once_and_retried() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.db");
        let _sink = TelegramSink::new(
            "123:abc".into(),
            "-1001234567890".into(),
            "@chan".into(),
            path.to_str().unwrap(),
            None,
        )
        .unwrap();
        let mut db = rusqlite::Connection::open(&path).unwrap();
        db.execute(
            "INSERT INTO pending(token,files,caption,msg_ids,originals,created_at,state,is_r18,item_meta)
             VALUES(1,'[\"/tmp/a.jpg\"]','cap','[9]','[\"/tmp/a.jpg\"]',1,'publishing',0,'{\"source\":\"x\"}')",
            [],
        )
        .unwrap();
        let row: PendingRow = (
            "[\"/tmp/a.jpg\"]".into(),
            "cap".into(),
            "[9]".into(),
            "[\"/tmp/a.jpg\"]".into(),
            false,
            "{\"source\":\"x\"}".into(),
        );
        assert!(
            complete_and_record_action(&mut db, 1, "discard", Some(&row))
                .unwrap()
                .is_empty()
        );
        let pending: i64 = db
            .query_row("SELECT COUNT(*) FROM pending", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pending, 0);

        let UndoClaim::Claimed(first) = claim_latest_undo(&mut db).unwrap() else {
            panic!("discard should be undoable")
        };
        assert_eq!(first.row.1, "cap");
        assert!(matches!(
            claim_latest_undo(&mut db).unwrap(),
            UndoClaim::Empty
        ));
        assert_eq!(restore_undo(&db, first.id).unwrap(), 1);
        assert!(matches!(
            claim_latest_undo(&mut db).unwrap(),
            UndoClaim::Claimed(_)
        ));
    }

    #[test]
    fn newer_publish_blocks_undo_of_older_discard_and_releases_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.db");
        let _sink = TelegramSink::new(
            "123:abc".into(),
            "-1001234567890".into(),
            "@chan".into(),
            path.to_str().unwrap(),
            None,
        )
        .unwrap();
        let mut db = rusqlite::Connection::open(&path).unwrap();
        for token in [1, 2] {
            db.execute(
                "INSERT INTO pending(token,files,caption,msg_ids,originals,created_at,state,is_r18,item_meta)
                 VALUES(?1,'[\"/tmp/a.jpg\"]','cap','[9]','[\"/tmp/a.jpg\"]',1,'publishing',0,'{}')",
                [token],
            )
            .unwrap();
        }
        let row: PendingRow = (
            "[\"/tmp/a.jpg\"]".into(),
            "cap".into(),
            "[9]".into(),
            "[\"/tmp/a.jpg\"]".into(),
            false,
            "{}".into(),
        );
        complete_and_record_action(&mut db, 1, "discard", Some(&row)).unwrap();
        let released = complete_and_record_action(&mut db, 2, "publish", None).unwrap();
        assert_eq!(released, vec![vec![PathBuf::from("/tmp/a.jpg")]]);
        assert!(matches!(
            claim_latest_undo(&mut db).unwrap(),
            UndoClaim::Irreversible(action) if action == "publish"
        ));
    }

    #[test]
    fn match_auto_forward_extracts_channel_msg_id() {
        let raw = include_str!("../../tests/fixtures/auto_forward_channel.json");
        let msg: teloxide::types::Message = serde_json::from_str(raw).unwrap();
        // 源频道用户名匹配 → 返回被转发的频道帖 msg_id(789)。
        let chan = to_recipient("@FurinaDeCanvas".into());
        assert_eq!(match_auto_forward(&msg, &chan), Some(789));
        // 用数字 id 匹配同一频道。
        let chan_id = to_recipient("-1002222222222".into());
        assert_eq!(match_auto_forward(&msg, &chan_id), Some(789));
        // 不匹配的频道 → None。
        let other = to_recipient("@SomeoneElse".into());
        assert_eq!(match_auto_forward(&msg, &other), None);
    }

    #[test]
    fn classify_single_vs_multi() {
        assert_eq!(
            classify_link("https://www.pixiv.net/artworks/123"),
            Some(LinkKind::Single)
        );
        assert_eq!(
            classify_link("https://www.pixiv.net/i/123"),
            Some(LinkKind::Single)
        );
        assert_eq!(
            classify_link("https://x.com/user/status/9"),
            Some(LinkKind::Single)
        );
        assert_eq!(
            classify_link("https://twitter.com/u/status/7"),
            Some(LinkKind::Single)
        );
        assert_eq!(
            classify_link("https://www.pixiv.net/users/555"),
            Some(LinkKind::Multi)
        );
        assert_eq!(
            classify_link("https://www.pixiv.net/ranking.php?mode=weekly"),
            Some(LinkKind::Multi)
        );
        assert_eq!(
            classify_link("https://x.com/i/lists/42"),
            Some(LinkKind::Multi)
        );
        // 子串伪装域名被 host 判定挡掉。
        assert_eq!(classify_link("https://evil.com/pixiv.net/artworks/1"), None);
        assert_eq!(classify_link("https://example.com/a"), None);
    }

    #[test]
    fn first_successful_telegram_batch_activates_exactly_once() {
        let mut sent = Vec::new();
        let mut activations = 0;
        let mut activate = || activations += 1;

        record_sent(&mut sent, [MessageId(10), MessageId(11)], &mut activate);
        record_sent(&mut sent, [MessageId(12)], &mut activate);

        assert_eq!(sent, vec![MessageId(10), MessageId(11), MessageId(12)]);
        assert_eq!(activations, 1);
    }
}
