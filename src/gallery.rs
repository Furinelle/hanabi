//! Vitrine 图库入库客户端：把本地图片 + 作品元数据 POST 到 CF Workers。

use std::path::{Path, PathBuf};

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

const CATALOG_WORK_PAGE_SIZE: usize = 100;

#[derive(Debug, serde::Deserialize)]
struct CatalogPage {
    ok: bool,
    images: Vec<CatalogImage>,
}

#[derive(Debug, serde::Deserialize)]
struct CatalogImage {
    work_id: String,
    page_index: u32,
    r2_key: String,
    content_type: String,
}

#[derive(Debug, Clone)]
pub(crate) struct GalleryWorkImage {
    pub(crate) r2_key: String,
    pub(crate) path: PathBuf,
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

    pub(crate) async fn download_work_images(
        &self,
        work_id: &str,
        destination: &Path,
    ) -> std::result::Result<Vec<GalleryWorkImage>, GalleryIngestError> {
        let mut images = Vec::new();
        let mut offset = 0_usize;
        loop {
            let url = (if offset == 0 {
                catalog_work_url(&self.endpoint, work_id)
            } else {
                catalog_work_page_url(&self.endpoint, work_id, offset)
            })
            .map_err(|error| {
                GalleryIngestError::permanent(format!("构造 Vitrine 作品目录地址失败: {error}"))
            })?;
            let response = self
                .client
                .get(url)
                .bearer_auth(&self.token)
                .send()
                .await
                .map_err(|error| {
                    GalleryIngestError::transient(format!("请求 Vitrine 作品目录失败: {error}"))
                })?;
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if !status.is_success() {
                let message = format!("Vitrine 作品目录 HTTP {status}: {body}");
                return Err(if retryable_status(status) {
                    GalleryIngestError::transient(message)
                } else {
                    GalleryIngestError::permanent(message)
                });
            }
            let page: CatalogPage = serde_json::from_str(&body).map_err(|error| {
                GalleryIngestError::transient(format!("解析 Vitrine 作品目录失败: {error}"))
            })?;
            if !page.ok || page.images.iter().any(|image| image.work_id != work_id) {
                return Err(GalleryIngestError::permanent(
                    "Vitrine 作品目录返回无效数据",
                ));
            }
            let count = page.images.len();
            images.extend(page.images);
            if count < CATALOG_WORK_PAGE_SIZE {
                break;
            }
            offset += count;
        }
        if images.is_empty() {
            return Err(GalleryIngestError::permanent(format!(
                "Vitrine 未找到作品: {work_id}"
            )));
        }
        images.sort_by_key(|image| image.page_index);
        let destination = destination.to_path_buf();
        let create_destination = destination.clone();
        tokio::task::spawn_blocking(move || std::fs::create_dir_all(&create_destination))
            .await
            .map_err(|error| {
                GalleryIngestError::transient(format!("创建相似图审批目录失败: {error}"))
            })?
            .map_err(|error| {
                GalleryIngestError::permanent(format!("创建相似图审批目录失败: {error}"))
            })?;

        let mut downloaded = Vec::with_capacity(images.len());
        for (index, image) in images.into_iter().enumerate() {
            let media_url = crate::gallery_sync::catalog_media_url(&self.endpoint, &image.r2_key)
                .map_err(|error| {
                GalleryIngestError::permanent(format!("构造 Vitrine 原图地址失败: {error}"))
            })?;
            let response = self
                .client
                .get(media_url)
                .send()
                .await
                .map_err(|error| {
                    GalleryIngestError::transient(format!("下载 Vitrine 原图失败: {error}"))
                })?
                .error_for_status()
                .map_err(|error| {
                    GalleryIngestError::transient(format!("下载 Vitrine 原图返回错误状态: {error}"))
                })?;
            let bytes = response.bytes().await.map_err(|error| {
                GalleryIngestError::transient(format!("读取 Vitrine 原图失败: {error}"))
            })?;
            let extension = media_extension(&image.r2_key, &image.content_type);
            let path = destination.join(format!("{index:02}.{extension}"));
            let write_path = path.clone();
            let bytes = bytes.to_vec();
            tokio::task::spawn_blocking(move || std::fs::write(&write_path, bytes))
                .await
                .map_err(|error| {
                    GalleryIngestError::transient(format!("写入相似图审批原图失败: {error}"))
                })?
                .map_err(|error| {
                    GalleryIngestError::permanent(format!("写入相似图审批原图失败: {error}"))
                })?;
            downloaded.push(GalleryWorkImage {
                r2_key: image.r2_key,
                path,
            });
        }
        Ok(downloaded)
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

    pub async fn prune_similar_works(
        &self,
        decision_id: &str,
        keep_work_id: &str,
        remove_work_ids: &[String],
    ) -> std::result::Result<GalleryWorkPruneResult, GalleryIngestError> {
        if remove_work_ids.is_empty() || remove_work_ids.len() > 20 {
            return Err(GalleryIngestError::permanent(
                "remove works must contain 1 to 20 entries",
            ));
        }
        let url = format!("{}/api/catalog/prune-works", self.endpoint);
        let response = self
            .client
            .post(url)
            .bearer_auth(&self.token)
            .json(&CatalogWorkPruneRequest {
                decision_id,
                keep_work_id,
                remove_work_ids,
            })
            .send()
            .await
            .map_err(|error| {
                GalleryIngestError::transient(format!("请求 Vitrine 整作品整理失败: {error}"))
            })?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let message = format!("Vitrine 整作品整理 HTTP {status}: {body}");
            return Err(if retryable_status(status) {
                GalleryIngestError::transient(message)
            } else {
                GalleryIngestError::permanent(message)
            });
        }
        let parsed: CatalogWorkPruneResponse = serde_json::from_str(&body).map_err(|error| {
            GalleryIngestError::transient(format!("解析 Vitrine 整作品整理响应失败: {error}"))
        })?;
        if !parsed.ok {
            return Err(GalleryIngestError::transient(
                "Vitrine 整作品整理返回 ok=false",
            ));
        }
        if parsed.telegram_targets.is_empty() {
            return Err(GalleryIngestError::permanent(
                "Vitrine 整作品整理未返回 Telegram 目标",
            ));
        }
        Ok(GalleryWorkPruneResult {
            removed_work_ids: parsed.removed_works,
            removed_r2_keys: parsed.removed_r2_keys,
            telegram_targets: parsed.telegram_targets,
            replayed: parsed.replayed,
        })
    }

