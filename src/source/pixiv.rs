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

/// 裸 pixiv 画师主页(`pixiv.net/users/<id>`)→ `/artworks` 子页(直接出作品)。
/// gallery-dl 对裸主页 `-j` 只回一个 type-6 Queue(指向 /artworks)、不递归 → 解析出 0 张;
/// `/artworks` 子页直接给 type-3 文件项。其它形态(/artworks/<id>、/bookmarks 等)与非 pixiv 链接原样返回。
pub fn normalize_profile_url(url: &str) -> String {
    let rest = match url.split_once("://") {
        Some((_, r)) => r,
        None => return url.to_string(),
    };
    let path_part = rest.split(['?', '#']).next().unwrap_or(rest);
    let mut segs = path_part.split('/');
    let host = segs.next().unwrap_or("").to_lowercase();
    if !(host == "pixiv.net" || host.ends_with(".pixiv.net")) {
        return url.to_string();
    }
    let path: Vec<&str> = segs.filter(|s| !s.is_empty()).collect();
    // 裸主页 users/<数字 id> → 加 /artworks。
    if path.len() == 2 && path[0] == "users" && path[1].chars().all(|c| c.is_ascii_digit()) {
        return format!("https://www.pixiv.net/users/{}/artworks", path[1]);
    }
    url.to_string()
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

#[cfg(test)]
mod tests {
    use super::normalize_profile_url;

    #[test]
    fn bare_user_becomes_artworks() {
        assert_eq!(
            normalize_profile_url("https://www.pixiv.net/users/1499614"),
            "https://www.pixiv.net/users/1499614/artworks"
        );
        assert_eq!(
            normalize_profile_url("https://www.pixiv.net/users/1499614?lang=zh"),
            "https://www.pixiv.net/users/1499614/artworks"
        );
    }

    #[test]
    fn non_bare_and_non_pixiv_unchanged() {
        for u in [
            "https://www.pixiv.net/users/1499614/artworks",
            "https://www.pixiv.net/users/123/bookmarks/artworks",
            "https://www.pixiv.net/artworks/141191404",
            "https://x.com/someone",
        ] {
            assert_eq!(normalize_profile_url(u), u);
        }
    }
}
