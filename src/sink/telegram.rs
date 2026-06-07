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
        Self { bot: Bot::new(token), channel }
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
        let prepared: Vec<PathBuf> = files.iter().map(|p| prepare(p)).collect::<Result<_>>()?;

        if prepared.len() == 1 {
            self.bot
                .send_photo(self.channel.clone(), InputFile::file(&prepared[0]))
                .caption(caption)
                .parse_mode(ParseMode::Html)
                .await?;
        } else {
            let media: Vec<InputMedia> = prepared
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let mut photo = InputMediaPhoto::new(InputFile::file(p));
                    if i == 0 {
                        photo = photo.caption(caption.clone()).parse_mode(ParseMode::Html);
                    }
                    InputMedia::Photo(photo)
                })
                .collect();
            self.bot.send_media_group(self.channel.clone(), media).await?;
        }
        Ok(())
    }
}