    pub async fn finish_telegram_prune(
        &self,
        decision_id: &str,
    ) -> std::result::Result<(), GalleryIngestError> {
        self.post_telegram_result(decision_id, true, None).await
    }

    pub async fn retract_work(
        &self,
        decision_id: &str,
        work_id: &str,
    ) -> std::result::Result<(), GalleryIngestError> {
        let url = format!("{}/api/catalog/retract", self.endpoint);
        let response = self
            .client
            .post(url)
            .bearer_auth(&self.token)
            .json(&CatalogRetractRequest {
                decision_id,
                work_id,
            })
            .send()
            .await
            .map_err(|error| {
                GalleryIngestError::transient(format!("请求 Vitrine 撤回作品失败: {error}"))
            })?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status.as_u16() == 409 && body.contains("work not active") {
            return Ok(());
        }
        if !status.is_success() {
            let message = format!("Vitrine 撤回作品 HTTP {status}: {body}");
            return Err(if retryable_status(status) {
                GalleryIngestError::transient(message)
            } else {
                GalleryIngestError::permanent(message)
            });
        }
        Ok(())
    }

    pub async fn report_telegram_prune_failure(
        &self,
        decision_id: &str,
        error: &str,
    ) -> std::result::Result<(), GalleryIngestError> {
        self.post_telegram_result(decision_id, false, Some(error))
            .await
    }

    async fn post_telegram_result(
        &self,
        decision_id: &str,
        complete: bool,
        error: Option<&str>,
    ) -> std::result::Result<(), GalleryIngestError> {
        let url = format!("{}/api/catalog/prune-works/telegram-result", self.endpoint);
        let error = error.map(sanitize_reported_error);
        let response = self
            .client
            .post(url)
            .bearer_auth(&self.token)
            .json(&CatalogTelegramResultRequest {
                decision_id,
                complete,
                error: error.as_deref(),
            })
            .send()
            .await
            .map_err(|error| {
                GalleryIngestError::transient(format!("请求 Vitrine Telegram 结果失败: {error}"))
            })?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let message = format!("Vitrine Telegram 结果 HTTP {status}: {body}");
            return Err(if retryable_status(status) {
                GalleryIngestError::transient(message)
            } else {
                GalleryIngestError::permanent(message)
            });
        }
        Ok(())
    }
}

fn catalog_work_url(endpoint: &str, work_id: &str) -> Result<String> {
    catalog_work_page_url(endpoint, work_id, 0)
}

