use std::path::PathBuf;

use anyhow::Result;

use crate::filter::FilterChain;
use crate::model::MediaItem;
use crate::sink::Sink;
use crate::source::Source;
use crate::store::Store;

/// 主循环一轮。`download` 注入下载逻辑(真实=gallery-dl 包 spawn_blocking;测试=空),
/// 返回该 item 的本地文件路径。改为返回 future:gallery-dl 子进程是同步等待、
/// 单作品可达数分钟,同步闭包会占死 tokio worker(单核 VPS 上审批全冻结)。
/// 分级隔离:单源/单 item 失败不影响其余。
pub async fn run_once<F, Fut>(
    store: &Store,
    sources: &[Box<dyn Source>],
    chain: &FilterChain,
    sink: &dyn Sink,
    download: F,
) -> Result<()>
where
    F: Fn(MediaItem) -> Fut,
    Fut: std::future::Future<Output = Vec<PathBuf>>,
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
            let files = download(item.clone()).await;
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
