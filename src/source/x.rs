use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::config::{SourceCfg, SourceFilterCfg};
use crate::gallerydl::GalleryDl;
use crate::model::{Author, ImageRef, MediaItem, SourceKind};
use crate::source::Source;
use crate::store::Store;

/// 下载时把图片尺寸设为最高画质(size=orig)。
pub fn download_extra(size: Option<&str>) -> Vec<String> {
    match size {
        Some(s) => vec!["-o".into(), format!("extractor.twitter.size={s}")],
        None => vec![],
    }
}

pub fn parse_twitter(root: &Value, origin: &str) -> Vec<MediaItem> {
    let arr = match root.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut by_id: std::collections::BTreeMap<String, MediaItem> =
        std::collections::BTreeMap::new();
    let mut order: Vec<String> = Vec::new();

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
        let id = match meta.get("tweet_id").and_then(|v| v.as_u64()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let image = ImageRef { url, referer: None };
        if let Some(existing) = by_id.get_mut(&id) {
            existing.images.push(image);
            continue;
        }
        let author = meta.get("author");
        let author_name = author
            .and_then(|a| a.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let handle = author
            .and_then(|a| a.get("nick"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let tags = meta
            .get("hashtags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let item = MediaItem {
            source: SourceKind::X,
            source_id: id.clone(),
            author: Author {
                name: author_name,
                url: format!("https://x.com/{handle}"),
            },
            title: meta
                .get("content")
                .and_then(|v| v.as_str())
                .map(String::from),
            url: format!("https://x.com/i/status/{id}"),
            tags,
            bookmark_count: meta
                .get("favorite_count")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32),
            is_r18: false,
            pixiv_type: None,
            page_count: 1,
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
    for it in &mut items {
        it.page_count = it.images.len() as u32;
    }
    items
}

pub struct XSource {
    cfg: SourceCfg,
    gdl: Arc<GalleryDl>,
}

impl XSource {
    pub fn new(cfg: SourceCfg, gdl: Arc<GalleryDl>) -> Self {
        Self { cfg, gdl }
    }
}

#[async_trait]
impl Source for XSource {
    fn name(&self) -> &str {
        &self.cfg.name
    }
    fn filter_cfg(&self) -> &SourceFilterCfg {
        &self.cfg.filters
    }
    async fn fetch(&self, _store: &Store) -> Result<Vec<MediaItem>> {
        let mut out = Vec::new();
        for target in self.cfg.targets.clone() {
            let gdl = self.gdl.clone();
            let origin = self.cfg.name.clone();
            let val = tokio::task::spawn_blocking(move || gdl.probe(&target)).await??;
            out.extend(parse_twitter(&val, &origin));
        }
        Ok(out)
    }
}
