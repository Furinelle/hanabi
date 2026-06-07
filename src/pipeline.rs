use std::path::PathBuf;

use anyhow::Result;

use crate::filter::FilterChain;
use crate::model::MediaItem;
use crate::sink::Sink;
use crate::source::Source;
use crate::store::Store;

/// 主循环一轮。`download` 注入下载逻辑(真实=gallery-dl;测试=空),
/// 返回该 item 的本地文件路径。分级隔离:单源/单 item 失败不影响其余。
pub async fn run_once<F>(
    store: &Store,
    sources: &[Box<dyn Source>],
    chain: &FilterChain,
    sink: &dyn Sink,
    download: F,
) -> Result<()>
where
    F: Fn(&MediaItem) -> Vec<PathBuf>,
{
    for src in sources {
        let cfg = src.filter_cfg();
        let items = match src.fetch(store).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(source = src.name(), error = %e, "fetch 失败,跳过该源");
                continue;
            }
        };
        for item in items {
            if store.already_pushed(&item)? {
                continue;
            }
            if !chain.keep(&item, cfg) {
                continue;
            }
            let files = download(&item);
            match sink.deliver(&item, &files).await {
                Ok(_) => store.mark_pushed(&item)?,
                Err(e) => {
                    tracing::warn!(id = item.source_id, error = %e, "交付失败,下轮重试");
                }
            }
        }
    }
    Ok(())
}
