//! Vitrine 图库入库客户端：把本地图片 + 作品元数据 POST 到 CF Workers。

use std::path::Path;

use anyhow::{Context, Result};
use rayon::prelude::*;
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

const MAX_GALLERY_TITLE_BYTES: usize = 512;

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
    pub(crate) async fn review_image(
        &self,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let response = self
            .client
            .post(format!("{}/api/catalog/image-review", self.endpoint))
            .bearer_auth(&self.token)
            .json(request)
            .send()
            .await?;
        let status = response.status();
        let result: serde_json::Value = response.json().await?;
        if !status.is_success() || result["ok"] != true {
            anyhow::bail!(
                "图库单图操作未完成（{status}）：{}",
                result["error"].as_str().unwrap_or("响应无效")
            );
        }
        Ok(result)
    }

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

    /// Read the author only from an exact cached Douyin work, never a fuzzy match.
    pub async fn douyin_author(&self, source_id: &str) -> Result<Option<String>> {
        anyhow::ensure!(
            !source_id.is_empty() && source_id.bytes().all(|byte| byte.is_ascii_digit()),
            "不是有效的抖音作品 ID"
        );
        let response: serde_json::Value = self
            .client
            .get(format!("{}/api/works", self.endpoint))
            .bearer_auth(&self.token)
            .query(&[("source", "douyin"), ("q", source_id), ("limit", "100")])
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        anyhow::ensure!(response["ok"] == true, "图库作者查询响应无效");
        let works = response["works"]
            .as_array()
            .context("图库作者查询缺少作品列表")?;
        let mut author = None;
        for work in works {
            if work["source"].as_str() != Some("douyin")
                || work["source_id"].as_str() != Some(source_id)
            {
                continue;
            }
            let Some(profile) = work["author_url"]
                .as_str()
                .and_then(crate::source::douyin::canonical_user_profile)
            else {
                return Ok(None);
            };
            if author.as_ref().is_some_and(|known| known != &profile) {
                return Ok(None);
            }
            author = Some(profile);
        }
        Ok(author)
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

        let paths: Vec<std::path::PathBuf> = files
            .iter()
            .map(|path| path.as_ref().to_path_buf())
            .collect();
        // Reading and decoding originals is blocking CPU/IO work; keep it off the runtime.
        let (payloads, files) = tokio::task::spawn_blocking(move || {
            let payloads = paths
                .par_iter()
                .enumerate()
                .map(|(i, path)| {
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
                    Ok((filename, ct, bytes))
                })
                .collect::<std::result::Result<Vec<_>, GalleryIngestError>>()?;
            // The gallery gets strips joined back into the picture; Telegram keeps the pages.
            let payloads = crate::stitch::stitch_pages(payloads);
            // Vitrine keeps the best copy of strict duplicates using these fingerprints.
            let files: Vec<serde_json::Value> = payloads
                .par_iter()
                .map(|(name, content_type, bytes)| {
                    serde_json::json!({
                        "name": name,
                        "content_type": content_type,
                        "size": bytes.len(),
                        "sha256": format!("{:x}", Sha256::digest(bytes)),
                        "fingerprint": crate::image_dedup::inspect_image_bytes(bytes)
                            .ok()
                            .map(|fp| fingerprint_json(&fp)),
                    })
                })
                .collect();
            Ok::<_, GalleryIngestError>((payloads, files))
        })
        .await
        .map_err(|error| GalleryIngestError::permanent(format!("计算图库指纹失败: {error}")))??;
        let key = idempotency_key_from_bytes(item, &meta_str, &payloads);
        let mut manifest = serde_json::json!({ "meta": meta_str, "files": files });

        // Originals go straight to R2 via presigned URLs; Vitrine only signs and
        // records them, which keeps its per-request CPU tiny.
        let prepared = self.ingest_step("prepare", &key, &manifest).await?;
        let Some(uploads) = prepared["uploads"].as_array() else {
            // A replayed Idempotency-Key: Vitrine already recorded this exact ingest.
            tracing::info!(
                source = item.source.as_str(),
                id = %item.source_id,
                "图库入库成功(幂等重放): {prepared}"
            );
            return Ok(());
        };
        if uploads.len() != payloads.len() {
            return Err(GalleryIngestError::permanent(format!(
                "图库返回 {} 个上传地址，预期 {}",
                uploads.len(),
                payloads.len()
            )));
        }
        // Pages upload concurrently; R2 bills per PUT, not per connection.
        let mut puts = tokio::task::JoinSet::new();
        for (upload, (_, _, bytes)) in uploads.iter().zip(payloads) {
            let url = upload["url"]
                .as_str()
                .ok_or_else(|| GalleryIngestError::permanent("图库上传地址缺失"))?;
            let mut request = self.client.put(url).body(bytes);
            for (name, value) in upload["headers"].as_object().into_iter().flatten() {
                request = request.header(name, value.as_str().unwrap_or_default());
            }
            puts.spawn(async move {
                let response = request.send().await.map_err(|error| {
                    GalleryIngestError::transient(format!("图库原图直传 R2 失败: {error}"))
                })?;
                let status = response.status();
                if status.is_success() {
                    return Ok(());
                }
                let body = response.text().await.unwrap_or_default();
                let message = format!("图库原图直传 R2 HTTP {status}: {body}");
                Err(if retryable_status(status) {
                    GalleryIngestError::transient(message)
                } else {
                    GalleryIngestError::permanent(message)
                })
            });
        }
        while let Some(result) = puts.join_next().await {
            result.map_err(|error| {
                GalleryIngestError::transient(format!("图库原图直传任务异常: {error}"))
            })??;
        }

        manifest["ingest_id"] = prepared["ingest_id"].clone();
        let committed = self.ingest_step("commit", &key, &manifest).await?;
        tracing::info!(
            source = item.source.as_str(),
            id = %item.source_id,
            "图库入库成功: {committed}"
        );
        Ok(())
    }

    async fn ingest_step(
        &self,
        step: &str,
        idempotency_key: &str,
        manifest: &serde_json::Value,
    ) -> std::result::Result<serde_json::Value, GalleryIngestError> {
        let response = self
            .client
            .post(format!("{}/api/ingest/{step}", self.endpoint))
            .bearer_auth(&self.token)
            .header("Idempotency-Key", idempotency_key)
            .json(manifest)
            .send()
            .await
            .map_err(|error| {
                GalleryIngestError::transient(format!(
                    "请求 Vitrine /api/ingest/{step} 失败: {error}"
                ))
            })?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let message = format!("图库入库 HTTP {status}: {body}");
            return Err(if retryable_status(status) {
                GalleryIngestError::transient(message)
            } else {
                GalleryIngestError::permanent(message)
            });
        }
        match serde_json::from_str::<serde_json::Value>(&body) {
            Ok(value) if value["ok"] == true => Ok(value),
            _ => Err(GalleryIngestError::permanent(format!(
                "图库 /api/ingest/{step} 响应无效: {body}"
            ))),
        }
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
}

