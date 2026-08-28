use std::path::PathBuf;

use anyhow::{Context, Result};
use hanabi::gallery_catalog::{scan_catalog, CatalogImage};

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let manifest = PathBuf::from(
        args.next()
            .context("用法: gallery_catalog <manifest.json> <report.json>")?,
    );
    let report_path = PathBuf::from(
        args.next()
            .context("用法: gallery_catalog <manifest.json> <report.json>")?,
    );
    if args.next().is_some() {
        anyhow::bail!("用法: gallery_catalog <manifest.json> <report.json>");
    }

    let images: Vec<CatalogImage> = serde_json::from_slice(
        &std::fs::read(&manifest)
            .with_context(|| format!("读取清单失败: {}", manifest.display()))?,
    )
    .context("解析图库清单失败")?;
    let report = scan_catalog(&images)?;
    std::fs::write(&report_path, serde_json::to_vec_pretty(&report)?)
        .with_context(|| format!("写扫描报告失败: {}", report_path.display()))?;
    eprintln!(
        "scanned={} strict_groups={} similar_pairs={}",
        report.scanned_images,
        report.strict_groups.len(),
        report.similar_pairs.len()
    );
    Ok(())
}
