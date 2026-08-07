//! 抖音图文(note/slides)解析:gallery-dl 不支持抖音,这里用 reqwest 直接抓分享页。
//! 路线(免签名,对标 versenilvis/douyin-downloader):移动端 UA 跟随短链 → note 页 HTML
//! → 抠 `window._ROUTER_DATA` JSON → 取 images[].url_list(无水印全分辨率 + 备用 CDN)、作者、desc 标签。
//! 混合图/视频 slides 页无 SSR 数据时,改请求 slidesinfo,只取 clip_type=2 的静态图片。
//! 比 pixiv/x 脆:抖音改 `_ROUTER_DATA` 结构或加验证墙时会失效,失败优雅提示即可。

use std::{
    cmp::Reverse,
    collections::HashSet,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{DouyinCfg, SourceCfg, SourceFilterCfg};
use crate::model::{Author, ImageRef, MediaItem, SourceKind};
use crate::source::Source;
use crate::store::Store;

/// 抖音 CDN 对桌面 UA 返回拦截页,必须移动端 UA(对标现有解析项目)。
const MOBILE_UA: &str = "Mozilla/5.0 (Linux; Android 11; SAMSUNG SM-G973U) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Mobile Safari/537.36";
const PAGE_MAX_ATTEMPTS: usize = 3;
const PAGE_RETRY_BASE_MS: u64 = if cfg!(test) { 1 } else { 500 };
const IMAGE_MAX_ATTEMPTS: usize = 3;
const IMAGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const IMAGE_RETRY_BASE_MS: u64 = if cfg!(test) { 1 } else { 400 };

/// 是否抖音链接(短链 v.douyin.com / www.douyin.com / iesdouyin.com)。
pub fn is_douyin_url(url: &str) -> bool {
    let host = url
        .split_once("://")
        .map(|(_, r)| r)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .to_lowercase();
    host == "douyin.com"
        || host.ends_with(".douyin.com")
        || host == "iesdouyin.com"
        || host.ends_with(".iesdouyin.com")
}

/// 从 desc 抽连续话题标签(`#tag#tag` 或 `#tag 文字`),返回(去标签后的正文, 标签列表)。
fn split_desc_tags(desc: &str) -> (String, Vec<String>) {
    let mut tags = Vec::new();
    let mut title = String::new();
    let mut chars = desc.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '#' {
            let mut tag = String::new();
            while let Some(&n) = chars.peek() {
                if n == '#' || n.is_whitespace() {
                    break;
                }
                tag.push(n);
                chars.next();
            }
            if !tag.is_empty() {
                tags.push(tag);
            }
        } else {
            title.push(c);
        }
    }
    // 折叠空白(去标签后可能留多余空格),title 更干净。
    (title.split_whitespace().collect::<Vec<_>>().join(" "), tags)
}

/// 从 note 页 HTML 抠出 `window._ROUTER_DATA` 的 JSON。
fn extract_router_data(html: &str) -> Option<Value> {
    let marker = "window._ROUTER_DATA";
    let after = &html[html.find(marker)? + marker.len()..];
    let eq = after.find('=')?;
    let brace = after[eq..].find('{')? + eq;
    let script_end = after[brace..].find("</script>")? + brace;
    let json = after[brace..script_end].trim().trim_end_matches(';').trim();
    serde_json::from_str(json).ok()
}