#[derive(Debug, serde::Serialize)]
struct CatalogRetractRequest<'a> {
    decision_id: &'a str,
    work_id: &'a str,
}

pub fn fingerprint_json(fp: &crate::image_dedup::ImageFingerprint) -> serde_json::Value {
    let hash = |value: u64| format!("{value:016x}");
    serde_json::json!({
        "solid_color": fp.solid_color,
        "content_sha256": fp.content_sha256,
        "strict_key": fp.strict_key,
        "average_hash": hash(fp.average_hash),
        "difference_hash": hash(fp.difference_hash),
        "color_key": fp.color_key,
        "detail_key": fp.detail_key,
        "width": fp.width,
        "height": fp.height,
        "bytes": fp.bytes,
        "format": fp.format,
        "regions": fp.regions.iter().map(|region| serde_json::json!({
            "width": region.width,
            "height": region.height,
            "average_hash": hash(region.average_hash),
            "difference_hash": hash(region.difference_hash),
            "color_key": region.color_key,
        })).collect::<Vec<_>>(),
    })
}

fn gallery_meta(
    item: &MediaItem,
    publication: Option<&GalleryPublication>,
) -> serde_json::Result<String> {
    let title = item
        .title
        .as_deref()
        .map(|title| truncate_utf8_bytes(title, MAX_GALLERY_TITLE_BYTES));
    let mut meta = serde_json::json!({
        "source": item.source.as_str(),
        "source_id": item.source_id,
        "source_url": item.url,
        "title": title,
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

fn truncate_utf8_bytes(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
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
    use super::{
        gallery_meta, idempotency_key, retryable_status, GalleryClient, MAX_GALLERY_TITLE_BYTES,
    };
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

    #[tokio::test]
    async fn douyin_author_requires_exact_work_and_unambiguous_valid_profile() {
        use std::io::{Read, Write};
        let cases = [
            (
                serde_json::json!([
                    {"source":"douyin","source_id":"123","author_url":"https://www.douyin.com/user/MS4wA"}
                ]),
                Some("https://www.douyin.com/user/MS4wA"),
            ),
            (
                serde_json::json!([
                    {"source":"douyin","source_id":"1234","author_url":"https://www.douyin.com/user/MS4wA"},
                    {"source":"pixiv","source_id":"123","author_url":"https://www.douyin.com/user/MS4wA"}
                ]),
                None,
            ),
            (
                serde_json::json!([
                    {"source":"douyin","source_id":"123","author_url":"https://evil.example/user/MS4wA"}
                ]),
                None,
            ),
            (
                serde_json::json!([
                    {"source":"douyin","source_id":"123","author_url":"https://www.douyin.com/note/123"}
                ]),
                None,
            ),
            (
                serde_json::json!([
                    {"source":"douyin","source_id":"123","author_url":"https://www.douyin.com/user/MS4wA"},
                    {"source":"douyin","source_id":"123","author_url":"https://www.douyin.com/user/MS4wB"}
                ]),
                None,
            ),
            (
                serde_json::json!([
                    {"source":"douyin","source_id":"123","author_url":"https://www.douyin.com/user/MS4wA"},
                    {"source":"douyin","source_id":"123","author_url":"https://evil.example/user/MS4wA"}
                ]),
                None,
            ),
            (
                serde_json::json!([
                    {"source":"douyin","source_id":"123","author_url":"https://www.douyin.com/user/MS4wA"},
                    {"source":"douyin","source_id":"123","author_url":"https://www.iesdouyin.com/share/user/MS4wA/?s=1"}
                ]),
                Some("https://www.douyin.com/user/MS4wA"),
            ),
        ];
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let bodies: Vec<_> = cases
            .iter()
            .map(|(works, _)| serde_json::json!({"ok":true,"works":works}).to_string())
            .collect();
        let server = std::thread::spawn(move || {
            for body in bodies {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                    let mut buffer = [0; 1024];
                    let length = socket.read(&mut buffer).unwrap();
                    assert!(length > 0);
                    request.extend_from_slice(&buffer[..length]);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request.starts_with("GET /api/works?source=douyin&q=123&limit=100 "));
                assert!(request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer fake-token\r\n"));
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            }
        });
        let mut gallery = GalleryClient::new(endpoint, "fake-token".into()).unwrap();
        gallery.client = reqwest::Client::builder().no_proxy().build().unwrap();
        assert!(gallery.douyin_author("../123").await.is_err());
        for (_, expected) in cases {
            assert_eq!(
                gallery.douyin_author("123").await.unwrap().as_deref(),
                expected
            );
        }
        server.join().unwrap();
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
    fn gallery_meta_truncates_title_at_utf8_boundary() {
        let mut item = item();
        item.title = Some("中".repeat(171));

        let meta: serde_json::Value =
            serde_json::from_str(&gallery_meta(&item, None).unwrap()).unwrap();
        let title = meta["title"].as_str().unwrap();
        assert_eq!(title, "中".repeat(170));
        assert!(title.len() <= MAX_GALLERY_TITLE_BYTES);
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
