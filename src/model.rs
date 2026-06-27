#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Pixiv,
    X,
    Douyin,
}

impl SourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceKind::Pixiv => "pixiv",
            SourceKind::X => "x",
            SourceKind::Douyin => "douyin",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixivType {
    Illust,
    Manga,
    Ugoira,
}

#[derive(Debug, Clone)]
pub struct Author {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct ImageRef {
    pub url: String,
    pub referer: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MediaItem {
    pub source: SourceKind,
    pub source_id: String,
    pub author: Author,
    pub title: Option<String>,
    pub url: String,
    pub tags: Vec<String>,
    pub bookmark_count: Option<u32>,
    pub is_r18: bool,
    pub pixiv_type: Option<PixivType>,
    pub page_count: u32,
    pub images: Vec<ImageRef>,
    /// 产出该 item 的源实例名(用于日志 + 主循环查 filter cfg)
    pub origin: String,
}

impl MediaItem {
    pub fn dedup_key(&self) -> (String, String) {
        (self.source.as_str().to_string(), self.source_id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> MediaItem {
        MediaItem {
            source: SourceKind::Pixiv,
            source_id: "123".into(),
            author: Author {
                name: "a".into(),
                url: "u".into(),
            },
            title: Some("t".into()),
            url: "w".into(),
            tags: vec!["原神".into()],
            bookmark_count: Some(500),
            is_r18: false,
            pixiv_type: Some(PixivType::Illust),
            page_count: 1,
            images: vec![ImageRef {
                url: "i".into(),
                referer: None,
            }],
            origin: "fav_artists".into(),
        }
    }

    #[test]
    fn dedup_key_combines_kind_and_id() {
        let item = sample();
        assert_eq!(item.dedup_key(), ("pixiv".to_string(), "123".to_string()));
    }
}
