use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use teloxide::prelude::*;
use teloxide::types::{
    AllowedUpdate, CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, InputFile,
    InputMedia, InputMediaPhoto, MessageId, ParseMode, Recipient, UpdateKind,
};
use tokio::sync::Mutex;

use crate::model::MediaItem;
use crate::sink::{needs_downscale, render_caption, Sink};

/// 一条待审批作品:全套(已缩放)图文件 + caption + 私聊里这条审批占用的所有消息 id
/// (多图 = 图组多条 + 控制消息一条;审批结束后整组删除)。
struct Pending {
    files: Vec<PathBuf>,
    caption: String,
    review_msg_ids: Vec<MessageId>,
}

/// 审批状态:由 `TelegramSink`(发审批消息)与 callback 轮询任务共享。
pub struct ReviewState {
    bot: Bot,
    review_chat: Recipient,     // 审批私聊
    publish_channel: Recipient, // 批准后发布频道
    pending: Mutex<HashMap<u64, Pending>>,
    counter: AtomicU64,
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
    pub fn new(token: String, review_chat_id: String, publish_channel_id: String) -> Self {
        Self {
            state: Arc::new(ReviewState {
                bot: Bot::new(token),
                review_chat: to_recipient(review_chat_id),
                publish_channel: to_recipient(publish_channel_id),
                pending: Mutex::new(HashMap::new()),
                counter: AtomicU64::new(1),
            }),
        }
    }

    /// 供 main 启动 callback 轮询任务(与抓取循环并发)。
    pub fn state(&self) -> Arc<ReviewState> {
        self.state.clone()
    }
}

fn to_recipient(id: String) -> Recipient {
    match id.parse::<i64>() {
        Ok(n) => Recipient::Id(ChatId(n)),
        Err(_) => Recipient::ChannelUsername(id),
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
    let scaled = dyn_img.resize(4096, 4096, image::imageops::FilterType::Lanczos3);
    let out = path.with_extension("scaled.jpg");
    scaled.to_rgb8().save(&out).context("保存缩放图失败")?;
    Ok(out)
}

fn prepare_all(files: &[PathBuf]) -> Result<Vec<PathBuf>> {
    files.iter().map(|p| prepare(p)).collect()
}

/// 构造图组:第一张挂 caption,其余无。
fn build_media(prepared: &[PathBuf], caption: &str) -> Vec<InputMedia> {
    prepared
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let mut photo = InputMediaPhoto::new(InputFile::file(p));
            if i == 0 && !caption.is_empty() {
                photo = photo.caption(caption.to_string()).parse_mode(ParseMode::Html);
            }
            InputMedia::Photo(photo)
        })
        .collect()
}

/// 发一组图到指定 chat(用于发布到频道)。sendMediaGroup 限 2–10,超出按 10 分批,
/// 余数 1 张退 sendPhoto。caption 仅置于最前一张。
async fn send_group(bot: &Bot, chat: &Recipient, prepared: &[PathBuf], caption: &str) -> Result<()> {
    if prepared.is_empty() {
        anyhow::bail!("无图可发");
    }
    if prepared.len() == 1 {
        bot.send_photo(chat.clone(), InputFile::file(&prepared[0]))
            .caption(caption.to_string())
            .parse_mode(ParseMode::Html)
            .await?;
        return Ok(());
    }
    for (ci, chunk) in prepared.chunks(10).enumerate() {
        if chunk.len() == 1 {
            let mut req = bot.send_photo(chat.clone(), InputFile::file(&chunk[0]));
            if ci == 0 {
                req = req.caption(caption.to_string()).parse_mode(ParseMode::Html);
            }
            req.await?;
            continue;
        }
        let cap = if ci == 0 { caption } else { "" };
        bot.send_media_group(chat.clone(), build_media(chunk, cap))
            .await?;
    }
    Ok(())
}

/// 清理某作品的临时目录(原图 + 缩放图同处一目录)。
fn cleanup(files: &[PathBuf]) {
    if let Some(parent) = files.first().and_then(|p| p.parent()) {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[async_trait]
impl Sink for TelegramSink {
    /// 发到审批私聊:**全套图**(单图=sendPhoto+按钮;多图=图组+一条带按钮的控制消息)。
    /// 登记 pending,文件保留到审批结束才清理(批准时要发全套到频道)。
    async fn deliver(&self, item: &MediaItem, files: &[PathBuf]) -> Result<()> {
        if files.is_empty() {
            anyhow::bail!("无图片可发: {}", item.source_id);
        }
        let caption = render_caption(item);
        let files_owned: Vec<PathBuf> = files.to_vec();

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
            let msg = bot
                .send_photo(chat.clone(), InputFile::file(&prepared[0]))
                .caption(format!("【待审】\n{caption}"))
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboard)
                .await?;
            review_ids.push(msg.id);
        } else {
            // 图组(全部图,第一张带 caption)。
            let first_cap = format!("【待审 · 共 {n} 张】\n{caption}");
            let msgs = bot
                .send_media_group(chat.clone(), build_media(&prepared, &first_cap))
                .await?;
            review_ids.extend(msgs.iter().map(|m| m.id));
            // 图组挂不了按钮,紧跟一条控制消息承载按钮。
            let ctrl = bot
                .send_message(chat.clone(), format!("👆 上面 {n} 张,请审批"))
                .reply_markup(keyboard)
                .await?;
            review_ids.push(ctrl.id);
        }

        self.state.pending.lock().await.insert(
            token,
            Pending {
                files: prepared,
                caption,
                review_msg_ids: review_ids,
            },
        );
        // 不在此清理文件——等 callback 审批后再清理。
        Ok(())
    }
}

