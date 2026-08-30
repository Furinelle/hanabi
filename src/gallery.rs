//! Vitrine 图库入库客户端：把本地图片 + 作品元数据 POST 到 CF Workers。

use std::path::Path;

use anyhow::{Context, Result};
use reqwest::multipart::{Form, Part};
use sha2::{Digest, Sha256};

use crate::model::MediaItem;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GalleryPublishState {
    Full,
    Partial,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GalleryPublication {
    pub chat_id: i64,
    pub message_ids: Vec<i32>,
    pub publish_state: GalleryPublishState,
}

#[derive(Debug, serde::Serialize)]
struct CatalogPruneRequest<'a> {
    decision_id: &'a str,
    keep_r2_key: &'a str,
    remove_r2_keys: &'a [String],
}

#[derive(Debug, serde::Deserialize)]
struct CatalogPruneResponse {
    ok: bool,
    removed: usize,
}

#[derive(Debug)]
pub struct GalleryIngestError {
    message: String,
    retryable: bool,
}

impl GalleryIngestError {
    pub fn transient(message: impl Into<String>) -> Self {
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

    pub fn is_retryable(&self) -> bool {
        self.retryable
    }
}

impl std::fmt::Display for GalleryIngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GalleryIngestError {}

#[derive(Clone)]
pub struct GalleryClient {
    endpoint: String,
    token: String,
    client: reqwest::Client,
}

impl std::fmt::Debug for GalleryClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GalleryClient")
            .field("endpoint", &self.endpoint)
            .field("token", &"[REDACTED]")
            .field("client", &self.client)
            .finish()
    }
}

impl GalleryClient {
    pub fn new(endpoint: String, token: String) -> Result<Self> {
        let endpoint = endpoint.trim_end_matches('/').to_string();
        #[allow(deprecated)]
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(180))
            .connect_timeout(std::time::Duration::from_secs(15))
            .user_agent(concat!("hanabi/", env!("CARGO_PKG_VERSION")))
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
    pub async fn ingest(
        &self,
        item: &MediaItem,
        files: &[impl AsRef<Path>],
        publication: Option<&GalleryPublication>,
    ) -> std::result::Result<(), GalleryIngestError> {
        if files.is_empty() {
            return Err(GalleryIngestError::permanent("无文件可入库"));
        }
        let meta_str = gallery_meta(item, publication).map_err(|error| {
            GalleryIngestError::permanent(format!("序列化图库元数据失败: {error}"))
        })?;

        let mut payloads = Vec::with_capacity(files.len());
        for (i, path) in files.iter().enumerate() {
            let path = path.as_ref();
            let bytes = std::fs::read(path).map_err(|error| {
                GalleryIngestError::permanent(format!(
                    "读入库文件失败: {}: {error}",
                    path.display()
                ))
            })?;
            let filename = path
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_string)
                .unwrap_or_else(|| format!("p{i:02}.jpg"));
            let ct = content_type_for(&filename);
            payloads.push((filename, ct, bytes));
        }
        let key = idempotency_key_from_bytes(item, &meta_str, &payloads);
        let mut form = Form::new().text("meta", meta_str);
        for (filename, ct, bytes) in payloads {
            let part = Part::bytes(bytes)
                .file_name(filename)
                .mime_str(ct)
                .map_err(|error| {
                    GalleryIngestError::permanent(format!("构造 multipart part 失败: {error}"))
                })?;
            form = form.part("files", part);
        }

        let url = format!("{}/api/ingest", self.endpoint);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .header("Idempotency-Key", key)
            .multipart(form)
            .send()
            .await
            .map_err(|error| {
                GalleryIngestError::transient(format!("请求 Vitrine /api/ingest 失败: {error}"))
            })?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let message = format!("图库入库 HTTP {status}: {body}");
            return Err(if retryable_status(status) {
                GalleryIngestError::transient(message)
            } else {
                GalleryIngestError::permanent(message)
            });
        }
        tracing::info!(
            source = item.source.as_str(),
            id = %item.source_id,
            "图库入库成功: {body}"
        );
        Ok(())
    }

    pub async fn prune_similar(
        &self,
        decision_id: &str,
        keep_r2_key: &str,
        remove_r2_keys: &[String],
    ) -> std::result::Result<usize, GalleryIngestError> {
        let url = format!("{}/api/catalog/prune", self.endpoint);
        let response = self
            .client
            .post(url)
            .bearer_auth(&self.token)
            .json(&CatalogPruneRequest {
                decision_id,
                keep_r2_key,
                remove_r2_keys,
            })
            .send()
            .await
            .map_err(|error| {
                GalleryIngestError::transient(format!("请求 Vitrine 相似图整理失败: {error}"))
            })?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let message = format!("Vitrine 相似图整理 HTTP {status}: {body}");
            return Err(if retryable_status(status) {
                GalleryIngestError::transient(message)
            } else {
                GalleryIngestError::permanent(message)
            });
        }
        let parsed: CatalogPruneResponse = serde_json::from_str(&body).map_err(|error| {
            GalleryIngestError::transient(format!("解析 Vitrine 相似图整理响应失败: {error}"))
        })?;
        if !parsed.ok {
            return Err(GalleryIngestError::transient(
                "Vitrine 相似图整理返回 ok=false",
            ));
        }
        Ok(parsed.removed)
    }
}

