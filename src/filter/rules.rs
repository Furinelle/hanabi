use crate::config::SourceFilterCfg;
use crate::filter::Filter;
use crate::model::{MediaItem, PixivType, SourceKind};

/// r18=false → 只留全年龄;r18=true → 放行 R18。
pub struct R18Filter;
impl Filter for R18Filter {
    fn keep(&self, item: &MediaItem, cfg: &SourceFilterCfg) -> bool {
        cfg.r18 || !item.is_r18
    }
}

/// illust_only 仅对 Pixiv 生效;X(pixiv_type=None)不被拦。
pub struct PixivTypeFilter;
impl Filter for PixivTypeFilter {
    fn keep(&self, item: &MediaItem, cfg: &SourceFilterCfg) -> bool {
        if !cfg.illust_only {
            return true;
        }
        matches!(item.pixiv_type, Some(PixivType::Illust) | None)
    }
}

/// 保留 page_count < max_pages。
pub struct PageCountFilter;
impl Filter for PageCountFilter {
    fn keep(&self, item: &MediaItem, cfg: &SourceFilterCfg) -> bool {
        match cfg.max_pages {
            Some(max) => item.page_count < max,
            None => true,
        }
    }
}

/// bookmark_count 存 Pixiv 收藏数 / X 点赞数。按来源选阈值:Pixiv 用 min_bookmarks,
/// X 用 min_likes(两者语义不同,不可交叉套用)。阈值已设但分数缺失 → 拦(保守)。
pub struct ScoreThreshold;
impl Filter for ScoreThreshold {
    fn keep(&self, item: &MediaItem, cfg: &SourceFilterCfg) -> bool {
        let min = match item.source {
            SourceKind::Pixiv => cfg.min_bookmarks,
            SourceKind::X => cfg.min_likes,
            SourceKind::Douyin => None, // 抖音仅手动链接(绕过过滤),无分数阈值
        };
        match min {
            Some(m) => item.bookmark_count.is_some_and(|s| s >= m),
            None => true,
        }
    }
}

/// require_media → images 必须非空。
pub struct RequireMedia;
impl Filter for RequireMedia {
    fn keep(&self, item: &MediaItem, cfg: &SourceFilterCfg) -> bool {
        !cfg.require_media || !item.images.is_empty()
    }
}

/// tag 白名单:作品 tag 与白名单有交集才留(精确匹配;ASCII 额外大小写不敏感,
/// 中日文标签按精确匹配)。
pub struct TagWhitelist;
impl Filter for TagWhitelist {
    fn keep(&self, item: &MediaItem, cfg: &SourceFilterCfg) -> bool {
        match &cfg.tags {
            Some(white) if !white.is_empty() => item
                .tags
                .iter()
                .any(|t| white.iter().any(|w| t == w || t.eq_ignore_ascii_case(w))),
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SourceFilterCfg;
    use crate::model::{Author, ImageRef, MediaItem, PixivType, SourceKind};

    fn base() -> MediaItem {
        MediaItem {
            source: SourceKind::Pixiv,
            source_id: "1".into(),
            author: Author {
                name: "a".into(),
                url: "u".into(),
            },
            title: None,
            url: "w".into(),
            tags: vec!["原神".into(), "風景".into()],
            bookmark_count: Some(800),
            is_r18: false,
            pixiv_type: Some(PixivType::Illust),
            page_count: 2,
            images: vec![ImageRef {
                url: "i".into(),
                referer: None,
            }],
            origin: "s".into(),
        }
    }

    #[test]
    fn r18_filtered_when_cfg_false() {
        let mut it = base();
        it.is_r18 = true;
        let cfg = SourceFilterCfg {
            r18: false,
            ..Default::default()
        };
        assert!(!R18Filter.keep(&it, &cfg));
        let cfg_allow = SourceFilterCfg {
            r18: true,
            ..Default::default()
        };
        assert!(R18Filter.keep(&it, &cfg_allow));
    }

    #[test]
    fn illust_only_rejects_manga_keeps_x() {
        let cfg = SourceFilterCfg {
            illust_only: true,
            ..Default::default()
        };
        let mut manga = base();
        manga.pixiv_type = Some(PixivType::Manga);
        assert!(!PixivTypeFilter.keep(&manga, &cfg));
        let mut x = base();
        x.source = SourceKind::X;
        x.pixiv_type = None;
        assert!(PixivTypeFilter.keep(&x, &cfg));
    }

    #[test]
    fn page_count_below_max_kept() {
        let cfg = SourceFilterCfg {
            max_pages: Some(5),
            ..Default::default()
        };
        let mut ok = base();
        ok.page_count = 4;
        assert!(PageCountFilter.keep(&ok, &cfg));
        let mut too_many = base();
        too_many.page_count = 5;
        assert!(!PageCountFilter.keep(&too_many, &cfg));
    }

    #[test]
    fn score_threshold_bookmarks_and_likes() {
        let cfg = SourceFilterCfg {
            min_bookmarks: Some(1000),
            ..Default::default()
        };
        assert!(!ScoreThreshold.keep(&base(), &cfg));
        let cfg_likes = SourceFilterCfg {
            min_likes: Some(500),
            ..Default::default()
        };
        assert!(ScoreThreshold.keep(&base(), &cfg_likes));
    }

    #[test]
    fn tag_whitelist_intersection() {
        let cfg = SourceFilterCfg {
            tags: Some(vec!["原神".into()]),
            ..Default::default()
        };
        assert!(TagWhitelist.keep(&base(), &cfg));
        let cfg_miss = SourceFilterCfg {
            tags: Some(vec!["東方".into()]),
            ..Default::default()
        };
        assert!(!TagWhitelist.keep(&base(), &cfg_miss));
    }

    #[test]
    fn require_media_rejects_empty() {
        let cfg = SourceFilterCfg {
            require_media: true,
            ..Default::default()
        };
        let mut empty = base();
        empty.images.clear();
        assert!(!RequireMedia.keep(&empty, &cfg));
        assert!(RequireMedia.keep(&base(), &cfg));
    }
}
