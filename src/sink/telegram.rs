use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use rusqlite::OptionalExtension;
use teloxide::prelude::*;
use teloxide::types::{
    AllowedUpdate, CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, InputFile,
    InputMedia, InputMediaDocument, InputMediaPhoto, MessageId, MessageOrigin, ParseMode,
    Recipient, ReplyParameters, UpdateKind,
};
use tokio::sync::Mutex;

use crate::model::MediaItem;
use crate::sink::{needs_downscale, render_caption, Sink};

/// Telegram photo 缩放目标边长上限(超限按比例缩到此框内)。
const MAX_DIMENSION: u32 = 4096;
/// pending 保留时长上限(秒);超期未审批自动清理(删消息+文件+记录)。
const PENDING_TTL_SECS: i64 = 7 * 24 * 3600;

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

/// 待投递评论区的原图任务:发布到频道后登记,等讨论组 auto-forward 到来时
/// 把原画质 document reply 进该帖评论区。temp_dir 在投递完成或超时后清理。
struct CommentJob {
    originals: Vec<PathBuf>,
    temp_dir: PathBuf,
    created_at: i64,
}

/// 评论区原图任务的兜底保留时长:超时仍未等到 auto-forward 则清临时目录,避免泄漏。
const COMMENT_TTL_SECS: i64 = 120;

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
}

impl ReviewState {
    fn next_token(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }
}

pub struct TelegramSink {
    state: Arc<ReviewState>,
}