fn gallery_meta(
    item: &MediaItem,
    publication: Option<&GalleryPublication>,
) -> serde_json::Result<String> {
    let mut meta = serde_json::json!({
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
    if let Some(publication) = publication {
        meta["telegram_publication"] = serde_json::to_value(publication)?;
    }
    serde_json::to_string(&meta)
}

fn hash_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn idempotency_key_from_bytes(
    item: &MediaItem,
    meta: &str,
    payloads: &[(String, &'static str, Vec<u8>)],
) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"hanabi-gallery-ingest-v1");
    hash_field(&mut hasher, item.source.as_str().as_bytes());
    hash_field(&mut hasher, item.source_id.as_bytes());
    hash_field(&mut hasher, meta.as_bytes());
    for (_, _, bytes) in payloads {
        hash_field(&mut hasher, bytes);
    }
    format!("hanabi-{digest:x}", digest = hasher.finalize())
}

#[cfg(test)]
fn idempotency_key(item: &MediaItem, files: &[impl AsRef<Path>]) -> Result<String> {
    let meta = gallery_meta(item, None)?;
    let mut payloads = Vec::with_capacity(files.len());
    for (index, path) in files.iter().enumerate() {
        let path = path.as_ref();
        let filename = path
            .file_name()
            .and_then(|part| part.to_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("p{index:02}.jpg"));
        payloads.push((
            filename.clone(),
            content_type_for(&filename),
            std::fs::read(path)?,
        ));
    }
    Ok(idempotency_key_from_bytes(item, &meta, &payloads))
}

fn retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
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

#[cfg(test)]
mod tests {
    use super::{idempotency_key, retryable_status, GalleryClient};
    use crate::model::{Author, ImageRef, MediaItem, SourceKind};

    fn item() -> MediaItem {
        MediaItem {
            source: SourceKind::Douyin,
            source_id: "123".into(),
            author: Author {
                name: "a".into(),
                url: "u".into(),
            },
            title: Some("t".into()),
            url: "https://www.douyin.com/note/123".into(),
            tags: vec!["tag".into()],
            bookmark_count: None,
            is_r18: false,
            pixiv_type: None,
            page_count: 1,
            images: vec![ImageRef {
                url: "https://example.test/a.jpg".into(),
                referer: None,
                fallback_urls: vec![],
            }],
            origin: "test".into(),
        }
    }

    #[test]
    fn debug_output_redacts_ingest_token() {
        let client = GalleryClient::new(
            "https://gallery.example.test".into(),
            "super-secret-ingest-token".into(),
        )
        .unwrap();
        let debug = format!("{client:?}");
        assert!(!debug.contains("super-secret-ingest-token"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn idempotency_key_is_stable_for_equal_payload_and_changes_with_file_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.jpg");
        let copied = temp.path().join("copied.jpg");
        std::fs::write(&first, b"same bytes").unwrap();
        std::fs::write(&copied, b"same bytes").unwrap();

        let a = idempotency_key(&item(), &[first.as_path()]).unwrap();
        let b = idempotency_key(&item(), &[copied.as_path()]).unwrap();
        assert_eq!(a, b);

        std::fs::write(&copied, b"changed bytes").unwrap();
        let changed = idempotency_key(&item(), &[copied.as_path()]).unwrap();
        assert_ne!(a, changed);
    }

    #[test]
    fn only_transient_http_statuses_are_retryable() {
        for code in [408, 429, 500, 502, 503] {
            assert!(retryable_status(
                reqwest::StatusCode::from_u16(code).unwrap()
            ));
        }
        for code in [400, 401, 403, 404, 409, 413, 422] {
            assert!(!retryable_status(
                reqwest::StatusCode::from_u16(code).unwrap()
            ));
        }
    }
}
