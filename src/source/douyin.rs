//! 抖音图文(note)解析:gallery-dl 不支持抖音,这里用 reqwest 直接抓分享页。
//! 路线(免签名,对标 versenilvis/douyin-downloader):移动端 UA 跟随短链 → note 页 HTML
//! → 抠 `window._ROUTER_DATA` JSON → 取 images[].url_list(无水印全分辨率 + 备用 CDN)、作者、desc 标签。
//! 比 pixiv/x 脆:抖音改 `_ROUTER_DATA` 结构或加验证墙时会失效,失败优雅提示即可。

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::model::{Author, ImageRef, MediaItem, SourceKind};

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

/// 解析 note 页 HTML → MediaItem。纯函数(便于测试),网络抓取见 `fetch_note`。
pub fn parse_note(html: &str, origin: &str) -> Option<MediaItem> {
    let root = extract_router_data(html)?;
    let item = find_note_item(&root)?;

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
    let images: Vec<ImageRef> = item
        .get("images")?
        .as_array()?
        .iter()
        .filter_map(|img| {
            let urls: Vec<String> = img
                .get("url_list")
                .and_then(|u| u.as_array())
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
                .filter(|url| !url.trim().is_empty())
                .map(str::to_string)
                .collect();
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
            let item = parse_note(&html, origin).with_context(|| {
                format!(
                    "抖音页面解析失败(无 _ROUTER_DATA / 结构变更 / 验证墙) \
                     status={status} final_url={safe_final_url} response_bytes={response_bytes}"
                )
            })?;
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
        assert!(it.images[0].url.contains("aweme-images"));
        assert_eq!(it.images[0].fallback_urls, vec!["https://p11/a"]);
        assert!(it.images[1].fallback_urls.is_empty());
        assert!(!it.is_r18);
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
