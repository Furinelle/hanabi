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

/// HTML 属性值转义:在文本转义之外还须转 `"`。href="…" 由双引号包裹,外部可控的
/// URL(X handle、抖音 sec_uid 拼出的作者链接)含引号会跳出属性注入任意标签,
/// 或直接让 Telegram HTML 解析报 400 → 整条交付确定性失败无限重试。
fn attr_escape(s: &str) -> String {
    html_escape(s).replace('"', "&quot;")
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
        crate::model::SourceKind::Douyin => "抖音",
    };
    s.push_str(&format!(
        "From <a href=\"{}\">{}</a> By <a href=\"{}\">{}</a>",
        attr_escape(&item.url),
        src,
        attr_escape(&item.author.url),
        html_escape(&item.author.name)
    ));
    s
}

/// Telegram photo 上限:约 10MB,且宽+高 ≤ 10000。超限需缩放。
pub fn needs_downscale(bytes: u64, width: u32, height: u32) -> bool {
    bytes > 10_000_000 || (width + height) > 10_000
}

/// Telegram sendPhoto 硬上限(实测报错阈值:file too big for a photo, max 10485760)。
/// prepare() 按更保守的 needs_downscale 预判缩放,但高细节/透明 PNG 缩放后仍可能超此线;
/// 到达硬上限时 sendPhoto 必定被拒,调用方应退化为 sendDocument。
pub const PHOTO_HARD_LIMIT_BYTES: u64 = 10_485_760;

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
    fn caption_escapes_href_attribute() {
        let mut it = item();
        it.author.url = "https://x.com/a\"><b>x".into();
        let c = render_caption(&it);
        // 引号被转义,不会跳出 href 属性。
        assert!(!c.contains("a\"><b>"));
        assert!(c.contains("&quot;"));
    }

    #[test]
    fn downscale_only_when_over_limits() {
        assert!(!needs_downscale(2_000_000, 3000, 2000));
        assert!(needs_downscale(11_000_000, 3000, 2000));
        assert!(needs_downscale(2_000_000, 9000, 4000));
    }
}
