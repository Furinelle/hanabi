//! Prints the Vitrine ingest fingerprint of each image path, one JSON object per line,
//! for tools that upload images Hanabi did not download itself.
use std::path::Path;

use anyhow::Result;
use hanabi::gallery::fingerprint_json;
use hanabi::image_dedup::inspect_image;

fn main() -> Result<()> {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        anyhow::bail!("用法: fingerprint <图片路径>...");
    }
    for path in paths {
        println!("{}", fingerprint_json(&inspect_image(Path::new(&path))?));
    }
    Ok(())
}
