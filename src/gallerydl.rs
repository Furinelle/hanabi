use std::collections::BTreeMap;

use serde_json::Value;

use crate::model::{Author, ImageRef, MediaItem, PixivType, SourceKind};

const PIXIV_REFERER: &str = "https://www.pixiv.net/";

fn pixiv_type(s: &str) -> Option<PixivType> {
    match s {
        "illust" => Some(PixivType::Illust),
        "manga" => Some(PixivType::Manga),
        "ugoira" => Some(PixivType::Ugoira),
        _ => None,
    }
}

fn str_field(meta: &Value, key: &str) -> Option<String> {
    meta.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn u32_field(meta: &Value, key: &str) -> Option<u32> {
    meta.get(key).and_then(|v| v.as_u64()).map(|n| n as u32)
}

fn tags_field(meta: &Value) -> Vec<String> {
    meta.get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// 解析 gallery-dl `-j` 的 pixiv 输出，按作品 id 分组成 MediaItem。
pub fn parse_pixiv(root: &Value, origin: &str) -> Vec<MediaItem> {
    let mut order: Vec<String> = Vec::new();
    let mut by_id: BTreeMap<String, MediaItem> = BTreeMap::new();

    let arr = match root.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };

    for elem in arr {
        let tuple = match elem.as_array() {
            Some(t) if t.len() >= 3 => t,
            _ => continue,
        };
        if tuple[0].as_u64() != Some(3) {
            continue;
        }
        let url = match tuple[1].as_str() {
            Some(u) => u.to_string(),
            None => continue,
        };
        let meta = &tuple[2];
        let id = match meta.get("id").and_then(|v| v.as_u64()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        let image = ImageRef {
            url,
            referer: Some(PIXIV_REFERER.to_string()),
        };

        if let Some(existing) = by_id.get_mut(&id) {
            existing.images.push(image);
            continue;
        }

        let user = meta.get("user");
        let author_name = user
            .and_then(|u| u.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let author_id = user.and_then(|u| u.get("id")).and_then(|v| v.as_u64());
        let author_url = match author_id {
            Some(uid) => format!("https://www.pixiv.net/users/{uid}"),
            None => String::new(),
        };

        let item = MediaItem {
            source: SourceKind::Pixiv,
            source_id: id.clone(),
            author: Author { name: author_name, url: author_url },
            title: str_field(meta, "title"),
            url: format!("https://www.pixiv.net/artworks/{id}"),
            tags: tags_field(meta),
            bookmark_count: u32_field(meta, "total_bookmarks"),
            is_r18: u32_field(meta, "x_restrict").map_or(false, |x| x > 0),
            pixiv_type: str_field(meta, "type").and_then(|s| pixiv_type(&s)),
            page_count: u32_field(meta, "page_count").unwrap_or(1),
            images: vec![image],
            origin: origin.to_string(),
        };
        order.push(id.clone());
        by_id.insert(id, item);
    }

    order.into_iter().filter_map(|id| by_id.remove(&id)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PixivType, SourceKind};

    #[test]
    fn parse_pixiv_groups_by_id() {
        let raw = include_str!("../tests/fixtures/pixiv_dump.json");
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let items = parse_pixiv(&val, "fav_artists");

        assert_eq!(items.len(), 2);

        let a = items.iter().find(|i| i.source_id == "123").unwrap();
        assert_eq!(a.source, SourceKind::Pixiv);
        assert_eq!(a.title.as_deref(), Some("湖と少女"));
        assert_eq!(a.pixiv_type, Some(PixivType::Illust));
        assert_eq!(a.page_count, 2);
        assert_eq!(a.bookmark_count, Some(800));
        assert!(!a.is_r18);
        assert_eq!(a.tags, vec!["原神".to_string(), "風景".to_string()]);
        assert_eq!(a.images.len(), 2);
        assert_eq!(a.author.name, "画师A");
        assert_eq!(a.author.url, "https://www.pixiv.net/users/555");
        assert_eq!(a.url, "https://www.pixiv.net/artworks/123");
        assert_eq!(a.images[0].referer.as_deref(), Some("https://www.pixiv.net/"));

        let b = items.iter().find(|i| i.source_id == "124").unwrap();
        assert_eq!(b.pixiv_type, Some(PixivType::Manga));
        assert!(b.is_r18);
    }
}
