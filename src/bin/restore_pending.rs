use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hanabi::config::Config;
use hanabi::gallerydl::GalleryDl;
use hanabi::model::{MediaItem, SourceKind};
use hanabi::sink::telegram::prepare_all;
use hanabi::source::x::download_extra;
use rusqlite::Connection;

fn paths(raw: &str) -> Result<Vec<PathBuf>> {
    Ok(serde_json::from_str::<Vec<String>>(raw)?
        .into_iter()
        .map(PathBuf::from)
        .collect())
}

fn move_existing(old: &[PathBuf], dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    hanabi::util::restrict_dir(dir);
    for src in old.iter().filter(|p| p.exists()) {
        let name = src.file_name().context("待审文件没有文件名")?;
        let dst = dir.join(name);
        if src != &dst && !dst.exists() {
            std::fs::rename(src, &dst)
                .or_else(|_| std::fs::copy(src, &dst).map(|_| ()))
                .with_context(|| format!("迁移待审文件 {} 失败", src.display()))?;
        }
    }
    Ok(())
}

fn expected_in(dir: &Path, old: &[PathBuf]) -> Result<Vec<PathBuf>> {
    old.iter()
        .map(|p| {
            p.file_name()
                .map(|name| dir.join(name))
                .context("待审文件没有文件名")
        })
        .collect()
}

fn restore_one(
    gdl: &GalleryDl,
    x_size: Option<&str>,
    item: &MediaItem,
    old: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    let dir = hanabi::util::pending_dir(item.source.as_str(), &item.source_id);
    move_existing(old, &dir)?;
    let mut expected = expected_in(&dir, old)?;
    if expected.iter().any(|p| !p.exists()) {
        let extra = match item.source {
            SourceKind::X => download_extra(x_size),
            SourceKind::Pixiv => vec![],
            SourceKind::Douyin => anyhow::bail!("恢复工具暂不支持抖音待审记录"),
        };
        gdl.download(&item.url, &dir, &extra).with_context(|| {
            format!("重新下载 {}:{} 失败", item.source.as_str(), item.source_id)
        })?;
        expected = expected_in(&dir, old)?;
    }
    let missing: Vec<String> = expected
        .iter()
        .filter(|p| !p.exists())
        .map(|p| p.display().to_string())
        .collect();
    if !missing.is_empty() {
        anyhow::bail!("重新下载后仍缺文件: {}", missing.join(", "));
    }
    Ok(expected)
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cfg_path = std::env::var("HANABI_CONFIG").unwrap_or_else(|_| "config.toml".into());
    let cfg = Config::load(&cfg_path).context("加载 config.toml 失败")?;
    let gdl = GalleryDl {
        config_path: cfg.gallery_dl.config_path.clone(),
        probe_range: cfg.gallery_dl.probe_range.clone(),
    };
    let only: HashSet<i64> = std::env::args()
        .skip(1)
        .filter_map(|s| s.parse().ok())
        .collect();
    let mut db = Connection::open("hanabi.db")?;
    let rows: Vec<(i64, String, String, String)> = {
        let mut stmt = db.prepare(
            "SELECT token, files, originals, item_meta FROM pending WHERE state='pending' ORDER BY created_at, token",
        )?;
        let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        mapped.collect::<rusqlite::Result<_>>()?
    };
    let mut restored = 0usize;
    let mut failed = 0usize;
    for (token, files_raw, originals_raw, item_raw) in rows {
        if !only.is_empty() && !only.contains(&token) {
            continue;
        }
        let result: Result<()> = (|| {
            let item: MediaItem = serde_json::from_str(&item_raw).context("解析 item_meta 失败")?;
            let old_originals = paths(&originals_raw)?;
            let old_files = paths(&files_raw)?;
            let originals = restore_one(&gdl, cfg.x_image.size.as_deref(), &item, &old_originals)?;
            let prepared = prepare_all(&originals)?;
            let files_json = serde_json::to_string(
                &prepared
                    .iter()
                    .map(|p| p.to_string_lossy())
                    .collect::<Vec<_>>(),
            )?;
            let originals_json = serde_json::to_string(
                &originals
                    .iter()
                    .map(|p| p.to_string_lossy())
                    .collect::<Vec<_>>(),
            )?;
            let tx = db.transaction()?;
            let changed = tx.execute(
                "UPDATE pending SET files=?1, originals=?2 WHERE token=?3 AND state='pending'",
                rusqlite::params![files_json, originals_json, token],
            )?;
            if changed != 1 {
                anyhow::bail!("pending 状态已变化");
            }
            tx.commit()?;
            if let Some(parent) = old_files.first().and_then(|p| p.parent()) {
                if parent != hanabi::util::pending_dir(item.source.as_str(), &item.source_id) {
                    let _ = std::fs::remove_dir(parent);
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                restored += 1;
                println!("restored token={token}");
            }
            Err(e) => {
                failed += 1;
                eprintln!("failed token={token}: {e:#}");
            }
        }
    }
    println!("restore_summary restored={restored} failed={failed}");
    if failed > 0 {
        anyhow::bail!("有 {failed} 条待审记录恢复失败");
    }
    Ok(())
}
