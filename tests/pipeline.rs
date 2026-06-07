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
        author: Author { name: "a".into(), url: "u".into() },
        title: Some("t".into()),
        url: "w".into(),
        tags: vec!["原神".into()],
        bookmark_count: Some(bookmarks),
        is_r18: false,
        pixiv_type: Some(PixivType::Illust),
        page_count: 1,
        images: vec![ImageRef { url: "i".into(), referer: None }],
        origin: "mock".into(),
    }
}

struct MockSource {
    items: Vec<MediaItem>,
    cfg: SourceFilterCfg,
}
#[async_trait]
impl Source for MockSource {
    fn name(&self) -> &str { "mock" }
    fn filter_cfg(&self) -> &SourceFilterCfg { &self.cfg }
    async fn fetch(&self, _: &Store) -> anyhow::Result<Vec<MediaItem>> {
        Ok(self.items.clone())
    }
}

#[derive(Default)]
struct MockSink { delivered: Mutex<Vec<String>> }
#[async_trait]
impl Sink for MockSink {
    async fn deliver(&self, item: &MediaItem, _files: &[PathBuf]) -> anyhow::Result<()> {
        self.delivered.lock().unwrap().push(item.source_id.clone());
        Ok(())
    }
}

#[tokio::test]
async fn filters_dedupes_and_delivers() {
    let store = Store::open_in_memory().unwrap();
    let cfg = SourceFilterCfg { min_bookmarks: Some(500), tags: Some(vec!["原神".into()]), ..Default::default() };
    let src = MockSource { items: vec![item("low", 100), item("hi", 800)], cfg };
    let sink = MockSink::default();
    let sources: Vec<Box<dyn Source>> = vec![Box::new(src)];

    run_once(&store, &sources, &FilterChain::standard(), &sink, |_| vec![]).await.unwrap();
    assert_eq!(*sink.delivered.lock().unwrap(), vec!["hi".to_string()]);

    run_once(&store, &sources, &FilterChain::standard(), &sink, |_| vec![]).await.unwrap();
    assert_eq!(sink.delivered.lock().unwrap().len(), 1);
}
