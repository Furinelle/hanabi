use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
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
    meta.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
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

        // AI 生成作品(pixiv illust_ai_type==2)打标:置于 tag 首位,渲染为 #AI生成
        // (放最前避免被 caption 的 take(6) 截掉)。
        let mut tags = tags_field(meta);
        if u32_field(meta, "illust_ai_type") == Some(2) {
            tags.insert(0, "AI生成".to_string());
        }
        let item = MediaItem {
            source: SourceKind::Pixiv,
            source_id: id.clone(),
            author: Author {
                name: author_name,
                url: author_url,
            },
            title: str_field(meta, "title"),
            url: format!("https://www.pixiv.net/artworks/{id}"),
            tags,
            bookmark_count: u32_field(meta, "total_bookmarks"),
            is_r18: u32_field(meta, "x_restrict").is_some_and(|x| x > 0),
            pixiv_type: str_field(meta, "type").and_then(|s| pixiv_type(&s)),
            page_count: u32_field(meta, "page_count").unwrap_or(1),
            images: vec![image],
            origin: origin.to_string(),
        };
        order.push(id.clone());
        by_id.insert(id, item);
    }

    let mut items: Vec<MediaItem> = order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect();
    // page_count 字段缺失或低于实际抓到的图片数时,以实际图片数为准,
    // 避免多图作品因字段不准绕过 PageCountFilter。
    for it in &mut items {
        let n = it.images.len() as u32;
        if it.page_count < n {
            it.page_count = n;
        }
    }
    items
}

pub struct GalleryDl {
    pub config_path: String,
    pub probe_range: String,
}

impl GalleryDl {
    /// `-j` 拉元数据(不下载),返回顶层 JSON。
    pub fn probe(&self, target: &str) -> Result<Value> {
        let out = Command::new("gallery-dl")
            .args([
                "--config",
                &self.config_path,
                "-j",
                "--range",
                &self.probe_range,
                target,
            ])
            .output()
            .context("启动 gallery-dl 失败,确认已安装并在 PATH")?;
        if !out.status.success() {
            anyhow::bail!(
                "gallery-dl probe 失败 ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let val: Value =
            serde_json::from_slice(&out.stdout).context("解析 gallery-dl JSON 失败")?;
        Ok(val)
    }

    /// 下载指定作品到 dir,返回该次落地的文件路径。
    pub fn download(
        &self,
        work_url: &str,
        dir: &std::path::Path,
        extra: &[String],
    ) -> Result<Vec<PathBuf>> {
        let before = list_files(dir);
        let mut cmd = Command::new("gallery-dl");
        cmd.args(["--config", &self.config_path, "-D"])
            .arg(dir)
            .args(extra)
            .arg(work_url);
        let out = cmd.output().context("启动 gallery-dl 下载失败")?;
        if !out.status.success() {
            anyhow::bail!(
                "gallery-dl download 失败 ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let after = list_files(dir);
        Ok(after.into_iter().filter(|p| !before.contains(p)).collect())
    }
}

fn list_files(dir: &std::path::Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default()
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
        assert_eq!(
            a.images[0].referer.as_deref(),
            Some("https://www.pixiv.net/")
        );

        let b = items.iter().find(|i| i.source_id == "124").unwrap();
        assert_eq!(b.pixiv_type, Some(PixivType::Manga));
        assert!(b.is_r18);
    }

    #[test]
    fn parse_pixiv_tags_ai_generated() {
        // illust_ai_type==2 → tag 首位插入 AI生成;==1 不插。
        let ai = serde_json::json!([[3, "https://i.pximg.net/a.png", {
            "id": 999, "title": "t", "type": "illust",
            "tags": ["原神"], "illust_ai_type": 2,
            "user": {"id": 1, "name": "a"}
        }]]);
        let items = parse_pixiv(&ai, "test");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].tags.first().map(String::as_str), Some("AI生成"));

        let not_ai = serde_json::json!([[3, "https://i.pximg.net/b.png", {
            "id": 1000, "title": "t", "type": "illust",
            "tags": ["原神"], "illust_ai_type": 1,
            "user": {"id": 1, "name": "a"}
        }]]);
        let items = parse_pixiv(&not_ai, "test");
        assert_eq!(items[0].tags, vec!["原神".to_string()]);
    }
}
