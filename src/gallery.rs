//! Vitrine 图库入库客户端：把本地图片 + 作品元数据 POST 到 CF Workers。

use std::path::Path;

use anyhow::{Context, Result};
use reqwest::multipart::{Form, Part};

use crate::model::MediaItem;

#[derive(Debug, Clone)]
pub struct GalleryClient {
    endpoint: String,
    token: String,
    client: reqwest::Client,
}

impl GalleryClient {
    pub fn new(endpoint: String, token: String) -> Result<Self> {
        let endpoint = endpoint.trim_end_matches('/').to_string();
        #[allow(deprecated)]
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(180))
            .connect_timeout(std::time::Duration::from_secs(15))
            .trust_dns(true)
            .build()
            .context("构造 gallery http client 失败")?;
        Ok(Self {
            endpoint,
            token,
            client,
        })
    }

    /// 上传整套作品图片到图库。失败返回 Err，调用方只记日志不阻断频道发布。
    pub async fn ingest(&self, item: &MediaItem, files: &[impl AsRef<Path>]) -> Result<()> {
        if files.is_empty() {
            anyhow::bail!("无文件可入库");
        }
        let meta = serde_json::json!({
            "source": item.source.as_str(),
            "source_id": item.source_id,
            "source_url": item.url,
            "title": item.title,
            "author_name": item.author.name,
            "author_url": item.author.url,
            "tags": item.tags,
            "is_r18": item.is_r18,
            "origin": item.origin,
        });
        let meta_str = serde_json::to_string(&meta)?;

        let mut form = Form::new().text("meta", meta_str);
        for (i, path) in files.iter().enumerate() {
            let path = path.as_ref();
            let bytes = std::fs::read(path)
                .with_context(|| format!("读入库文件失败: {}", path.display()))?;
            let filename = path
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_string)
                .unwrap_or_else(|| format!("p{i:02}.jpg"));
            let ct = content_type_for(&filename);
            let part = Part::bytes(bytes)
                .file_name(filename)
                .mime_str(ct)
                .context("构造 multipart part 失败")?;
            form = form.part("files", part);
        }

        let url = format!("{}/api/ingest", self.endpoint);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .multipart(form)
            .send()
            .await
            .context("请求 Vitrine /api/ingest 失败")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("图库入库 HTTP {status}: {body}");
        }
        tracing::info!(
            source = item.source.as_str(),
            id = %item.source_id,
            "图库入库成功: {body}"
        );
        Ok(())
    }
}

fn content_type_for(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else {
        "image/jpeg"
    }
}
