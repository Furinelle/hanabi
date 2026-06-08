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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cfg_path = std::env::var("HANABI_CONFIG").unwrap_or_else(|_| "config.toml".into());
    let cfg = Config::load(&cfg_path).context("加载 config.toml 失败")?;
    let token = std::env::var("HANABI_BOT_TOKEN").context("缺少环境变量 HANABI_BOT_TOKEN")?;

    let store = Store::open("hanabi.db")?;
    let chain = FilterChain::standard();
    let sink = TelegramSink::new(
        token,
        cfg.telegram.channel_id.clone(),
        cfg.telegram.publish_channel.clone(),
    );
    // 手动触发通道:/run 命令经此通知抓取循环立即跑一轮。
    let (trigger_tx, mut trigger_rx) = tokio::sync::mpsc::channel::<()>(8);
    // 启动审批回调 + 命令轮询任务(监听按钮/命令,与抓取循环并发运行)。
    tokio::spawn(hanabi::sink::telegram::run_review_loop(
        sink.state(),
        trigger_tx,
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

    // 下载闭包:按来源决定 gallery-dl 额外参数(X 用 size=orig)。
    let gdl_dl = gdl.clone();
    let download = move |item: &MediaItem| -> Vec<PathBuf> {
        let dir = std::env::temp_dir().join(format!(
            "hanabi_{}_{}",
            item.source.as_str(),
            item.source_id
        ));
        let _ = std::fs::create_dir_all(&dir);
        let extra = match item.source {
            SourceKind::X => download_extra(x_size.as_deref()),
            SourceKind::Pixiv => vec![],
        };
        gdl_dl
            .download(&item.url, &dir, &extra)
            .unwrap_or_else(|e| {
                tracing::warn!(id = %item.source_id, error = %e, "gallery-dl 下载失败");
                Vec::new()
            })
    };

    let interval = Duration::from_secs(cfg.poll_interval_secs);
    loop {
        if let Err(e) = run_once(&store, &sources, &chain, &sink as &dyn Sink, &download).await {
            tracing::error!(error = %e, "本轮异常");
        }
        // 等下一轮:定时到点 或 收到 /run 手动触发,任一就绪即开始。
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = trigger_rx.recv() => {
                tracing::info!("收到 /run 手动触发,立即抓取");
            }
        }
    }
}
