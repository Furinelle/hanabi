use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use hanabi::config::Config;
use hanabi::filter::FilterChain;
use hanabi::gallerydl::GalleryDl;
use hanabi::model::{MediaItem, SourceKind};
use hanabi::pipeline::run_once;
use hanabi::sink::telegram::TelegramSink;
use hanabi::sink::Sink;
use hanabi::source::pixiv::PixivSource;
use hanabi::source::x::{download_extra, XSource};
use hanabi::source::Source;
use hanabi::store::Store;

/// 计算距下一个整点时间槽的秒数（CST = UTC+8）。
/// poll_interval_secs 须能整除 86400，例如 28800 → 00:00 / 08:00 / 16:00。
fn secs_until_next_slot(interval_secs: u64) -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let utc = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let local = utc + 8 * 3600; // CST (UTC+8)
    let secs_into_day = local % 86400;
    let next_slot = ((secs_into_day / interval_secs) + 1) * interval_secs;
    next_slot - secs_into_day
}

/// 下载单个作品到独立临时目录(X 用 size=orig)。供定时抓取与手动链接共用。
fn download_work(gdl: &GalleryDl, item: &MediaItem, x_size: Option<&str>) -> Vec<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "hanabi_{}_{}",
        item.source.as_str(),
        item.source_id
    ));
    let _ = std::fs::create_dir_all(&dir);
    let extra = match item.source {
        SourceKind::X => download_extra(x_size),
        SourceKind::Pixiv => vec![],
    };
    gdl.download(&item.url, &dir, &extra).unwrap_or_else(|e| {
        tracing::warn!(id = %item.source_id, error = %e, "gallery-dl 下载失败");
        Vec::new()
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cfg_path = std::env::var("HANABI_CONFIG").unwrap_or_else(|_| "config.toml".into());
    let cfg = Config::load(&cfg_path).context("加载 config.toml 失败")?;
    // 校验:整点时间槽要求 poll_interval_secs 能整除 86400。
    if cfg.poll_interval_secs == 0 || 86400 % cfg.poll_interval_secs != 0 {
        tracing::warn!(
            poll = cfg.poll_interval_secs,
            "poll_interval_secs 不能整除 86400,整点时间槽将不均匀(建议 21600/28800/43200)"
        );
    }
    let token = std::env::var("HANABI_BOT_TOKEN").context("缺少环境变量 HANABI_BOT_TOKEN")?;

    let store = Store::open("hanabi.db")?;
    let chain = FilterChain::standard();
    let sink = TelegramSink::new(
        token,
        cfg.telegram.channel_id.clone(),
        cfg.telegram.publish_channel.clone(),
        "hanabi.db",
    )?;
    // 手动触发通道:/run 命令经此通知抓取循环立即跑一轮。
    let (trigger_tx, mut trigger_rx) = tokio::sync::mpsc::channel::<()>(8);
    // 手动链接通道:发来的 Pixiv/X 作品链接经此交抓取循环直发频道。
    let (link_tx, mut link_rx) = tokio::sync::mpsc::channel::<String>(16);
    // 启动审批回调 + 命令/链接轮询任务(与抓取循环并发运行)。
    tokio::spawn(hanabi::sink::telegram::run_review_loop(
        sink.state(),
        trigger_tx,
        link_tx,
    ));
    let gdl = Arc::new(GalleryDl {
        config_path: cfg.gallery_dl.config_path.clone(),
        probe_range: cfg.gallery_dl.probe_range.clone(),
    });

    let x_size = cfg.x_image.size.clone();
    let sources: Vec<Box<dyn Source>> = cfg
        .sources
        .iter()
        .map(|s| -> Box<dyn Source> {
            if s.kind.starts_with("pixiv") {
                Box::new(PixivSource::new(s.clone(), gdl.clone()))
            } else {
                Box::new(XSource::new(s.clone(), gdl.clone()))
            }
        })
        .collect();

    // 下载闭包:复用 download_work。x_size 克隆给闭包,原值留给手动链接(handle_link)。
    let gdl_dl = gdl.clone();
    let x_size_dl = x_size.clone();
    let download =
        move |item: &MediaItem| -> Vec<PathBuf> { download_work(&gdl_dl, item, x_size_dl.as_deref()) };

    // 启动立即跑首轮。
    if let Err(e) = run_once(&store, &sources, &chain, &sink as &dyn Sink, &download).await {
        tracing::error!(error = %e, "本轮异常");
    }
    loop {
        let wait = secs_until_next_slot(cfg.poll_interval_secs);
        tracing::info!(wait_secs = wait, "下次抓取在 {:.1} 小时后", wait as f64 / 3600.0);
        // 整点时间槽到点 / /run 手动触发 → 跑一轮;手动链接 → 直发频道(不跑全量)。
        let do_fetch = tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(wait)) => true,
            _ = trigger_rx.recv() => {
                tracing::info!("收到 /run 手动触发,立即抓取");
                true
            }
            Some(url) = link_rx.recv() => {
                tracing::info!(url = %url, "收到手动链接,直发频道");
                if let Err(e) = handle_link(&url, &gdl, x_size.as_deref(), &sink, &store).await {
                    tracing::warn!(error = %e, "手动链接处理失败");
                }
                false
            }
        };
        if do_fetch {
            if let Err(e) = run_once(&store, &sources, &chain, &sink as &dyn Sink, &download).await {
                tracing::error!(error = %e, "本轮异常");
            }
        }
    }
}

/// 处理手动发来的作品链接:probe + 解析 + 下载,直接发布到频道(跳过审批)。
/// 发布前查去重,已发过的跳过,避免重复进频道。
async fn handle_link(
    url: &str,
    gdl: &Arc<GalleryDl>,
    x_size: Option<&str>,
    sink: &TelegramSink,
    store: &Store,
) -> Result<()> {
    let is_pixiv = url.contains("pixiv");
    let g = gdl.clone();
    let u = url.to_string();
    let val = tokio::task::spawn_blocking(move || g.probe(&u)).await??;
    let items = if is_pixiv {
        hanabi::gallerydl::parse_pixiv(&val, "manual")
    } else {
        hanabi::source::x::parse_twitter(&val, "manual")
    };
    if items.is_empty() {
        anyhow::bail!("链接未解析出作品(确认是作品/推文页)");
    }
    for item in &items {
        if store.already_pushed(item)? {
            tracing::info!(id = %item.source_id, "手动链接作品已发过,跳过");
            continue;
        }
        let files = download_work(gdl, item, x_size);
        if files.is_empty() {
            continue;
        }
        sink.publish_direct(item, &files).await?;
        let _ = store.mark_pushed(item);
    }
    Ok(())
}
