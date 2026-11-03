use std::path::PathBuf;

use anyhow::Result;

use crate::filter::FilterChain;
use crate::model::{MediaItem, SourceKind};
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
    // 抓取失败的源排到本轮末尾再试一次(多为抖音偶发风控 403),仍失败才等下一轮。
    let mut queue: std::collections::VecDeque<_> = sources.iter().map(|s| (s, false)).collect();
    while let Some((src, retried)) = queue.pop_front() {
        let cfg = src.filter_cfg();
        let items = match src.fetch(store).await {
            Ok(v) => v,
            Err(e) if !retried => {
                tracing::warn!(source = src.name(), error = %e, "fetch 失败,本轮末尾重试");
                queue.push_back((src, true));
                continue;
            }
            Err(e) => {
                tracing::warn!(source = src.name(), error = %e, "fetch 重试仍失败,跳过该源");
                continue;
            }
        };
        let mut candidates = Vec::new();
        for item in items {
            if item.source == SourceKind::Douyin {
                store.remember_douyin_author(&item.author.url, &item.author.name)?;
                if store.is_douyin_disabled(&item.author.url)? {
                    continue;
                }
            }
            if store.is_author_blocked(&item)?
                || store.already_pushed(&item)?
                || store.is_quota_dropped(&item)?
                || !chain.keep(&item, cfg)
            {
                continue;
            }
            candidates.push(item);
        }
        if let Some(max) = cfg.max_per_poll {
            if candidates.len() > max {
                // 稳定排序:分数高的在前,None 最小;同分保持抓取顺序。
                candidates.sort_by_key(|i| std::cmp::Reverse(i.bookmark_count));
                for item in candidates.split_off(max) {
                    store.defer_over_quota(&item)?;
                }
            }
        }
        for item in candidates {
            // 手动链接可能正在处理同一作品:持锁后再查重,避免重复交付。
            let _work = crate::util::work_lock(item.source.as_str(), &item.source_id).await;
            if store.already_pushed(&item)? {
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