fn catalog_work_page_url(endpoint: &str, work_id: &str, offset: usize) -> Result<String> {
    let mut url =
        reqwest::Url::parse(endpoint.trim_end_matches('/')).context("Vitrine endpoint 无效")?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Vitrine endpoint 不能作为基础 URL"))?;
        segments.pop_if_empty();
        segments.push("api");
        segments.push("catalog");
    }
    url.query_pairs_mut()
        .append_pair("work_id", work_id)
        .append_pair("limit", &CATALOG_WORK_PAGE_SIZE.to_string())
        .append_pair("offset", &offset.to_string());
    Ok(url.to_string())
}

fn media_extension(r2_key: &str, content_type: &str) -> String {
    r2_key
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 8
                && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
        .map(str::to_owned)
        .unwrap_or_else(|| match content_type {
            "image/png" => "png".into(),
            "image/webp" => "webp".into(),
            "image/gif" => "gif".into(),
            _ => "jpg".into(),
        })
}

#[derive(Debug, serde::Serialize)]
struct CatalogRetractRequest<'a> {
    decision_id: &'a str,
    work_id: &'a str,
}

#[derive(Debug, serde::Serialize)]
struct CatalogWorkPruneRequest<'a> {
    decision_id: &'a str,
    keep_work_id: &'a str,
    remove_work_ids: &'a [String],
}

#[derive(Debug, serde::Deserialize)]
struct CatalogWorkPruneResponse {
    ok: bool,
    removed_works: Vec<String>,
    removed_r2_keys: Vec<String>,
    telegram_targets: Vec<GalleryTelegramTarget>,
    replayed: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct GalleryTelegramTarget {
    pub publication_id: String,
    pub work_id: String,
    pub chat_id: i64,
    pub message_ids: Vec<i64>,
}

#[derive(Debug, Clone)]
pub struct GalleryWorkPruneResult {
    pub removed_work_ids: Vec<String>,
    pub removed_r2_keys: Vec<String>,
    pub telegram_targets: Vec<GalleryTelegramTarget>,
    pub replayed: bool,
}

#[derive(Debug, serde::Serialize)]
struct CatalogTelegramResultRequest<'a> {
    decision_id: &'a str,
    complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

fn sanitize_reported_error(raw: &str) -> String {
    let filtered: String = raw.chars().filter(|ch| !ch.is_control()).collect();
    let truncated: String = filtered.chars().take(500).collect();
    let lower = truncated.to_ascii_lowercase();
    if lower.contains("bearer ")
        || lower.contains("authorization:")
        || lower.contains("cookie:")
        || lower.contains("set-cookie:")
    {
        return "[redacted]".into();
    }
    truncated
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
    use super::{catalog_work_url, idempotency_key, retryable_status, GalleryClient};
    use crate::model::{Author, ImageRef, MediaItem, SourceKind};
    use std::io::{Read, Write};

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

    #[test]
    fn catalog_work_url_preserves_the_exact_work_id() {
        let url = reqwest::Url::parse(
            &catalog_work_url("https://gallery.example.test/", "pixiv:123").unwrap(),
        )
        .unwrap();
        assert_eq!(url.path(), "/api/catalog");
        assert!(url
            .query_pairs()
            .any(|(key, value)| key == "work_id" && value == "pixiv:123"));
    }

    #[tokio::test]
    async fn download_work_images_fetches_the_requested_full_group() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            let bodies = [
                br#"{"ok":true,"images":[{"work_id":"pixiv:123","page_index":0,"r2_key":"pixiv/123/v/00.jpg","content_type":"image/jpeg"}]}"#.as_slice(),
                b"gallery-image".as_slice(),
            ];
            for body in bodies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4096];
                let count = stream.read(&mut request).unwrap();
                requests.push(String::from_utf8_lossy(&request[..count]).into_owned());
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
            }
            requests
        });
        let destination = tempfile::tempdir().unwrap();
        let client = GalleryClient::new(format!("http://{address}"), "test-token".into()).unwrap();

        let images = client
            .download_work_images("pixiv:123", destination.path())
            .await
            .unwrap();

        assert_eq!(images.len(), 1);
        assert_eq!(images[0].r2_key, "pixiv/123/v/00.jpg");
        assert_eq!(std::fs::read(&images[0].path).unwrap(), b"gallery-image");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/catalog?work_id=pixiv%3A123"));
        assert!(requests[1].starts_with("GET /media/pixiv/123/v/00.jpg"));
    }
}
