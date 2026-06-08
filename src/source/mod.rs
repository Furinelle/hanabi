pub mod pixiv;
pub mod x;

use anyhow::Result;
use async_trait::async_trait;

use crate::config::SourceFilterCfg;
use crate::model::MediaItem;
use crate::store::Store;

#[async_trait]
pub trait Source: Send + Sync {
    fn name(&self) -> &str;
    fn filter_cfg(&self) -> &SourceFilterCfg;
    async fn fetch(&self, store: &Store) -> Result<Vec<MediaItem>>;
}

#[cfg(test)]
mod tests {
    #[test]
    fn pixiv_user_target_is_passthrough() {
        let urls = crate::source::pixiv::targets_for(
            "pixiv_user",
            &["https://www.pixiv.net/users/123".into()],
        );
        assert_eq!(urls, vec!["https://www.pixiv.net/users/123".to_string()]);
    }

    #[test]
    fn x_download_extra_sets_size_orig() {
        let extra = crate::source::x::download_extra(Some("orig"));
        assert!(extra.iter().any(|s| s.contains("orig")));
    }

    #[test]
    fn parse_twitter_maps_likes_and_hashtags() {
        let raw = include_str!("../../tests/fixtures/twitter_dump.json");
        let val: serde_json::Value = serde_json::from_str(raw).unwrap();
        let items = crate::source::x::parse_twitter(&val, "x_foryou");
        assert_eq!(items.len(), 1);
        let t = &items[0];
        assert_eq!(t.source, crate::model::SourceKind::X);
        assert_eq!(t.source_id, "1700000000000000001");
        assert_eq!(t.bookmark_count, Some(3000));
        assert_eq!(t.tags, vec!["イラスト".to_string(), "原神".to_string()]);
        assert_eq!(t.pixiv_type, None);
        assert_eq!(t.images.len(), 1);
        assert_eq!(t.author.name, "Artist B");
    }
}