impl TelegramSink {
    pub fn new(
        token: String,
        review_chat_id: String,
        publish_channel_id: String,
        db_path: &str,
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
        let conn = rusqlite::Connection::open(db_path).context("打开 pending 数据库失败")?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS pending(
                token      INTEGER PRIMARY KEY,
                files      TEXT NOT NULL,
                caption    TEXT NOT NULL,
                msg_ids    TEXT NOT NULL,
                originals  TEXT NOT NULL DEFAULT '[]',
                created_at INTEGER NOT NULL DEFAULT 0
             );",
        )
        .context("初始化 pending 表失败")?;
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
        // counter 从已有最大 token 续上,避免重启后 token 与旧记录冲突。
        let max_token: i64 = conn
            .query_row("SELECT COALESCE(MAX(token), 0) FROM pending", [], |r| {
                r.get(0)
            })
            .unwrap_or(0);
        // 自定义 client:
        // - timeout(300):整体超时。yunyoo-la 上传带宽约 170KB/s,orig 4K 大图(几 MB)
        //   单张需数十秒、多图一次 sendMediaGroup 可达 2-3 分钟,给足 5 分钟避免超时。
        // - connect_timeout(15):连接阶段超时,短一些好快速失败重试。
        // - trust_dns(true):纯 Rust DNS,避开 musl 静态二进制 getaddrinfo 解析失败
        //   (reqwest 0.11 光开 feature 不够,必须显式调用此方法)。
        // trust_dns 已 deprecated 但保留:它是 musl 静态二进制 DNS 解析的命脉
        // (reqwest 0.11 仅开 feature 不够,必须显式调用),不为消 lint 冒险换 hickory_dns。
        #[allow(deprecated)]
        let client = teloxide::net::default_reqwest_settings()
            .timeout(std::time::Duration::from_secs(300))
            .connect_timeout(std::time::Duration::from_secs(15))
            .trust_dns(true)
            .build()
            .context("构造 reqwest client 失败")?;
        Ok(Self {
            state: Arc::new(ReviewState {
                bot: Bot::with_client(token, client),
                review_chat: to_recipient(review_chat_id),
                owner,
                publish_channel: to_recipient(publish_channel_id),
                db: Mutex::new(conn),
                counter: AtomicU64::new(max_token as u64 + 1),
                pending_comments: Mutex::new(std::collections::HashMap::new()),
            }),
        })
    }

    /// 供 main 启动 callback 轮询任务(与抓取循环并发)。
    pub fn state(&self) -> Arc<ReviewState> {
        self.state.clone()
    }

    /// 直接发布到频道(跳过审批):用于手动发来的链接,作品即时发布。
    pub async fn publish_direct(&self, item: &MediaItem, files: &[PathBuf]) -> Result<()> {
        if files.is_empty() {
            anyhow::bail!("无图片可发: {}", item.source_id);
        }
        let caption = render_caption(item);
        let files_owned: Vec<PathBuf> = files.to_vec();
        let prepared = tokio::task::spawn_blocking(move || prepare_all(&files_owned)).await??;
        let first_id = send_group(
            &self.state.bot,
            &self.state.publish_channel,
            &prepared,
            &caption,
            item.is_r18, // R18 → 频道帖打剧透遮罩
        )
        .await?;
        // 登记原图评论任务,等讨论组 auto-forward 到来再投递;登记则延后清理临时目录。
        match first_id {
            Some(mid) => register_comment(&self.state, mid.0, files).await,
            None => cleanup(files),
        }
        Ok(())
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
    let _ = std::fs::remove_dir_all(&job.temp_dir);
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
        let _ = std::fs::remove_dir_all(&job.temp_dir);
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

fn prepare_all(files: &[PathBuf]) -> Result<Vec<PathBuf>> {
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

/// 发一组图到指定 chat(用于发布到频道)。sendMediaGroup 限 2–10,超出按 10 分批,
/// 余数 1 张退 sendPhoto。caption 仅置于最前一张。每个请求带限流重试。
async fn send_group(
    bot: &Bot,
    chat: &Recipient,
    prepared: &[PathBuf],
    caption: &str,
    spoiler: bool,
) -> Result<Option<MessageId>> {
    if prepared.is_empty() {
        anyhow::bail!("无图可发");
    }
    if prepared.len() == 1 {
        let m = tg_retry(|| {
            bot.send_photo(chat.clone(), InputFile::file(&prepared[0]))
                .caption(caption.to_string())
                .parse_mode(ParseMode::Html)
                .has_spoiler(spoiler)
        })
        .await?;
        return Ok(Some(m.id));
    }
    // 记首条帖 msg_id:用于评论区把原图 reply 到该帖的 auto-forward 上。
    let mut first_id: Option<MessageId> = None;
    for (ci, chunk) in prepared.chunks(10).enumerate() {
        let cap = if ci == 0 { caption } else { "" };
        if chunk.len() == 1 {
            let m = tg_retry(|| {
                let req = bot
                    .send_photo(chat.clone(), InputFile::file(&chunk[0]))
                    .has_spoiler(spoiler);
                if ci == 0 {
                    req.caption(cap.to_string()).parse_mode(ParseMode::Html)
                } else {
                    req
                }
            })
            .await?;
            first_id.get_or_insert(m.id);
        } else {
            let msgs =
                tg_retry(|| bot.send_media_group(chat.clone(), build_media(chunk, cap, spoiler)))
                    .await?;
            if let Some(m) = msgs.first() {
                first_id.get_or_insert(m.id);
            }
        }
    }
    Ok(first_id)
}

/// 清理某作品的临时目录(原图 + 缩放图同处一目录)。
fn cleanup(files: &[PathBuf]) {
    if let Some(parent) = files.first().and_then(|p| p.parent()) {
        let _ = std::fs::remove_dir_all(parent);
    }
}

/// 启动清理:① 删超期未审 pending(消息+文件+记录);② 删 `/tmp/hanabi_*` 中
/// 不被任何 pending 引用的孤儿目录(多为旧版本/重启遗留)。
async fn cleanup_stale(state: &Arc<ReviewState>) {
    // ① 超期 pending。
    let cutoff = now_secs() - PENDING_TTL_SECS;
    let expired: Vec<(i64, String, String)> = {
        let db = state.db.lock().await;
        let mut out = Vec::new();
        if let Ok(mut stmt) = db.prepare(
            "SELECT token, files, msg_ids FROM pending WHERE created_at > 0 AND created_at < ?1",
        ) {
            if let Ok(rows) = stmt.query_map([cutoff], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            }) {
                out.extend(rows.flatten());
            }
        }
        out
    };
    for (token, files_json, msg_json) in &expired {
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
        let _ = state
            .db
            .lock()
            .await
            .execute("DELETE FROM pending WHERE token=?1", [*token]);
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
    if let Ok(rd) = std::fs::read_dir(std::env::temp_dir()) {
        let mut orphans = 0;
        for e in rd.flatten() {
            let p = e.path();
            let is_hanabi = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("hanabi_"));
            if is_hanabi && p.is_dir() && !referenced.contains(&p) {
                let _ = std::fs::remove_dir_all(&p);
                orphans += 1;
            }
        }
        if orphans > 0 {
            tracing::info!(orphans, "清理孤儿临时目录");
        }
    }
}

#[async_trait]
impl Sink for TelegramSink {
    /// 发到审批私聊:**全套图**(单图=sendPhoto+按钮;多图=图组+一条带按钮的控制消息)。
    /// 发送成功后把 pending 持久化到 sqlite;文件保留到审批结束才清理。
    async fn deliver(&self, item: &MediaItem, files: &[PathBuf]) -> Result<()> {
        if files.is_empty() {
            anyhow::bail!("无图片可发: {}", item.source_id);
        }
        let caption = render_caption(item);
        let files_owned: Vec<PathBuf> = files.to_vec();
        // 原始(缩放前)路径:审批通过后发原画质 document 进频道帖评论区(发 photo 用缩放版)。
        let originals: Vec<PathBuf> = files.to_vec();

        // 全套图缩放(CPU 阻塞,放 blocking 线程);审批需要看到全部,批准后直接复用。
        let prepared = tokio::task::spawn_blocking(move || prepare_all(&files_owned)).await??;

        let token = self.state.next_token();
        let keyboard = InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback("✅ 发送到频道", format!("ok:{token}")),
            InlineKeyboardButton::callback("❌ 丢弃", format!("no:{token}")),
        ]]);

        let n = prepared.len();
        let bot = &self.state.bot;
        let chat = self.state.review_chat.clone();
        let mut review_ids: Vec<MessageId> = Vec::new();

        if n == 1 {
            let msg = tg_retry(|| {
                bot.send_photo(chat.clone(), InputFile::file(&prepared[0]))
                    .caption(format!("【待审】\n{caption}"))
                    .parse_mode(ParseMode::Html)
                    .reply_markup(keyboard.clone())
            })
            .await?;
            review_ids.push(msg.id);
        } else {
            let first_cap = format!("【待审 · 共 {n} 张】\n{caption}");
            let msgs = tg_retry(|| {
                bot.send_media_group(chat.clone(), build_media(&prepared, &first_cap, false))
            })
            .await?;
            review_ids.extend(msgs.iter().map(|m| m.id));
            let ctrl = tg_retry(|| {
                bot.send_message(chat.clone(), format!("👆 上面 {n} 张,请审批"))
                    .reply_markup(keyboard.clone())
            })
            .await?;
            review_ids.push(ctrl.id);
        }

        // 持久化 pending(发送成功后才写,保证按钮一定对得上)。
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
        {
            let db = self.state.db.lock().await;
            db.execute(
                "INSERT OR REPLACE INTO pending(token, files, caption, msg_ids, originals, created_at) VALUES(?1,?2,?3,?4,?5,?6)",
                rusqlite::params![token as i64, files_json, caption, msg_json, originals_json, now_secs()],
            )?;
        }
        Ok(())
    }
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
    // 启动先清一次超期/孤儿(顺手清掉旧版本遗留的临时图)。
    cleanup_stale(&state).await;
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
                                deliver_comment(&state, &msg, chan_msg_id).await;
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
/// /run 触发抓取;/status /ping /help 即时回复;非命令的 Pixiv/X 链接交抓取循环直发频道。
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
            let _ = link
                .send(LinkJob {
                    url,
                    user_msg_id: msg.id.0,
                    notice_msg_id: notice.id.0,
                })
                .await;
        }
        return Ok(());
    }
    let cmd = text.split_whitespace().next().unwrap_or("");
    let cmd = cmd.split('@').next().unwrap_or(cmd);
    let reply: String = match cmd {
        "/run" => {
            let _ = trigger.send(()).await;
            "🚀 开始手动抓取一轮,有命中会发审批消息过来".to_string()
        }
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
            "命令列表:\n/run — 立即抓取一轮\n/status — 待审数+运行状态\n/ping — 存活测试\n/help — 本帮助\n\n💡 直接发 Pixiv/X 作品链接 → 自动抓取并发布到频道"
                .to_string()
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
    let data = q.data.clone().unwrap_or_default();
    let (action, token_str) = data.split_once(':').unwrap_or(("", ""));
    let token: i64 = token_str.parse().unwrap_or(-1);

    // 查 pending(不删);db 锁仅在查询期间持有,发送期间不持锁。
    let row: Option<(String, String, String, String)> = {
        let db = state.db.lock().await;
        db.query_row(
            "SELECT files, caption, msg_ids, originals FROM pending WHERE token=?1",
            [token],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?
    };

    let Some((files_json, caption, msg_json, originals_json)) = row else {
        let _ = state
            .bot
            .answer_callback_query(q.id)
            .text("该条已失效")
            .await;
        return Ok(());
    };

    let files: Vec<PathBuf> = serde_json::from_str::<Vec<String>>(&files_json)?
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let originals: Vec<PathBuf> = serde_json::from_str::<Vec<String>>(&originals_json)
        .unwrap_or_default()
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let msg_ids: Vec<i32> = serde_json::from_str(&msg_json)?;
    let is_ok = action == "ok";

    // 立即应答,停止按钮转圈(必须 3 秒内,否则 callback query 过期)。
    // 发图/删消息这些耗时操作放后台,不让你盯着转圈等上传。
    let _ = state
        .bot
        .answer_callback_query(q.id)
        .text(if is_ok {
            "⏳ 发布中…"
        } else {
            "❌ 已丢弃"
        })
        .await;

    // 后台执行:批准→发频道;然后删私聊整组 + 清文件 + 删 pending。
    // 失败(如限流)保留 pending,发提示可重点。
    let state = state.clone();
    tokio::spawn(async move {
        // R18 → 频道帖打剧透遮罩;caption 由 render_caption 生成,R18 时以 🔞 R18 起头。
        let spoiler = caption.contains("🔞 R18");
        let send_result: Result<Option<MessageId>> = if is_ok {
            send_group(
                &state.bot,
                &state.publish_channel,
                &files,
                &caption,
                spoiler,
            )
            .await
        } else {
            Ok(None)
        };
        match send_result {
            Ok(first_id) => {
                for mid in &msg_ids {
                    let _ = state
                        .bot
                        .delete_message(state.review_chat.clone(), MessageId(*mid))
                        .await;
                }
                let _ = state
                    .db
                    .lock()
                    .await
                    .execute("DELETE FROM pending WHERE token=?1", [token]);
                // 批准且拿到首条频道帖 msg_id → 登记原图评论任务(延后清理);否则即时清。
                match (is_ok, first_id) {
                    (true, Some(mid)) if !originals.is_empty() => {
                        register_comment(&state, mid.0, &originals).await;
                    }
                    _ => cleanup(&files),
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, token, "审批发布失败,pending 保留可重试");
                let _ = state
                    .bot
                    .send_message(
                        state.review_chat.clone(),
                        "⚠️ 发布失败(可能限流),过会儿再点一次那条审批",
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
    fn cleanup_due_after_interval() {
        assert!(!cleanup_due(1000, 1000 + 6 * 3600 - 1, 6 * 3600));
        assert!(cleanup_due(1000, 1000 + 6 * 3600, 6 * 3600));
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
        assert_eq!(parse_owner("7794592020"), Some(7794592020));
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
        let _sink = TelegramSink::new(
            "123:abc".into(),
            "7794592020".into(),
            "@chan".into(),
            path.to_str().unwrap(),
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
}
