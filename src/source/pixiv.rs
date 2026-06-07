use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::config::{SourceCfg, SourceFilterCfg};
use crate::gallerydl::{parse_pixiv, GalleryDl};
use crate::model::MediaItem;
use crate::source::Source;
use crate::store::Store;

/// Pixiv 三类源 target 直接透传配置 URL(user/bookmarks/ranking 均为完整 URL)。
pub fn targets_for(_kind: &str, targets: &[String]) -> Vec<String> {
    targets.to_vec()
}

pub struct PixivSource {
    cfg: SourceCfg,
    gdl: Arc<GalleryDl>,
}

impl PixivSource {
    pub fn new(cfg: SourceCfg, gdl: Arc<GalleryDl>) -> Self {
        Self { cfg, gdl }
    }
}

#[async_trait]
impl Source for PixivSource {
    fn name(&self) -> &str {
        &self.cfg.name
    }
    fn filter_cfg(&self) -> &SourceFilterCfg {
        &self.cfg.filters
    }
    async fn fetch(&self, _store: &Store) -> Result<Vec<MediaItem>> {
        let mut out = Vec::new();
        for target in targets_for(&self.cfg.kind, &self.cfg.targets) {
            let gdl = self.gdl.clone();
            let origin = self.cfg.name.clone();
            let val = tokio::task::spawn_blocking(move || gdl.probe(&target)).await??;
            out.extend(parse_pixiv(&val, &origin));
        }
        Ok(out)
    }
}
