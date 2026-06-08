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

/// caption 格式(HTML):
/// ```text
/// 🔞 R18            (仅 is_r18 时)
/// Title: 标题
/// Tag: #标签 #标签
/// From <Pixiv|X>(作品链接) By 作者名(作者链接)
/// ```
pub fn render_caption(item: &MediaItem) -> String {
    let mut s = String::new();
    if item.is_r18 {
        s.push_str("🔞 R18\n");
    }
    // Title(截断防止整条 caption 超 Telegram 1024 上限)
    let title = item.title.as_deref().unwrap_or("(无标题)");
    let title = if title.chars().count() > 150 {
        title.chars().take(150).collect::<String>() + "…"
    } else {
        title.to_string()
    };
    s.push_str(&format!("Title: {}\n", html_escape(&title)));
    // Tag(取前 6 个)
    if item.tags.is_empty() {
        s.push_str("Tag: -\n");
    } else {
        let tags = item
            .tags
            .iter()
            .take(6)
            .map(|t| format!("#{}", t.replace(' ', "_")))
            .collect::<Vec<_>>()
            .join(" ");
        s.push_str(&format!("Tag: {}\n", html_escape(&tags)));
    }
    // From <来源>(作品链接) By 作者名(作者链接)
    let src = match item.source {
        crate::model::SourceKind::Pixiv => "Pixiv",
        crate::model::SourceKind::X => "X",
    };
    s.push_str(&format!(
        "From <a href=\"{}\">{}</a> By <a href=\"{}\">{}</a>",
        item.url,
        src,
        item.author.url,
        html_escape(&item.author.name)
    ));
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
