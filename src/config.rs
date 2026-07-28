use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub poll_interval_secs: u64,
    /// 整点时间槽所用时区相对 UTC 的偏移(CST=+8,默认 8)。
    #[serde(default = "default_tz_offset")]
    pub tz_offset_hours: i64,
    pub telegram: TelegramCfg,
    pub gallery_dl: GalleryDlCfg,
    #[serde(default)]
    pub x_image: XImageCfg,
    /// 可选:Vitrine 图库入库(CF Workers)。未配置时不显示「发送并入库」按钮。
    #[serde(default)]
    pub gallery: GalleryCfg,
    #[serde(rename = "source", default)]
    pub sources: Vec<SourceCfg>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct GalleryCfg {
    /// 图库 Workers 根地址,如 `https://vitrine.xxx.workers.dev`
    #[serde(default)]
    pub endpoint: String,
    /// 入库 token(与 Worker secret INGEST_TOKEN 一致)。也可用环境变量 HANABI_GALLERY_TOKEN。
    #[serde(default)]
    pub token: String,
}

impl GalleryCfg {
    pub fn enabled(&self) -> bool {
        let token = if self.token.is_empty() {
            std::env::var("HANABI_GALLERY_TOKEN").unwrap_or_default()
        } else {
            self.token.clone()
        };
        !self.endpoint.trim().is_empty() && !token.trim().is_empty()
    }

    pub fn resolved_token(&self) -> String {
        if !self.token.is_empty() {
            self.token.clone()
        } else {
            std::env::var("HANABI_GALLERY_TOKEN").unwrap_or_default()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct TelegramCfg {
    /// 审批私聊:抓到的作品先发到这里等人工审批。
    pub channel_id: String,
    /// 批准后发布的目标频道(@username 或数字 id)。
    #[serde(default)]
    pub publish_channel: String,
}

#[derive(Debug, Deserialize)]
pub struct GalleryDlCfg {
    pub config_path: String,
    #[serde(default = "default_range")]
    pub probe_range: String,
}

fn default_range() -> String {
    "1-20".to_string()
}

fn default_tz_offset() -> i64 {
    8
}

#[derive(Debug, Deserialize, Default)]
pub struct XImageCfg {
    #[serde(default)]
    pub size: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SourceCfg {
    pub name: String,
    /// pixiv_user | pixiv_bookmarks | pixiv_ranking | x_list | x_foryou
    pub kind: String,
    pub targets: Vec<String>,
    #[serde(default)]
    pub filters: SourceFilterCfg,
}

/// 每源过滤配置。语义:r18=false 表示「过滤掉 R18,只留全年龄」。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SourceFilterCfg {
    #[serde(default)]
    pub r18: bool,
    #[serde(default)]
    pub min_bookmarks: Option<u32>,
    #[serde(default)]
    pub min_likes: Option<u32>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub illust_only: bool,
    #[serde(default)]
    pub max_pages: Option<u32>,
    #[serde(default)]
    pub require_media: bool,
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
poll_interval_secs = 1800
[telegram]
channel_id = "@my_channel"
[gallery_dl]
config_path = "gallery-dl.conf"
[[source]]
name = "fav_artists"
kind = "pixiv_user"
targets = ["https://www.pixiv.net/users/123"]
filters = { r18 = false }
[[source]]
name = "my_bookmarks"
kind = "pixiv_bookmarks"
targets = ["https://www.pixiv.net/users/0/bookmarks/artworks"]
filters = { r18 = false, min_bookmarks = 500, tags = ["原神"], illust_only = true, max_pages = 5 }
"#;

    #[test]
    fn parses_sources_and_filters() {
        let cfg: Config = toml::from_str(SAMPLE).unwrap();
        assert_eq!(cfg.poll_interval_secs, 1800);
        assert_eq!(cfg.telegram.channel_id, "@my_channel");
        assert_eq!(cfg.gallery_dl.probe_range, "1-20"); // default
        assert_eq!(cfg.sources.len(), 2);
        let bm = &cfg.sources[1];
        assert_eq!(bm.kind, "pixiv_bookmarks");
        assert_eq!(bm.filters.min_bookmarks, Some(500));
        assert_eq!(bm.filters.tags.as_deref(), Some(&["原神".to_string()][..]));
        assert!(bm.filters.illust_only);
        assert_eq!(bm.filters.max_pages, Some(5));
    }
}
