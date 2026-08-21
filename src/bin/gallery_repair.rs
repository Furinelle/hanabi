use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use hanabi::config::Config;
use hanabi::gallery::GalleryClient;
use hanabi::gallery_outbox::{GalleryOutbox, RetryOutcome};
use hanabi::gallery_repair::normalize_douyin_target;
use hanabi::source::douyin;

fn usage() -> &'static str {
    "用法: gallery_repair [--dry-run] douyin <作品 URL 或 ID>"
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let mut dry_run = false;
    let mut positional = Vec::new();
    for arg in std::env::args().skip(1) {
        if arg == "--dry-run" {
            dry_run = true;
        } else {
            positional.push(arg);
        }
    }
    if positional.len() != 2 || positional[0] != "douyin" {
        anyhow::bail!(usage());
    }
    let raw_target = &positional[1];
    let target = normalize_douyin_target(raw_target)?;
    let expected_id = raw_target
        .bytes()
        .all(|byte| byte.is_ascii_digit())
        .then_some(raw_target.as_str());

    let cfg_path = std::env::var("HANABI_CONFIG").unwrap_or_else(|_| "config.toml".into());
    let cfg = Config::load(&cfg_path).context("加载 config.toml 失败")?;
    if !cfg.gallery.enabled() {
        anyhow::bail!("[gallery] endpoint/token 未配置，无法补图库");
    }
    let client = douyin::build_client()?;
    let item = douyin::fetch_note(&client, &cfg.douyin, &target, "gallery_repair")
        .await
        .context("重新抓取抖音作品失败")?;
    if let Some(expected_id) = expected_id {
        if item.source_id != expected_id {
            anyhow::bail!(
                "抓取结果 ID 不匹配: expected={expected_id} actual={}",
                item.source_id
            );
        }
    }

    if dry_run {
        println!(
            "dry_run source=douyin id={} images={} action=none telegram=untouched pushed=untouched",
            item.source_id,
            item.images.len()
        );
        return Ok(());
    }

    let outbox = GalleryOutbox::for_database("hanabi.db")?;
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let download_dir: PathBuf = outbox
        .root()
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("gallery-repair-downloads")
        .join(format!("douyin_{}_{}", item.source_id, unique));
    let (files, failed) = douyin::download_images(&client, &item, &download_dir).await;
    if failed > 0 || files.len() != item.images.len() {
        anyhow::bail!(
            "重新下载不完整: downloaded={} expected={} failed={failed}; 已保留目录 {}",
            files.len(),
            item.images.len(),
            download_dir.display()
        );
    }

    let queued = outbox
        .enqueue(&item, &files, "manual gallery repair")
        .with_context(|| {
            format!(
                "登记图库补偿队列失败；已保留下载目录 {}",
                download_dir.display()
            )
        })?;
    println!(
        "queued source={} id={} copies={} outbox={}",
        queued.source_kind,
        queued.source_id,
        queued.files.len(),
        outbox.root().display()
    );
    // 入队成功后即使删除抓取目录，失败重试仍使用 outbox 独立副本。
    if let Err(error) = std::fs::remove_dir_all(&download_dir) {
        tracing::warn!(error = %error, dir = %download_dir.display(), "清理图库修复下载目录失败");
    }

    let gallery = GalleryClient::new(cfg.gallery.endpoint.clone(), cfg.gallery.resolved_token())?;
    match outbox
        .retry_source_now(&gallery, item.source.as_str(), &item.source_id)
        .await?
    {
        RetryOutcome::Succeeded => println!(
            "repair_result source={} id={} status=uploaded telegram=untouched pushed=untouched",
            item.source.as_str(),
            item.source_id
        ),
        RetryOutcome::Failed => anyhow::bail!(
            "图库补传失败；队列/dead-letter 与持久副本已保留；可重试错误将由后台继续，永久错误请检查日志 source={} id={}",
            item.source.as_str(),
            item.source_id
        ),
        RetryOutcome::Missing => anyhow::bail!("图库补偿队列记录意外缺失"),
    }
    Ok(())
}
