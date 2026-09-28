use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::Url;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;

use crate::image_dedup::{init_schema, inspect_image_bytes, ImageFingerprint, RegionFingerprint};
use crate::model::SourceKind;

const PAGE_SIZE: usize = 100;

#[derive(Debug, Clone, Deserialize)]
pub struct CatalogImageRecord {
    pub work_id: String,
    pub source: SourceKind,
    pub source_id: String,
    pub source_url: String,
    pub title: String,
    pub page_index: u32,
    pub r2_key: String,
    pub byte_size: u64,
    pub content_type: String,
    pub sha256: String,
}

#[derive(Debug, Deserialize)]
struct CatalogPage {
    ok: bool,
    images: Vec<CatalogImageRecord>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CatalogSyncSummary {
    pub listed: usize,
    pub imported: usize,
    pub unchanged: usize,
    pub failed: usize,
    pub pushed: usize,
}

pub fn catalog_media_url(endpoint: &str, r2_key: &str) -> Result<String> {
    let mut url = Url::parse(endpoint.trim_end_matches('/')).context("Vitrine endpoint 无效")?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Vitrine endpoint 不能作为基础 URL"))?;
        segments.pop_if_empty();
        segments.push("media");
        for segment in r2_key.split('/') {
            segments.push(segment);
        }
    }
    Ok(url.to_string())
}

pub fn needs_catalog_image(conn: &Connection, image: &CatalogImageRecord) -> Result<bool> {
    let exists = conn
        .query_row(
            "SELECT 1 FROM image_fingerprints
             WHERE source_kind=?1 AND source_id=?2 AND image_index=?3
               AND status='published' AND content_sha256=?4
               AND regions_json != '[]'",
            params![
                image.source.as_str(),
                image.source_id,
                i64::from(image.page_index),
                image.sha256,
            ],
            |_| Ok(()),
        )
        .optional()?
        .is_none();
    Ok(exists)
}

pub fn import_catalog_image(
    conn: &Connection,
    image: &CatalogImageRecord,
    fingerprint: &ImageFingerprint,
) -> Result<bool> {
    if !needs_catalog_image(conn, image)? {
        return Ok(false);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    conn.execute(
        "INSERT INTO image_fingerprints(
            source_kind,source_id,image_index,title,source_url,status,
            content_sha256,strict_key,average_hash,difference_hash,color_key,detail_key,
            width,height,bytes,format,regions_json,recorded_at,solid_color
         ) VALUES(?1,?2,?3,?4,?5,'published',?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)
         ON CONFLICT(source_kind,source_id,image_index) DO UPDATE SET
            title=excluded.title,
            source_url=excluded.source_url,
            status='published',
            content_sha256=excluded.content_sha256,
            strict_key=excluded.strict_key,
            average_hash=excluded.average_hash,
            difference_hash=excluded.difference_hash,
            color_key=excluded.color_key,
            detail_key=excluded.detail_key,
            width=excluded.width,
            height=excluded.height,
            bytes=excluded.bytes,
            format=excluded.format,
            regions_json=excluded.regions_json,
            recorded_at=excluded.recorded_at,
            solid_color=excluded.solid_color",
        params![
            image.source.as_str(),
            image.source_id,
            i64::from(image.page_index),
            image.title,
            image.source_url,
            fingerprint.content_sha256,
            fingerprint.strict_key,
            format!("{:016x}", fingerprint.average_hash),
            format!("{:016x}", fingerprint.difference_hash),
            fingerprint.color_key,
            fingerprint.detail_key,
            i64::from(fingerprint.width),
            i64::from(fingerprint.height),
            fingerprint.bytes as i64,
            fingerprint.format,
            serde_json::to_string(&fingerprint.regions)?,
            now,
            fingerprint.solid_color,
        ],
    )?;
    Ok(true)
}

pub fn fingerprint_upload_json(r2_key: &str, fingerprint: &ImageFingerprint) -> serde_json::Value {
    serde_json::json!({
        "r2_key": r2_key,
        "solid_color": fingerprint.solid_color,
        "content_sha256": fingerprint.content_sha256,
        "strict_key": fingerprint.strict_key,
        "average_hash": format!("{:016x}", fingerprint.average_hash),
        "difference_hash": format!("{:016x}", fingerprint.difference_hash),
        "color_key": fingerprint.color_key,
        "detail_key": fingerprint.detail_key,
        "width": fingerprint.width,
        "height": fingerprint.height,
        "bytes": fingerprint.bytes,
        "format": fingerprint.format,
        "regions": fingerprint.regions.iter().map(|region| serde_json::json!({
            "width": region.width,
            "height": region.height,
            "average_hash": format!("{:016x}", region.average_hash),
            "difference_hash": format!("{:016x}", region.difference_hash),
            "color_key": region.color_key,
        })).collect::<Vec<_>>(),
    })
}