/// 递归找第一个同时含 `aweme_id` 与 `images` 数组的对象(即图文作品数据)。
fn find_note_item(v: &Value) -> Option<&Value> {
    match v {
        Value::Object(map) => {
            if map.contains_key("aweme_id") && map.get("images").is_some_and(|i| i.is_array()) {
                return Some(v);
            }
            map.values().find_map(find_note_item)
        }
        Value::Array(arr) => arr.iter().find_map(find_note_item),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum ImagePolicy {
    All,
    StaticSlidesOnly,
}

fn extract_media_urls(source: &Value) -> Vec<String> {
    match source {
        Value::String(url) if !url.trim().is_empty() => vec![url.clone()],
        Value::Array(values) => values
            .iter()
            .filter_map(Value::as_str)
            .filter(|url| !url.trim().is_empty())
            .map(str::to_string)
            .collect(),
        Value::Object(map) => map
            .get("url_list")
            .or_else(|| map.get("urlList"))
            .map(extract_media_urls)
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn positive_int(value: Option<&Value>) -> u64 {
    value
        .and_then(|v| {
            v.as_u64().or_else(|| {
                v.as_str()
                    .and_then(|s| s.trim().parse::<f64>().ok())
                    .filter(|n| *n > 0.0)
                    .map(|n| n as u64)
            })
        })
        .unwrap_or(0)
}

fn image_resolution_score(metadata: &Value) -> u64 {
    let width = positive_int(metadata.get("width").or_else(|| metadata.get("w")));
    let height = positive_int(metadata.get("height").or_else(|| metadata.get("h")));
    if width > 0 && height > 0 {
        width.saturating_mul(height)
    } else {
        width.max(height)
    }
}

fn is_watermarked_url(url: &str) -> bool {
    let normalized = url.to_ascii_lowercase();
    [
        "tplv-dy-water",
        "dy-water",
        "owner_watermark",
        "watermark_image",
        "watermark=1",
        "playwm",
    ]
    .iter()
    .any(|hint| normalized.contains(hint))
}

type ImageRankKey = (u8, u8, Reverse<u64>, u8);

fn ranked_image_urls(item: &Value) -> Vec<String> {
    // 与 douyin-downloader 一致：无水印列表 > 原图 > 展示图 > 通用 url_list > 下载图 > 水印图；
    // 同档优先更高分辨率、非 webp。每张图保留所有镜像供下载失败时轮换。
    let mut entries: Vec<(ImageRankKey, String)> = Vec::new();
    let mut add = |source: Option<&Value>, metadata: &Value, rank: u8| {
        for url in source.into_iter().flat_map(extract_media_urls) {
            let watermark = u8::from(rank >= 4 || is_watermarked_url(&url));
            let webp = u8::from(
                reqwest::Url::parse(&url)
                    .ok()
                    .is_some_and(|u| u.path().to_ascii_lowercase().contains(".webp")),
            );
            entries.push((
                (
                    watermark,
                    rank,
                    Reverse(image_resolution_score(metadata)),
                    webp,
                ),
                url,
            ));
        }
    };

    add(item.get("watermark_free_download_url_list"), item, 0);
    if let Some(origin) = item.get("origin_image") {
        add(Some(origin), origin, 1);
    }
    if let Some(display) = item.get("display_image") {
        add(Some(display), display, 2);
    }
    add(Some(item), item, 3);
    if let Some(download) = item.get("download_url") {
        add(Some(download), download, 4);
    }
    if let Some(download) = item.get("download_addr") {
        add(Some(download), download, 5);
    }
    add(item.get("download_url_list"), item, 6);
    if let Some(watermark) = item.get("owner_watermark_image") {
        add(Some(watermark), watermark, 7);
    }

    entries.sort_by_key(|(key, _)| *key);
    let mut seen = HashSet::new();
    entries
        .into_iter()
        .map(|(_, url)| url)
        .filter(|url| seen.insert(url.clone()))
        .collect()
}

fn gallery_items(item: &Value) -> Option<&Vec<Value>> {
    item.get("image_post_info")
        .and_then(|post| post.get("images").or_else(|| post.get("image_list")))
        .and_then(Value::as_array)
        .filter(|images| !images.is_empty())
        .or_else(|| {
            item.get("images")
                .or_else(|| item.get("image_list"))
                .and_then(Value::as_array)
        })
}

fn parse_item(item: &Value, origin: &str, image_policy: ImagePolicy) -> Option<MediaItem> {
    // aweme_id 可能是数字或字符串。
    let aweme_id = item.get("aweme_id").and_then(|v| {
        v.as_str()
            .map(String::from)
            .or_else(|| v.as_u64().map(|n| n.to_string()))
    })?;
    // 只接受纯数字 id:该值会拼进临时目录路径(hanabi_douyin_<id>),后续还会对
    // 该路径 remove_dir_all;页面数据不可信,含 `/`、`..` 会造成路径穿越。
    if aweme_id.is_empty() || !aweme_id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }

    // 每张图保留完整 url_list:首项为主地址,其余作为 CDN 故障时的备用地址。
    // slidesinfo 的 images 同时包含视频卡片:clip_type=2 才是静态图;
    // 其他 clip_type(1/3/4)以及显式带 video 的条目都不应当成图片封面发布。
    let images: Vec<ImageRef> = gallery_items(item)?
        .iter()
        .filter(|img| match image_policy {
            ImagePolicy::All => true,
            ImagePolicy::StaticSlidesOnly => {
                img.get("clip_type").and_then(Value::as_u64) == Some(2)
                    || (img.get("clip_type").is_none()
                        && img.get("video").is_none_or(Value::is_null))
            }
        })
        .filter_map(|img| {
            let urls = ranked_image_urls(img);
            let url = urls.first()?.clone();
            let mut seen = HashSet::from([url.as_str()]);
            let fallback_urls = urls
                .iter()
                .skip(1)
                .filter(|candidate| seen.insert(candidate.as_str()))
                .cloned()
                .collect();
            Some(ImageRef {
                url,
                referer: None,
                fallback_urls,
            })
        })
        .collect();
    if images.is_empty() {
        return None;
    }

    let author = item.get("authorInfo").or_else(|| item.get("author"));
    let nickname = author
        .and_then(|a| a.get("nickname"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let sec_uid = author
        .and_then(|a| a.get("sec_uid"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let desc = item.get("desc").and_then(|v| v.as_str()).unwrap_or("");
    let (title, tags) = split_desc_tags(desc);

    let page_count = images.len() as u32;
    Some(MediaItem {
        source: SourceKind::Douyin,
        source_id: aweme_id.clone(),
        author: Author {
            name: nickname,
            url: format!("https://www.douyin.com/user/{sec_uid}"),
        },
        title: if title.is_empty() { None } else { Some(title) },
        url: format!("https://www.douyin.com/note/{aweme_id}"),
        tags,
        bookmark_count: None,
        is_r18: false,
        pixiv_type: None,
        page_count,
        images,
        origin: origin.to_string(),
    })
}

/// 解析作者作品接口中的一个 aweme。纯视频没有图库字段，会被自然跳过。
pub fn parse_user_aweme(item: &Value, origin: &str) -> Option<MediaItem> {
    parse_item(item, origin, ImagePolicy::All)
}

#[derive(Debug, Serialize)]
struct FeedBridgeRequest {
    target: String,
    max_pages: u32,
    browser_fallback: bool,
    browser_headless: bool,
    known_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct FeedBridgeResponse {
    #[serde(default)]
    sec_user_id: String,
    #[serde(default)]
    pages_fetched: u32,
    #[serde(default)]
    restricted: bool,
    #[serde(default)]
    browser_fallback_used: bool,
    #[serde(default)]
    items: Vec<Value>,
}

fn truncate_stderr(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let mut compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() > 1200 {
        compact = compact.chars().take(1200).collect::<String>() + "…";
    }
    compact
}

fn run_feed_helper(
    runtime: &DouyinCfg,
    target: String,
    known_ids: Vec<String>,
) -> Result<FeedBridgeResponse> {
    let helper_path =
        std::env::var("HANABI_DOUYIN_HELPER").unwrap_or_else(|_| runtime.helper_path.clone());
    let request = FeedBridgeRequest {
        target,
        max_pages: runtime.max_pages,
        browser_fallback: runtime.browser_fallback,
        browser_headless: runtime.browser_headless,
        known_ids,
    };
    let input = serde_json::to_vec(&request).context("序列化抖音作者桥接请求失败")?;

    let mut command = Command::new(&runtime.python_command);
    command.arg("-u").arg(&helper_path);
    if !runtime.cookie_file.trim().is_empty() {
        command.env("HANABI_DOUYIN_COOKIE_FILE", runtime.cookie_file.trim());
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "启动抖音作者桥接器失败: command={} helper={helper_path}",
                runtime.python_command
            )
        })?;
    child
        .stdin
        .take()
        .context("抖音作者桥接器 stdin 不可用")?
        .write_all(&input)
        .context("写入抖音作者桥接请求失败")?;
    let output = child.wait_with_output().context("等待抖音作者桥接器失败")?;
    if !output.status.success() {
        anyhow::bail!(
            "抖音作者桥接器失败 exit={}: {}",
            output.status,
            truncate_stderr(&output.stderr)
        );
    }
    serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "解析抖音作者桥接器 JSON 失败(stdout={} bytes, stderr={})",
            output.stdout.len(),
            truncate_stderr(&output.stderr)
        )
    })
}

/// 定时抓取抖音作者主页公开图文作品。签名、Cookie 与可选 Playwright 兜底由
/// `tools/douyin_user_feed.py` 调用 douyin-downloader 处理，Rust 侧只接收原始 aweme JSON。
pub struct DouyinUserSource {
    cfg: SourceCfg,
    runtime: DouyinCfg,
}

impl DouyinUserSource {
    pub fn new(cfg: SourceCfg, runtime: DouyinCfg) -> Result<Self> {
        if runtime.python_command.trim().is_empty() {
            anyhow::bail!("[douyin].python_command 不能为空");
        }
        if runtime.helper_path.trim().is_empty() {
            anyhow::bail!("[douyin].helper_path 不能为空");
        }
        if runtime.max_pages == 0 || runtime.max_pages > 100 {
            anyhow::bail!("[douyin].max_pages 必须在 1..=100");
        }
        Ok(Self { cfg, runtime })
    }
}

#[async_trait]
impl Source for DouyinUserSource {
    fn name(&self) -> &str {
        &self.cfg.name
    }

    fn filter_cfg(&self) -> &SourceFilterCfg {
        &self.cfg.filters
    }

    async fn fetch(&self, _store: &Store) -> Result<Vec<MediaItem>> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for target in self.cfg.targets.clone() {
            let runtime = self.runtime.clone();
            let target_for_log = target.clone();
            // 最终增量幂等由 pipeline 的 SQLite already_pushed 统一完成。
            let known = Vec::new();
            let response =
                tokio::task::spawn_blocking(move || run_feed_helper(&runtime, target, known))
                    .await
                    .context("抖音作者桥接任务 join 失败")??;

            let raw_count = response.items.len();
            let before = out.len();
            for raw in response.items {
                if let Some(item) = parse_user_aweme(&raw, &self.cfg.name) {
                    if seen.insert(item.source_id.clone()) {
                        out.push(item);
                    }
                }
            }
            tracing::info!(
                source = %self.cfg.name,
                target = %target_for_log,
                sec_user_id = %response.sec_user_id,
                pages = response.pages_fetched,
                restricted = response.restricted,
                browser_fallback = response.browser_fallback_used,
                raw_items = raw_count,
                image_items = out.len() - before,
                "抖音作者作品发现完成"
            );
        }
        Ok(out)
    }
}

/// 解析 note 页 HTML → MediaItem。纯函数(便于测试),网络抓取见 `fetch_note`。
pub fn parse_note(html: &str, origin: &str) -> Option<MediaItem> {
    let root = extract_router_data(html)?;
    let item = find_note_item(&root)?;
    parse_item(item, origin, ImagePolicy::All)
}

/// 解析 slidesinfo JSON,只保留静态图片,忽略视频/动态照片卡片。
fn parse_slides_info(body: &str, origin: &str) -> Option<MediaItem> {
    let root: Value = serde_json::from_str(body).ok()?;
    if root.get("status_code").and_then(Value::as_i64) != Some(0) {
        return None;
    }
    let item = find_note_item(&root)?;
    parse_item(item, origin, ImagePolicy::StaticSlidesOnly)
}

fn slides_id(url: &reqwest::Url) -> Option<&str> {
    let mut segments = url.path_segments()?;
    if segments.next()? != "share" || segments.next()? != "slides" {
        return None;
    }
    let id = segments.next()?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(id)
}

/// 构造抖音抓取用的 reqwest client:移动端 UA + 跟随短链跳转 + trust_dns(musl 静态二进制
/// getaddrinfo 会失败,同 teloxide 客户端的处理)。
pub fn build_client() -> Result<reqwest::Client> {
    #[allow(deprecated)]
    reqwest::Client::builder()
        .user_agent(MOBILE_UA)
        .redirect(reqwest::redirect::Policy::limited(10))
        .timeout(std::time::Duration::from_secs(30))
        .trust_dns(true)
        .build()
        .context("构造 douyin reqwest client 失败")
}

/// 抓 note 页并解析为 MediaItem。`url` 可为短链(自动跟随跳转)。
pub async fn fetch_note(client: &reqwest::Client, url: &str, origin: &str) -> Result<MediaItem> {
    fetch_note_with_validator(client, url, origin, is_douyin_url).await
}

async fn fetch_note_with_validator<F>(
    client: &reqwest::Client,
    url: &str,
    origin: &str,
    final_url_allowed: F,
) -> Result<MediaItem>
where
    F: Fn(&str) -> bool,
{
    let mut last_error = None;
    for attempt in 1..=PAGE_MAX_ATTEMPTS {
        let result: Result<(MediaItem, String, u16, usize)> = async {
            let resp =
                client.get(url).send().await.map_err(|error| {
                    anyhow::anyhow!("抖音页面请求失败: {}", error.without_url())
                })?;
            let status = resp.status();
            let final_url = resp.url().clone();
            let mut safe_final_url = final_url.clone();
            safe_final_url.set_query(None);
            safe_final_url.set_fragment(None);

            // 入口只校验了用户提交的原始 URL,client 会跟随最多 10 次重定向;
            // 落点若跳出抖音域,页面内容完全不可信,拒绝解析。
            if !final_url_allowed(final_url.as_str()) {
                anyhow::bail!("抖音链接重定向到非抖音域名,拒绝解析: {}", safe_final_url);
            }

            if !status.is_success() {
                anyhow::bail!("抖音页面响应状态异常 status={status} final_url={safe_final_url}");
            }
            let html = resp
                .text()
                .await
                .map_err(|error| anyhow::anyhow!("抖音页面读取失败: {}", error.without_url()))?;
            let response_bytes = html.len();
            let item = if let Some(id) = slides_id(&final_url) {
                let mut api_url = final_url.clone();
                api_url.set_path("/web/api/v2/aweme/slidesinfo/");
                api_url.set_query(None);
                api_url.set_fragment(None);
                api_url
                    .query_pairs_mut()
                    .append_pair("aweme_ids", &format!("[{id}]"))
                    .append_pair("request_source", "200");
                let api_resp = client
                    .get(api_url)
                    .header(reqwest::header::REFERER, final_url.as_str())
                    .send()
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("抖音 slidesinfo 请求失败: {}", error.without_url())
                    })?;
                let api_status = api_resp.status();
                let api_final_url = api_resp.url().clone();
                if !final_url_allowed(api_final_url.as_str()) {
                    anyhow::bail!("抖音 slidesinfo 重定向到非抖音域名,拒绝解析");
                }
                if !api_status.is_success() {
                    anyhow::bail!("抖音 slidesinfo 响应状态异常 status={api_status}");
                }
                let body = api_resp.text().await.map_err(|error| {
                    anyhow::anyhow!("抖音 slidesinfo 读取失败: {}", error.without_url())
                })?;
                parse_slides_info(&body, origin).with_context(|| {
                    format!(
                        "抖音 slidesinfo 解析失败(无静态图片 / 结构变更 / 验证墙) \
                         status={api_status} response_bytes={}",
                        body.len()
                    )
                })?
            } else {
                parse_note(&html, origin).with_context(|| {
                    format!(
                        "抖音页面解析失败(无 _ROUTER_DATA / 结构变更 / 验证墙) \
                         status={status} final_url={safe_final_url} response_bytes={response_bytes}"
                    )
                })?
            };
            Ok((
                item,
                safe_final_url.to_string(),
                status.as_u16(),
                response_bytes,
            ))
        }
        .await;

        match result {
            Ok((item, final_url, status, response_bytes)) => {
                if attempt > 1 {
                    tracing::info!(
                        attempt,
                        max_attempts = PAGE_MAX_ATTEMPTS,
                        final_url,
                        status,
                        response_bytes,
                        "抖音页面重试成功"
                    );
                }
                return Ok(item);
            }
            Err(error) => {
                tracing::warn!(
                    attempt,
                    max_attempts = PAGE_MAX_ATTEMPTS,
                    error = %format!("{error:#}"),
                    "抖音页面抓取或解析尝试失败"
                );
                last_error = Some(error);
            }
        }

        if attempt < PAGE_MAX_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(PAGE_RETRY_BASE_MS * attempt as u64)).await;
        }
    }

    Err(last_error
        .expect("PAGE_MAX_ATTEMPTS 必须大于 0")
        .context(format!("抖音页面在 {PAGE_MAX_ATTEMPTS} 次尝试后仍无法解析")))
}

