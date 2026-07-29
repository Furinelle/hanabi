use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use hanabi::config::SourceFilterCfg;
use hanabi::filter::FilterChain;
use hanabi::model::{Author, ImageRef, MediaItem, PixivType, SourceKind};
use hanabi::pipeline::run_once;
use hanabi::sink::Sink;
use hanabi::source::Source;
use hanabi::store::Store;

fn item(id: &str, bookmarks: u32) -> MediaItem {
    MediaItem {
        source: SourceKind::Pixiv,
        source_id: id.into(),
        author: Author {
            name: "a".into(),
            url: "u".into(),
        },
        title: Some("t".into()),
        url: "w".into(),
        tags: vec!["原神".into()],
        bookmark_count: Some(bookmarks),
        is_r18: false,
        pixiv_type: Some(PixivType::Illust),
        page_count: 1,
        images: vec![ImageRef {
            url: "i".into(),
            referer: None,
            fallback_urls: vec![],
        }],
        origin: "mock".into(),
    }
}

struct MockSource {
    items: Vec<MediaItem>,
    cfg: SourceFilterCfg,
}
#[async_trait]
impl Source for MockSource {
    fn name(&self) -> &str {
        "mock"
    }
    fn filter_cfg(&self) -> &SourceFilterCfg {
        &self.cfg
    }
    async fn fetch(&self, _: &Store) -> anyhow::Result<Vec<MediaItem>> {
        Ok(self.items.clone())
    }
}

#[derive(Default)]
struct MockSink {
    delivered: Mutex<Vec<String>>,
    /// 这些 id 的 deliver 直接返回 Err,模拟发送失败。
    fail_ids: std::collections::HashSet<String>,
}
#[async_trait]
impl Sink for MockSink {
    async fn deliver(&self, item: &MediaItem, _files: &[PathBuf]) -> anyhow::Result<()> {
        if self.fail_ids.contains(&item.source_id) {
            anyhow::bail!("mock deliver 失败: {}", item.source_id);
        }
        self.delivered.lock().unwrap().push(item.source_id.clone());
        Ok(())
    }
}

#[tokio::test]
async fn filters_dedupes_and_delivers() {
    let store = Store::open_in_memory().unwrap();
    let cfg = SourceFilterCfg {
        min_bookmarks: Some(500),
        tags: Some(vec!["原神".into()]),
        ..Default::default()
    };
    let src = MockSource {
        items: vec![item("low", 100), item("hi", 800)],
        cfg,
    };
    let sink = MockSink::default();
    let sources: Vec<Box<dyn Source>> = vec![Box::new(src)];

    run_once(
        &store,
        &sources,
        &FilterChain::standard(),
        &sink,
        |_| async { vec![] },
    )
    .await
    .unwrap();
    assert_eq!(*sink.delivered.lock().unwrap(), vec!["hi".to_string()]);

    run_once(
        &store,
        &sources,
        &FilterChain::standard(),
        &sink,
        |_| async { vec![] },
    )
    .await
    .unwrap();
    assert_eq!(sink.delivered.lock().unwrap().len(), 1);
}

/// 核心语义锁定:deliver 失败不入去重库(下轮自动重试),成功后只投一次。
#[tokio::test]
async fn failed_delivery_not_marked_and_retried_next_round() {
    let store = Store::open_in_memory().unwrap();
    let src = MockSource {
        items: vec![item("a", 800)],
        cfg: SourceFilterCfg::default(),
    };
    let sources: Vec<Box<dyn Source>> = vec![Box::new(src)];

    // 第一轮:deliver 失败 → 不 mark_pushed。
    let failing = MockSink {
        fail_ids: ["a".to_string()].into_iter().collect(),
        ..Default::default()
    };
    run_once(
        &store,
        &sources,
        &FilterChain::standard(),
        &failing,
        |_| async { vec![] },
    )
    .await
    .unwrap();
    assert!(failing.delivered.lock().unwrap().is_empty());

    // 第二轮:恢复成功 → 重新投递。
    let ok_sink = MockSink::default();
    run_once(
        &store,
        &sources,
        &FilterChain::standard(),
        &ok_sink,
        |_| async { vec![] },
    )
    .await
    .unwrap();
    assert_eq!(*ok_sink.delivered.lock().unwrap(), vec!["a".to_string()]);

    // 第三轮:已入库,不再投递。
    run_once(
        &store,
        &sources,
        &FilterChain::standard(),
        &ok_sink,
        |_| async { vec![] },
    )
    .await
    .unwrap();
    assert_eq!(ok_sink.delivered.lock().unwrap().len(), 1);
}
