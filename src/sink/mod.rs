pub mod telegram;

use anyhow::Result;
use async_trait::async_trait;

use crate::model::MediaItem;

#[async_trait]
pub trait Sink: Send + Sync {
    async fn deliver(&self, item: &MediaItem, files: &[std::path::PathBuf]) -> Result<()>;
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// caption:标题 / 画师(超链)/ 原作品链接 / 前 5 个 tag。HTML 格式。
pub fn render_caption(item: &MediaItem) -> String {
    let mut s = String::new();
    if let Some(t) = &item.title {
        // 限制标题长度,叠加 ≤5 个 tag,保证整条 caption 远低于 Telegram 1024 字符上限。
        let t = if t.chars().count() > 200 {
            t.chars().take(200).collect::<String>() + "…"
        } else {
            t.clone()
        };
        s.push_str(&html_escape(&t));
        s.push('\n');
    }
    s.push_str(&format!(
        "<a href=\"{}\">{}</a>\n",
        item.author.url,
        html_escape(&item.author.name)
    ));
    s.push_str(&format!("<a href=\"{}\">原作品</a>", item.url));
    if !item.tags.is_empty() {
        let tags = item
            .tags
            .iter()
            .take(5)
            .map(|t| format!("#{}", t.replace(' ', "_")))
            .collect::<Vec<_>>()
            .join(" ");
        s.push_str(&format!("\n{tags}"));
    }
    s
}

/// Telegram photo 上限:约 10MB,且宽+高 ≤ 10000。超限需缩放。
pub fn needs_downscale(bytes: u64, width: u32, height: u32) -> bool {
    bytes > 10_000_000 || (width + height) > 10_000
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Author, ImageRef, MediaItem, PixivType, SourceKind};

    fn item() -> MediaItem {
        MediaItem {
            source: SourceKind::Pixiv,
            source_id: "123".into(),
            author: Author {
                name: "画师A".into(),
                url: "https://www.pixiv.net/users/555".into(),
            },
            title: Some("湖と少女".into()),
            url: "https://www.pixiv.net/artworks/123".into(),
            tags: vec!["原神".into(), "風景".into()],
            bookmark_count: Some(800),
            is_r18: false,
            pixiv_type: Some(PixivType::Illust),
            page_count: 2,
            images: vec![ImageRef {
                url: "i".into(),
                referer: None,
            }],
            origin: "fav_artists".into(),
        }
    }

    #[test]
    fn caption_has_title_author_link_tags() {
        let c = render_caption(&item());
        assert!(c.contains("湖と少女"));
        assert!(c.contains("https://www.pixiv.net/users/555"));
        assert!(c.contains("画师A"));
        assert!(c.contains("https://www.pixiv.net/artworks/123"));
        assert!(c.contains("#原神"));
    }

    #[test]
    fn caption_escapes_html() {
        let mut it = item();
        it.title = Some("a<b>&c".into());
        let c = render_caption(&it);
        assert!(c.contains("a&lt;b&gt;&amp;c"));
    }

    #[test]
    fn downscale_only_when_over_limits() {
        assert!(!needs_downscale(2_000_000, 3000, 2000));
        assert!(needs_downscale(11_000_000, 3000, 2000));
        assert!(needs_downscale(2_000_000, 9000, 4000));
    }
}