/// 下载图文每张图到 dir,webp 转 jpg(Telegram sendPhoto 对 webp 不友好)。
/// 返回(落地文件, 失败张数):部分失败时频道帖会缺图,调用方须提示用户。
pub async fn download_images(
    client: &reqwest::Client,
    item: &MediaItem,
    dir: &Path,
) -> (Vec<PathBuf>, usize) {
    let _ = std::fs::create_dir_all(dir);
    crate::util::restrict_dir(dir);
    let mut out = Vec::new();
    let mut failed = 0usize;
    for (i, img) in item.images.iter().enumerate() {
        match download_one(client, img, dir, i).await {
            Ok(p) => out.push(p),
            Err(e) => {
                failed += 1;
                tracing::warn!(idx = i, error = %format!("{e:#}"), "抖音图片下载失败");
            }
        }
    }
    (out, failed)
}

async fn download_one(
    client: &reqwest::Client,
    image_ref: &ImageRef,
    dir: &Path,
    idx: usize,
) -> Result<PathBuf> {
    let mut seen = HashSet::new();
    let urls: Vec<&str> = std::iter::once(image_ref.url.as_str())
        .chain(image_ref.fallback_urls.iter().map(String::as_str))
        .filter(|url| !url.trim().is_empty() && seen.insert(*url))
        .collect();
    if urls.is_empty() {
        anyhow::bail!("图片没有可用 URL");
    }

    let mut errors = Vec::with_capacity(IMAGE_MAX_ATTEMPTS);
    for attempt in 0..IMAGE_MAX_ATTEMPTS {
        let url = urls[attempt % urls.len()];
        let host = reqwest::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| "<invalid-host>".to_string());
        let result: Result<PathBuf> = async {
            let response = client
                .get(url)
                .header(reqwest::header::REFERER, "https://www.douyin.com/")
                .timeout(IMAGE_REQUEST_TIMEOUT)
                .send()
                .await
                .context("发送图片请求失败")?
                .error_for_status()
                .context("图片响应状态异常")?;
            let bytes = response.bytes().await.context("读取图片响应失败")?.to_vec();
            transcode_to_jpeg(bytes, dir, idx).await
        }
        .await;

        match result {
            Ok(path) => return Ok(path),
            Err(error) => {
                let detail = format!(
                    "attempt {}/{} host={host}: {error:#}",
                    attempt + 1,
                    IMAGE_MAX_ATTEMPTS
                );
                tracing::warn!(
                    idx,
                    attempt = attempt + 1,
                    max_attempts = IMAGE_MAX_ATTEMPTS,
                    host,
                    error = %format!("{error:#}"),
                    "抖音图片下载尝试失败"
                );
                errors.push(detail);
            }
        }

        if attempt + 1 < IMAGE_MAX_ATTEMPTS {
            let delay_ms = IMAGE_RETRY_BASE_MS * (1_u64 << attempt);
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }

    anyhow::bail!(
        "图片经过 {IMAGE_MAX_ATTEMPTS} 次尝试仍失败: {}",
        errors.join(" | ")
    )
}

