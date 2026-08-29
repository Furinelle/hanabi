use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub poll_interval_secs: u64,
    /// 整点时间槽所用时区相对 UTC 的偏移(CST=+8,默认 8)。
    #[serde(default = "default_tz_offset")]
    pub tz_offset_hours: i64,
    pub telegram: TelegramCfg,
    pub gallery_dl: GalleryDlCfg,
    /// 抖音作者主页抓取桥接器。签名/Cookie 由 Python douyin-downloader 隔离处理。
    #[serde(default)]
    pub douyin: DouyinCfg,
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
    /// 定期从 Vitrine 补齐已发布图片指纹；0 表示关闭。
    #[serde(default = "default_gallery_fingerprint_sync_interval")]
    pub fingerprint_sync_interval_secs: u64,
}

fn default_gallery_fingerprint_sync_interval() -> u64 {
    6 * 3600
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

#[derive(Debug, Clone, Deserialize)]
pub struct DouyinCfg {
    /// Python 解释器路径。
    #[serde(default = "default_python_command")]
    pub python_command: String,
    /// Hanabi 自带的 douyin-downloader 桥接脚本。
    #[serde(default = "default_douyin_helper_path")]
    pub helper_path: String,
    /// 每个作者每轮最多拉取的作品页数，每页 20 条。
    #[serde(default = "default_douyin_max_pages")]
    pub max_pages: u32,
    /// 可选 Cookie JSON/header 文件路径；文件本身应 chmod 600 且不得提交。
    #[serde(default)]
    pub cookie_file: String,
    /// API 翻页受限时是否启用 douyin-downloader 的 Playwright 主页滚动兜底。
    #[serde(default)]
    pub browser_fallback: bool,
    /// 浏览器兜底是否无头运行。需要人工过验证码时应设为 false。
    #[serde(default)]
    pub browser_headless: bool,
}

impl Default for DouyinCfg {
    fn default() -> Self {
        Self {
            python_command: default_python_command(),
            helper_path: default_douyin_helper_path(),
            max_pages: default_douyin_max_pages(),
            cookie_file: String::new(),
            browser_fallback: false,
            browser_headless: false,
        }
    }
}

fn default_python_command() -> String {
    "python3".to_string()
}

fn default_douyin_helper_path() -> String {
    "tools/douyin_user_feed.py".to_string()
}

fn default_douyin_max_pages() -> u32 {
    3
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
    /// pixiv_user | pixiv_bookmarks | pixiv_ranking | x_list | x_foryou | douyin_user
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
        assert_eq!(cfg.douyin.max_pages, 3);
        assert_eq!(cfg.douyin.helper_path, "tools/douyin_user_feed.py");
        assert_eq!(cfg.sources.len(), 2);
        let bm = &cfg.sources[1];
        assert_eq!(bm.kind, "pixiv_bookmarks");
        assert_eq!(bm.filters.min_bookmarks, Some(500));
        assert_eq!(bm.filters.tags.as_deref(), Some(&["原神".to_string()][..]));
        assert!(bm.filters.illust_only);
        assert_eq!(bm.filters.max_pages, Some(5));
    }

    #[test]
    fn gallery_fingerprint_sync_defaults_to_six_hours() {
        let cfg: Config = toml::from_str(
            r#"
poll_interval_secs = 21600
[telegram]
channel_id = "1"
[gallery_dl]
config_path = "gallery-dl.conf"
[gallery]
endpoint = "https://gallery.example"
token = "secret"
"#,
        )
        .unwrap();
        assert_eq!(cfg.gallery.fingerprint_sync_interval_secs, 21600);
    }
}
