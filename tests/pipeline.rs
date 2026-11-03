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

#[tokio::test]
async fn removed_douyin_author_is_not_downloaded_or_marked_pushed() {
    let store = Store::open_in_memory().unwrap();
    let url = "https://www.douyin.com/user/MS4wREMOVED";
    store.subscribe_douyin(url, url).unwrap();
    let id = store.list_douyin_monitors().unwrap()[0].id;
    store.unsubscribe_douyin(id).unwrap();
    let mut removed = item("removed", 100);
    removed.source = SourceKind::Douyin;
    removed.author.url = url.into();
    let sources: Vec<Box<dyn Source>> = vec![Box::new(MockSource {
        items: vec![removed.clone(), item("keep", 100)],
        cfg: Default::default(),
    })];
    let sink = MockSink::default();
    run_once(
        &store,
        &sources,
        &FilterChain::standard(),
        &sink,
        |work| async move {
            assert_ne!(work.source_id, "removed");
            Vec::<PathBuf>::new()
        },
    )
    .await
    .unwrap();
    assert_eq!(*sink.delivered.lock().unwrap(), vec!["keep"]);
    assert!(!store.already_pushed(&removed).unwrap());
}

#[tokio::test]
async fn blocked_author_is_skipped_until_unblocked() {
    let store = Store::open_in_memory().unwrap();
    let mut blocked = item("blocked", 100);
    blocked.author.url = "https://www.pixiv.net/users/1".into();
    store.block_author(&blocked).unwrap();
    let sources: Vec<Box<dyn Source>> = vec![Box::new(MockSource {
        items: vec![blocked.clone(), item("keep", 100)],
        cfg: Default::default(),
    })];
    let sink = MockSink::default();
    run_once(
        &store,
        &sources,
        &FilterChain::standard(),
        &sink,
        |work| async move {
            assert_ne!(work.source_id, "blocked");
            Vec::<PathBuf>::new()
        },
    )
    .await
    .unwrap();
    assert_eq!(*sink.delivered.lock().unwrap(), vec!["keep"]);
    assert!(!store.already_pushed(&blocked).unwrap());

    let id = store.list_blocked_authors().unwrap()[0].id;
    store.unblock_author(id).unwrap();
    run_once(
        &store,
        &sources,
        &FilterChain::standard(),
        &sink,
        |_| async { vec![] },
    )
    .await
    .unwrap();
    assert_eq!(*sink.delivered.lock().unwrap(), vec!["keep", "blocked"]);
}

/// 每轮配额:超出的低分作品先暂缓,下一轮刷新分数后仍在配额外才彻底舍弃。
#[tokio::test]
async fn over_quota_items_deferred_once_then_dropped() {
    let store = Store::open_in_memory().unwrap();
    let sink = MockSink::default();
    let round = |items: Vec<MediaItem>| -> Vec<Box<dyn Source>> {
        vec![Box::new(MockSource {
            items,
            cfg: SourceFilterCfg {
                max_per_poll: Some(1),
                ..Default::default()
            },
        })]
    };
    for items in [
        vec![item("b", 100), item("a", 900), item("c", 50)],
        // b 涨上来进配额;c 第二次落在配额外被舍弃;d 新出现只暂缓。
        vec![item("b", 2000), item("c", 60), item("d", 500)],
        // c 即使分数暴涨也不再送审;d 这轮进配额。
        vec![item("c", 5000), item("d", 10)],
    ] {
        run_once(
            &store,
            &round(items),
            &FilterChain::standard(),
            &sink,
            |_| async { vec![] },
        )
        .await
        .unwrap();
    }
    assert_eq!(*sink.delivered.lock().unwrap(), vec!["a", "b", "d"]);
    assert!(store.is_quota_dropped(&item("c", 0)).unwrap());
}

/// 首次 fetch 失败,之后成功;记录每次 fetch 的源名以检查顺序。
struct FlakySource {
    name: &'static str,
    fails_left: Mutex<u32>,
    items: Vec<MediaItem>,
    log: std::sync::Arc<Mutex<Vec<&'static str>>>,
    cfg: SourceFilterCfg,
}
#[async_trait]
impl Source for FlakySource {
    fn name(&self) -> &str {
        self.name
    }
    fn filter_cfg(&self) -> &SourceFilterCfg {
        &self.cfg
    }
    async fn fetch(&self, _: &Store) -> anyhow::Result<Vec<MediaItem>> {
        self.log.lock().unwrap().push(self.name);
        let mut fails = self.fails_left.lock().unwrap();
        if *fails > 0 {
            *fails -= 1;
            anyhow::bail!("mock 403");
        }
        Ok(self.items.clone())
    }
}

#[tokio::test]
async fn failed_fetch_retried_once_at_end_of_round() {
    let store = Store::open_in_memory().unwrap();
    let log = std::sync::Arc::new(Mutex::new(Vec::new()));
    let source = |name, fails, id| FlakySource {
        name,
        fails_left: Mutex::new(fails),
        items: vec![item(id, 800)],
        log: log.clone(),
        cfg: SourceFilterCfg::default(),
    };
    let sources: Vec<Box<dyn Source>> = vec![
        Box::new(source("flaky", 1, "f")),
        Box::new(source("ok", 0, "o")),
        Box::new(source("dead", 2, "d")),
    ];
    let sink = MockSink::default();
    run_once(
        &store,
        &sources,
        &FilterChain::standard(),
        &sink,
        |_| async { Vec::<PathBuf>::new() },
    )
    .await
    .unwrap();
    assert_eq!(
        *log.lock().unwrap(),
        vec!["flaky", "ok", "dead", "flaky", "dead"]
    );
    assert_eq!(*sink.delivered.lock().unwrap(), vec!["o", "f"]);
}