async fn transcode_to_jpeg(bytes: Vec<u8>, dir: &Path, idx: usize) -> Result<PathBuf> {
    // 抖音图通常为 webp,转 jpg(q92)发 Telegram;转码放阻塞线程。
    // 字节缓冲 Send + 'static,直接 move 到 blocking 任务。
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<PathBuf> {
        let img = image::load_from_memory(&bytes).context("解码图片失败")?;
        let out = dir.join(format!("{idx:03}.jpg"));
        let mut f = std::io::BufWriter::new(std::fs::File::create(&out)?);
        img.to_rgb8()
            .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
                &mut f, 92,
            ))
            .context("编码 jpg 失败")?;
        Ok(out)
    })
    .await?
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;

    #[test]
    fn detects_douyin_urls() {
        assert!(is_douyin_url("https://v.douyin.com/QSTFuN4OPw8/"));
        assert!(is_douyin_url(
            "https://www.douyin.com/note/7655599676083931850"
        ));
        assert!(is_douyin_url("https://www.iesdouyin.com/share/note/123/"));
        assert!(!is_douyin_url("https://www.pixiv.net/artworks/1"));
        assert!(!is_douyin_url("https://x.com/u/status/9"));
    }

    #[test]
    fn splits_desc_into_title_and_tags() {
        let (title, tags) = split_desc_tags("天使だ。🪽#若葉睦#睦子米 #wakaba 末尾");
        assert_eq!(title, "天使だ。🪽 末尾");
        assert_eq!(tags, vec!["若葉睦", "睦子米", "wakaba"]);
        let (t2, tags2) = split_desc_tags("没有标签");
        assert_eq!(t2, "没有标签");
        assert!(tags2.is_empty());
    }

    #[test]
    fn parse_note_extracts_images_author_tags() {
        // 仿真 note 页:_ROUTER_DATA 含图文作品(2 图 + 作者 + desc 标签)。
        let html = r#"<html><script>window._ROUTER_DATA = {
            "loaderData": { "note_(id)/page": { "videoInfoRes": { "item_list": [ {
                "aweme_id": 7655599676083931850,
                "desc": "天使だ#若葉睦#wakaba",
                "images": [
                    { "url_list": ["https://p3.douyinpic.com/a~tplv-dy-aweme-images:q75.webp?s=1", "https://p11/a"] },
                    { "url_list": ["https://p3.douyinpic.com/b~tplv-dy-aweme-images:q75.webp?s=2"] }
                ],
                "author": { "nickname": "xEe", "sec_uid": "MS4wABC" }
            } ] } } }
        };</script></html>"#;
        let it = parse_note(html, "manual").expect("应解析出图文");
        assert_eq!(it.source, SourceKind::Douyin);
        assert_eq!(it.source_id, "7655599676083931850");
        assert_eq!(it.url, "https://www.douyin.com/note/7655599676083931850");
        assert_eq!(it.author.name, "xEe");
        assert_eq!(it.author.url, "https://www.douyin.com/user/MS4wABC");
        assert_eq!(it.title.as_deref(), Some("天使だ"));
        assert_eq!(it.tags, vec!["若葉睦", "wakaba"]);
        assert_eq!(it.images.len(), 2);
        assert_eq!(it.page_count, 2);
        // douyin-downloader 排序规则同档优先非 webp；另一镜像仍保留为 fallback。
        assert_eq!(it.images[0].url, "https://p11/a");
        assert!(it.images[0].fallback_urls[0].contains("aweme-images"));
        assert!(it.images[1].fallback_urls.is_empty());
        assert!(!it.is_r18);
    }

    #[test]
    fn parse_user_aweme_supports_image_post_info_and_prefers_original() {
        let raw: Value = serde_json::from_str(
            r#"{
                "aweme_id": "7600000000000000001",
                "desc": "作者主页图集#壁纸",
                "author": {"nickname": "Artist", "sec_uid": "MS4wUSER"},
                "image_post_info": {"images": [{
                    "origin_image": {
                        "width": 3000, "height": 4000,
                        "url_list": ["https://p3.example/original.jpeg", "https://p9.example/original.jpeg"]
                    },
                    "display_image": {
                        "width": 6000, "height": 8000,
                        "url_list": ["https://p3.example/display.webp"]
                    },
                    "owner_watermark_image": {
                        "url_list": ["https://p3.example/tplv-dy-water.webp"]
                    }
                }]}
            }"#,
        )
        .unwrap();

        let item = parse_user_aweme(&raw, "artist_feed").unwrap();
        assert_eq!(item.source, SourceKind::Douyin);
        assert_eq!(item.source_id, "7600000000000000001");
        assert_eq!(item.author.name, "Artist");
        assert_eq!(item.origin, "artist_feed");
        assert_eq!(item.images[0].url, "https://p3.example/original.jpeg");
        assert_eq!(
            item.images[0].fallback_urls,
            vec![
                "https://p9.example/original.jpeg",
                "https://p3.example/display.webp",
                "https://p3.example/tplv-dy-water.webp"
            ]
        );
    }

    #[test]
    fn parse_user_aweme_skips_pure_video() {
        let raw: Value = serde_json::from_str(
            r#"{
                "aweme_id": "7600000000000000002",
                "desc": "video",
                "author": {"nickname": "Artist", "sec_uid": "MS4wUSER"},
                "video": {"play_addr": {"url_list": ["https://v.example/video.mp4"]}}
            }"#,
        )
        .unwrap();
        assert!(parse_user_aweme(&raw, "artist_feed").is_none());
    }

    #[test]
    fn parse_slides_info_keeps_only_static_images() {
        let body = r#"{
            "status_code": 0,
            "aweme_details": [ {
                "aweme_id": "7668950916557299363",
                "desc": "午后 #桑多涅 #原神 #画画",
                "author": { "nickname": "Hanna", "sec_uid": "MS4wSLIDES" },
                "images": [
                    {
                        "clip_type": 2,
                        "url_list": ["https://p3.douyinpic.com/first.webp"]
                    },
                    {
                        "clip_type": 2,
                        "url_list": ["https://p3.douyinpic.com/second.webp"]
                    },
                    {
                        "clip_type": 4,
                        "url_list": ["https://p3.douyinpic.com/video-cover.webp"],
                        "video": { "duration": 20034, "play_addr": { "url_list": ["https://v.example/video"] } }
                    }
                ]
            } ]
        }"#;

        let item = parse_slides_info(body, "manual").expect("应解析出静态图片");
        assert_eq!(item.source_id, "7668950916557299363");
        assert_eq!(item.images.len(), 2);
        assert_eq!(item.page_count, 2);
        assert!(item.images[0].url.ends_with("first.webp"));
        assert!(item.images[1].url.ends_with("second.webp"));
        assert!(item
            .images
            .iter()
            .all(|image| !image.url.contains("video-cover")));
    }

    #[test]
    fn detects_numeric_slides_id_only() {
        assert_eq!(
            slides_id(
                &reqwest::Url::parse("https://www.iesdouyin.com/share/slides/7668950916557299363/")
                    .unwrap()
            ),
            Some("7668950916557299363")
        );
        assert_eq!(
            slides_id(&reqwest::Url::parse("https://www.iesdouyin.com/share/note/123/").unwrap()),
            None
        );
        assert_eq!(
            slides_id(
                &reqwest::Url::parse("https://www.iesdouyin.com/share/slides/../video/").unwrap()
            ),
            None
        );
    }

    #[tokio::test]
    async fn fetch_note_retries_transient_page_without_router_data() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let valid_html = r#"<html><script>window._ROUTER_DATA = {
            "loaderData": { "note_(id)/page": { "videoInfoRes": { "item_list": [ {
                "aweme_id": "7668600536471822181",
                "desc": "一些蕾米壁纸#绝区零",
                "images": [ { "url_list": ["https://p3.douyinpic.com/image.webp"] } ],
                "author": { "nickname": "tester", "sec_uid": "MS4wTEST" }
            } ] } } }
        };</script></html>"#
            .to_string();

        let server = std::thread::spawn(move || {
            let responses = [
                "<html><title>抖音</title><body>transient page</body></html>".to_string(),
                valid_html,
            ];
            let mut paths = Vec::new();
            for body in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4096];
                let n = stream.read(&mut request).unwrap();
                let first_line = String::from_utf8_lossy(&request[..n])
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string();
                paths.push(
                    first_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_string(),
                );
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=UTF-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
            paths
        });

        let item = fetch_note_with_validator(
            &build_client().unwrap(),
            &format!("http://{addr}/share/note/7668600536471822181"),
            "manual",
            |_| true,
        )
        .await
        .unwrap();
        assert_eq!(item.source_id, "7668600536471822181");
        assert_eq!(item.images.len(), 1);
        assert_eq!(
            server.join().unwrap(),
            vec![
                "/share/note/7668600536471822181".to_string(),
                "/share/note/7668600536471822181".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn fetch_note_uses_slidesinfo_and_ignores_video_clips() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let slides_info = r#"{
            "status_code": 0,
            "aweme_details": [ {
                "aweme_id": "7668950916557299363",
                "desc": "午后 #桑多涅",
                "images": [
                    { "clip_type": 2, "url_list": ["https://p3/first.webp"] },
                    { "clip_type": 2, "url_list": ["https://p3/second.webp"] },
                    {
                        "clip_type": 4,
                        "url_list": ["https://p3/video-cover.webp"],
                        "video": { "play_addr": { "url_list": ["https://v/video.mp4"] } }
                    }
                ]
            } ]
        }"#
        .to_string();

        let server = std::thread::spawn(move || {
            let responses = [
                "<html><body>slides client shell without router data</body></html>".to_string(),
                slides_info,
            ];
            let mut paths = Vec::new();
            for body in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4096];
                let n = stream.read(&mut request).unwrap();
                paths.push(
                    String::from_utf8_lossy(&request[..n])
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_string(),
                );
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
            paths
        });

        let item = fetch_note_with_validator(
            &build_client().unwrap(),
            &format!("http://{addr}/share/slides/7668950916557299363/"),
            "manual",
            |_| true,
        )
        .await
        .unwrap();
        assert_eq!(item.source_id, "7668950916557299363");
        assert_eq!(item.images.len(), 2);
        assert_eq!(item.page_count, 2);

        let paths = server.join().unwrap();
        assert_eq!(paths[0], "/share/slides/7668950916557299363/");
        assert!(paths[1].starts_with("/web/api/v2/aweme/slidesinfo/?"));
        assert!(paths[1].contains("aweme_ids=%5B7668950916557299363%5D"));
        assert!(paths[1].contains("request_source=200"));
    }

    #[tokio::test]
    async fn download_one_rotates_to_fallback_url_after_http_failure() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let mut cursor = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(&mut cursor, image::ImageFormat::Png)
            .unwrap();
        let png = cursor.into_inner();

        let server = std::thread::spawn(move || {
            let mut paths = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4096];
                let n = stream.read(&mut request).unwrap();
                let first_line = String::from_utf8_lossy(&request[..n])
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string();
                let path = first_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                paths.push(path.clone());

                if path == "/primary" {
                    stream
                        .write_all(
                            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                } else {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        png.len()
                    )
                    .unwrap();
                    stream.write_all(&png).unwrap();
                }
            }
            paths
        });

        let image_ref = ImageRef {
            url: format!("http://{addr}/primary"),
            referer: None,
            fallback_urls: vec![format!("http://{addr}/fallback")],
        };
        let dir = tempfile::tempdir().unwrap();
        let path = download_one(&build_client().unwrap(), &image_ref, dir.path(), 0)
            .await
            .unwrap();
        assert_eq!(path.file_name().unwrap(), "000.jpg");
        assert!(image::open(path).is_ok());
        assert_eq!(
            server.join().unwrap(),
            vec!["/primary".to_string(), "/fallback".to_string()]
        );
    }

    #[test]
    fn parse_note_none_on_garbage() {
        assert!(parse_note("<html>no router data</html>", "manual").is_none());
    }
}
