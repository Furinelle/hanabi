use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use teloxide::prelude::*;
use teloxide::types::{InputFile, InputMedia, InputMediaPhoto, ParseMode, Recipient};

use crate::model::MediaItem;
use crate::sink::{needs_downscale, render_caption, Sink};

pub struct TelegramSink {
    bot: Bot,
    channel: Recipient,
}

impl TelegramSink {
    pub fn new(token: String, channel_id: String) -> Self {
        let channel = match channel_id.parse::<i64>() {
            Ok(id) => Recipient::Id(ChatId(id)),
            Err(_) => Recipient::ChannelUsername(channel_id),
        };
        Self {
            bot: Bot::new(token),
            channel,
        }
    }

    /// 发送已准备好的图片。caption 仅置于最前一张。
    /// sendMediaGroup 限 2–10 张,超出按 10 分批;余数 1 张退回 sendPhoto。
    async fn send_all(&self, prepared: &[PathBuf], caption: &str) -> Result<()> {
        if prepared.len() == 1 {
            self.bot
                .send_photo(self.channel.clone(), InputFile::file(&prepared[0]))
                .caption(caption.to_string())
                .parse_mode(ParseMode::Html)
                .await?;
            return Ok(());
        }
        for (ci, chunk) in prepared.chunks(10).enumerate() {
            if chunk.len() == 1 {
                let mut req = self
                    .bot
                    .send_photo(self.channel.clone(), InputFile::file(&chunk[0]));
                if ci == 0 {
                    req = req.caption(caption.to_string()).parse_mode(ParseMode::Html);
                }
                req.await?;
                continue;
            }
            let media: Vec<InputMedia> = chunk
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let mut photo = InputMediaPhoto::new(InputFile::file(p));
                    if ci == 0 && i == 0 {
                        photo = photo
                            .caption(caption.to_string())
                            .parse_mode(ParseMode::Html);
                    }
                    InputMedia::Photo(photo)
                })
                .collect();
            self.bot
                .send_media_group(self.channel.clone(), media)
                .await?;
        }
        Ok(())
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

#[async_trait]
impl Sink for TelegramSink {
    async fn deliver(&self, item: &MediaItem, files: &[PathBuf]) -> Result<()> {
        if files.is_empty() {
            anyhow::bail!("无图片可发: {}", item.source_id);
        }
        let caption = render_caption(item);

        // 图片解码/缩放是 CPU 阻塞操作,放到 blocking 线程,避免占用 tokio worker。
        let files_owned: Vec<PathBuf> = files.to_vec();
        let prepared: Vec<PathBuf> = tokio::task::spawn_blocking(move || {
            files_owned
                .iter()
                .map(|p| prepare(p))
                .collect::<Result<Vec<_>>>()
        })
        .await??;

        let result = self.send_all(&prepared, &caption).await;

        // 清理本 item 的临时目录(原图 + 缩放图同处一目录),无论成败,防磁盘泄漏。
        if let Some(parent) = files.first().and_then(|p| p.parent()) {
            let _ = std::fs::remove_dir_all(parent);
        }
        result
    }
}