/// callback 轮询:监听按钮点击。批准 → 发频道 + 删私聊整组审批消息;
/// 拒绝 → 删私聊整组审批消息。无论结果都清理临时文件。与抓取循环并发运行。
///
/// 用短轮询(timeout=0,空结果 sleep)而非长轮询:经代理时长连接长轮询易被掐断
/// (operation timed out),导致按钮点击收不到。
pub async fn run_review_loop(
    state: Arc<ReviewState>,
    trigger: tokio::sync::mpsc::Sender<()>,
) {
    let mut offset: i32 = 0;
    loop {
        let updates = state
            .bot
            .get_updates()
            .offset(offset)
            .timeout(0)
            .allowed_updates(vec![AllowedUpdate::CallbackQuery, AllowedUpdate::Message])
            .await;
        match updates {
            Ok(list) => {
                if list.is_empty() {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    continue;
                }
                for u in list {
                    offset = u.id.0 as i32 + 1;
                    match u.kind {
                        UpdateKind::CallbackQuery(q) => {
                            if let Err(e) = handle_callback(&state, q).await {
                                tracing::warn!(error = %e, "处理审批回调失败");
                            }
                        }
                        UpdateKind::Message(msg) => {
                            if let Err(e) = handle_command(&state, &msg, &trigger).await {
                                tracing::warn!(error = %e, "处理命令失败");
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "get_updates 失败");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        }
    }
}

/// 处理 `/` 命令(仅文本消息)。/run 发触发信号让抓取循环立即跑一轮;
/// /status /ping /help 即时回复。
async fn handle_command(
    state: &Arc<ReviewState>,
    msg: &Message,
    trigger: &tokio::sync::mpsc::Sender<()>,
) -> Result<()> {
    let text = msg.text().unwrap_or("");
    if !text.starts_with('/') {
        return Ok(());
    }
    // 取首词,去掉可能的 @botname 后缀(如 /run@Furinabi_bot)。
    let cmd = text.split_whitespace().next().unwrap_or("");
    let cmd = cmd.split('@').next().unwrap_or(cmd);
    let reply: String = match cmd {
        "/run" => {
            let _ = trigger.send(()).await;
            "🚀 开始手动抓取一轮,有命中会发审批消息过来".to_string()
        }
        "/status" => {
            let pending = state.pending.lock().await.len();
            format!("✅ 运行中\n待审: {pending} 条")
        }
        "/ping" => "pong 🏓".to_string(),
        "/help" => {
            "命令列表:\n/run — 立即抓取一轮\n/status — 待审数+运行状态\n/ping — 存活测试\n/help — 本帮助"
                .to_string()
        }
        _ => return Ok(()),
    };
    state.bot.send_message(msg.chat.id, reply).await?;
    Ok(())
}

async fn handle_callback(state: &Arc<ReviewState>, q: CallbackQuery) -> Result<()> {
    let data = q.data.clone().unwrap_or_default();
    let (action, token_str) = data.split_once(':').unwrap_or(("", ""));
    let token: u64 = token_str.parse().unwrap_or(0);

    let pending = state.pending.lock().await.remove(&token);
    let note: &str;
    if let Some(p) = pending {
        match action {
            "ok" => {
                // 批准:全套图(已缩放)发频道。
                send_group(&state.bot, &state.publish_channel, &p.files, &p.caption).await?;
                note = "✅ 已发布到频道";
            }
            "no" => {
                note = "❌ 已丢弃";
            }
            _ => {
                note = "未知操作";
            }
        }
        // 删私聊整组审批消息(图组各条 + 控制消息)。
        for mid in &p.review_msg_ids {
            let _ = state
                .bot
                .delete_message(state.review_chat.clone(), *mid)
                .await;
        }
        // 清理临时文件。
        cleanup(&p.files);
    } else {
        note = "该条已失效";
    }

    // 让按钮停止转圈。
    let _ = state.bot.answer_callback_query(q.id).text(note).await;
    Ok(())
}
