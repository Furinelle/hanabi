pub mod rules;

use crate::config::SourceFilterCfg;
use crate::model::MediaItem;

pub trait Filter: Send + Sync {
    fn keep(&self, item: &MediaItem, cfg: &SourceFilterCfg) -> bool;
}

pub struct FilterChain {
    rules: Vec<Box<dyn Filter>>,
}

impl FilterChain {
    pub fn new(rules: Vec<Box<dyn Filter>>) -> Self {
        Self { rules }
    }

    /// 设计文档 §2.2 的标准规则顺序。
    pub fn standard() -> Self {
        use rules::*;
        Self::new(vec![
            Box::new(R18Filter),
            Box::new(PixivTypeFilter),
            Box::new(PageCountFilter),
            Box::new(ScoreThreshold),
            Box::new(RequireMedia),
            Box::new(TagWhitelist),
        ])
    }

    pub fn keep(&self, item: &MediaItem, cfg: &SourceFilterCfg) -> bool {
        self.rules.iter().all(|r| r.keep(item, cfg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SourceFilterCfg;
    use crate::model::{Author, ImageRef, MediaItem, PixivType, SourceKind};

    struct AlwaysNo;
    impl Filter for AlwaysNo {
        fn keep(&self, _: &MediaItem, _: &SourceFilterCfg) -> bool {
            false
        }
    }

    fn item() -> MediaItem {
        MediaItem {
            source: SourceKind::Pixiv,
            source_id: "1".into(),
            author: Author {
                name: "a".into(),
                url: "u".into(),
            },
            title: None,
            url: "w".into(),
            tags: vec![],
            bookmark_count: Some(1),
            is_r18: false,
            pixiv_type: Some(PixivType::Illust),
            page_count: 1,
            images: vec![ImageRef {
                url: "i".into(),
                referer: None,
            }],
            origin: "s".into(),
        }
    }

    #[test]
    fn chain_rejects_if_any_rule_rejects() {
        let chain = FilterChain::new(vec![Box::new(AlwaysNo)]);
        assert!(!chain.keep(&item(), &SourceFilterCfg::default()));
    }

    #[test]
    fn empty_chain_keeps_everything() {
        let chain = FilterChain::new(vec![]);
        assert!(chain.keep(&item(), &SourceFilterCfg::default()));
    }
}