fn load_fingerprint(
    conn: &Connection,
    image: &CatalogImageRecord,
) -> Result<Option<ImageFingerprint>> {
    conn.query_row(
        "SELECT content_sha256,strict_key,average_hash,difference_hash,color_key,detail_key,
                width,height,bytes,format,COALESCE(regions_json,'[]'),solid_color
         FROM image_fingerprints
         WHERE source_kind=?1 AND source_id=?2 AND image_index=?3 AND status='published'",
        params![
            image.source.as_str(),
            image.source_id,
            i64::from(image.page_index),
        ],
        |row| {
            Ok(ImageFingerprint {
                content_sha256: row.get(0)?,
                strict_key: row.get(1)?,
                average_hash: u64::from_str_radix(&row.get::<_, String>(2)?, 16).unwrap_or(0),
                difference_hash: u64::from_str_radix(&row.get::<_, String>(3)?, 16).unwrap_or(0),
                color_key: row.get(4)?,
                detail_key: row.get(5)?,
                width: row.get::<_, i64>(6)?.max(0) as u32,
                height: row.get::<_, i64>(7)?.max(0) as u32,
                bytes: row.get::<_, i64>(8)?.max(0) as u64,
                format: row.get(9)?,
                regions: serde_json::from_str::<Vec<RegionFingerprint>>(&row.get::<_, String>(10)?)
                    .unwrap_or_default(),
                solid_color: row.get(11)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

pub async fn sync_gallery_fingerprints(
    db_path: &Path,
    endpoint: &str,
    token: &str,
) -> Result<CatalogSyncSummary> {
    let endpoint = endpoint.trim_end_matches('/');
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .connect_timeout(Duration::from_secs(15))
        .user_agent(concat!("hanabi-catalog-sync/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("构造图库指纹同步客户端失败")?;
    let conn = Connection::open(db_path).context("打开图片指纹数据库失败")?;
    conn.busy_timeout(Duration::from_secs(5))?;
    init_schema(&conn)?;
    let conn = Arc::new(Mutex::new(conn));

    let mut summary = CatalogSyncSummary::default();
    let mut offset = 0_usize;
    let mut pending_upload: Vec<serde_json::Value> = Vec::new();
    loop {
        let response = client
            .get(format!("{endpoint}/api/catalog"))
            .bearer_auth(token)
            .query(&[("limit", PAGE_SIZE), ("offset", offset)])
            .send()
            .await
            .context("请求 Vitrine 图片目录失败")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("Vitrine 图片目录 HTTP {status}: {body}");
        }
        let page: CatalogPage = response.json().await.context("解析 Vitrine 图片目录失败")?;
        if !page.ok {
            bail!("Vitrine 图片目录返回 ok=false");
        }
        let count = page.images.len();
        if count == 0 {
            break;
        }
        for image in page.images {
            summary.listed += 1;
            let needs_image = {
                let conn = conn
                    .lock()
                    .map_err(|_| anyhow::anyhow!("图片指纹数据库锁损坏"))?;
                needs_catalog_image(&conn, &image)?
            };
            if needs_image {
                let result = async {
                    let media_url = catalog_media_url(endpoint, &image.r2_key)?;
                    let response = client
                        .get(media_url)
                        .send()
                        .await
                        .context("下载 Vitrine 原图失败")?
                        .error_for_status()
                        .context("下载 Vitrine 原图返回错误状态")?;
                    let bytes = response.bytes().await.context("读取 Vitrine 原图失败")?;
                    let fingerprint =
                        tokio::task::spawn_blocking(move || inspect_image_bytes(&bytes))
                            .await
                            .context("图片指纹计算任务失败")??;
                    if !image.sha256.is_empty() && fingerprint.content_sha256 != image.sha256 {
                        bail!("Vitrine 原图 SHA-256 与目录不一致: {}", image.work_id);
                    }
                    let conn = conn
                        .lock()
                        .map_err(|_| anyhow::anyhow!("图片指纹数据库锁损坏"))?;
                    import_catalog_image(&conn, &image, &fingerprint)
                }
                .await;
                match result {
                    Ok(true) => summary.imported += 1,
                    Ok(false) => summary.unchanged += 1,
                    Err(error) => {
                        summary.failed += 1;
                        tracing::warn!(
                            error = %error,
                            source = image.source.as_str(),
                            id = %image.source_id,
                            page = image.page_index,
                            "图库图片指纹同步失败"
                        );
                        continue;
                    }
                }
            } else {
                summary.unchanged += 1;
            }
            let fingerprint = {
                let conn = conn
                    .lock()
                    .map_err(|_| anyhow::anyhow!("图片指纹数据库锁损坏"))?;
                load_fingerprint(&conn, &image)?
            };
            if let Some(fingerprint) = fingerprint {
                pending_upload.push(fingerprint_upload_json(&image.r2_key, &fingerprint));
                if pending_upload.len() == 50 {
                    summary.pushed +=
                        push_fingerprints(&client, endpoint, token, &mut pending_upload).await;
                }
            }
        }
        offset += count;
        if count < PAGE_SIZE {
            break;
        }
    }
    summary.pushed += push_fingerprints(&client, endpoint, token, &mut pending_upload).await;
    Ok(summary)
}

async fn push_fingerprints(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    batch: &mut Vec<serde_json::Value>,
) -> usize {
    if batch.is_empty() {
        return 0;
    }
    let count = batch.len();
    let body = serde_json::json!({ "images": std::mem::take(batch) });
    match client
        .put(format!("{endpoint}/api/catalog/fingerprints"))
        .bearer_auth(token)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => count,
        Ok(response) => {
            tracing::warn!(
                status = %response.status(),
                "上传图库指纹失败"
            );
            0
        }
        Err(error) => {
            tracing::warn!(error = %error, "上传图库指纹失败");
            0
        }
    }
}
