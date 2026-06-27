//! 抖音图文(note)解析:gallery-dl 不支持抖音,这里用 reqwest 直接抓分享页。
//! 路线(免签名,对标 versenilvis/douyin-downloader):移动端 UA 跟随短链 → note 页 HTML
//! → 抠 `window._ROUTER_DATA` JSON → 取 images[].url_list[0](无水印全分辨率)、作者、desc 标签。
//! 比 pixiv/x 脆:抖音改 `_ROUTER_DATA` 结构或加验证墙时会失效,失败优雅提示即可。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::model::{Author, ImageRef, MediaItem, SourceKind};

/// 抖音 CDN 对桌面 UA 返回拦截页,必须移动端 UA(对标现有解析项目)。
const MOBILE_UA: &str = "Mozilla/5.0 (Linux; Android 11; SAMSUNG SM-G973U) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Mobile Safari/537.36";

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

    // 每张图取 url_list[0](tplv-dy-aweme-images,无水印全分辨率)。
    let images: Vec<ImageRef> = item
        .get("images")?
        .as_array()?
        .iter()
        .filter_map(|img| {
            img.get("url_list")
                .and_then(|u| u.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(|url| ImageRef {
                    url: url.to_string(),
                    referer: None,
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
    let html = client
        .get(url)
        .send()
        .await
        .context("抖音页面请求失败")?
        .text()
        .await
        .context("抖音页面读取失败")?;
    parse_note(&html, origin).context("抖音页面解析失败(无 _ROUTER_DATA / 结构变更 / 验证墙)")
}

/// 下载图文每张图到 dir,webp 转 jpg(Telegram sendPhoto 对 webp 不友好)。返回落地文件。
pub async fn download_images(
    client: &reqwest::Client,
    item: &MediaItem,
    dir: &Path,
) -> Vec<PathBuf> {
    let _ = std::fs::create_dir_all(dir);
    let mut out = Vec::new();
    for (i, img) in item.images.iter().enumerate() {
        match download_one(client, &img.url, dir, i).await {
            Ok(p) => out.push(p),
            Err(e) => tracing::warn!(idx = i, error = %e, "抖音图片下载失败"),
        }
    }
    out
}

async fn download_one(
    client: &reqwest::Client,
    url: &str,
    dir: &Path,
    idx: usize,
) -> Result<PathBuf> {
    let bytes = client
        .get(url)
        .send()
        .await
        .context("图片请求失败")?
        .bytes()
        .await
        .context("图片读取失败")?;
    // 抖音图为 webp,转 jpg(q92)发 Telegram;转码放阻塞线程。
    let dir = dir.to_path_buf();
    let bytes = bytes.to_vec();
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
        assert!(!it.is_r18);
    }

    #[test]
    fn parse_note_none_on_garbage() {
        assert!(parse_note("<html>no router data</html>", "manual").is_none());
    }
}
