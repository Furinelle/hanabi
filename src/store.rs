use anyhow::Result;
use rusqlite::{params, Connection};

use crate::model::MediaItem;

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(&conn)?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init(&conn)?;
        Ok(Self { conn })
    }

    fn init(conn: &Connection) -> Result<()> {
        // WAL + busy_timeout:抓取循环与审批任务两条连接并发写 hanabi.db,
        // 不加锁会撞 "database is locked";sink 端连接也启用了 WAL,此处对齐。
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;",
        )?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS pushed (
                 source_kind TEXT NOT NULL,
                 source_id   TEXT NOT NULL,
                 pushed_at   INTEGER NOT NULL,
                 PRIMARY KEY (source_kind, source_id)
             );
             CREATE TABLE IF NOT EXISTS douyin_subscriptions (
                 profile_url TEXT PRIMARY KEY,
                 shared_url TEXT NOT NULL
             );",
        )?;
        Ok(())
    }

    pub fn subscribe_douyin(&self, profile_url: &str, shared_url: &str) -> Result<bool> {
        let profile_url = crate::source::douyin::canonical_user_profile(profile_url)
            .ok_or_else(|| anyhow::anyhow!("不是有效的抖音作者主页"))?;
        Ok(self.conn.execute(
            "INSERT OR IGNORE INTO douyin_subscriptions (profile_url, shared_url) VALUES (?1, ?2)",
            params![profile_url, shared_url],
        )? > 0)
    }

    pub fn douyin_subscriptions(&self) -> Result<Vec<(String, String)>> {
        let mut statement = self
            .conn
            .prepare("SELECT profile_url, shared_url FROM douyin_subscriptions ORDER BY rowid")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn already_pushed(&self, item: &MediaItem) -> Result<bool> {
        let (kind, id) = item.dedup_key();
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM pushed WHERE source_kind = ?1 AND source_id = ?2",
            params![kind, id],
            |row| row.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn mark_pushed(&self, item: &MediaItem) -> Result<()> {
        let (kind, id) = item.dedup_key();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;
        self.conn.execute(
            "INSERT OR IGNORE INTO pushed (source_kind, source_id, pushed_at) VALUES (?1, ?2, ?3)",
            params![kind, id, ts],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Author, ImageRef, MediaItem, PixivType, SourceKind};

    fn item(id: &str) -> MediaItem {
        MediaItem {
            source: SourceKind::Pixiv,
            source_id: id.into(),
            author: Author {
                name: "a".into(),
                url: "u".into(),
            },
            title: None,
            url: "w".into(),
            tags: vec![],
            bookmark_count: Some(1),
            is_r18: false,
            pixiv_type: Some(PixivType::Illust),
            page_count: 1,
            images: vec![ImageRef {
                url: "i".into(),
                referer: None,
                fallback_urls: vec![],
            }],
            origin: "s".into(),
        }
    }

    #[test]
    fn open_sets_wal_and_busy_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let store = Store::open(path.to_str().unwrap()).unwrap();
        let mode: String = store
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        let busy: i64 = store
            .conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(busy, 5000);
    }

    #[test]
    fn mark_then_already_pushed() {
        let store = Store::open_in_memory().unwrap();
        let it = item("123");
        assert!(!store.already_pushed(&it).unwrap());
        store.mark_pushed(&it).unwrap();
        assert!(store.already_pushed(&it).unwrap());
    }

    #[test]
    fn mark_is_idempotent() {
        let store = Store::open_in_memory().unwrap();
        let it = item("123");
        store.mark_pushed(&it).unwrap();
        store.mark_pushed(&it).unwrap();
        assert!(store.already_pushed(&it).unwrap());
    }
    #[test]
    fn douyin_subscriptions_survive_reopen_and_deduplicate_cards() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subscriptions.db");
        let profile = "https://www.douyin.com/user/MS4wTEST";
        {
            let store = Store::open(path.to_str().unwrap()).unwrap();
            assert!(store
                .subscribe_douyin(
                    "https://www.iesdouyin.com/share/user/MS4wTEST/?share=1",
                    "https://v.douyin.com/card1/",
                )
                .unwrap());
            assert!(!store
                .subscribe_douyin(profile, "https://v.douyin.com/card2/")
                .unwrap());
            for invalid in [
                "https://www.douyin.com/note/123",
                "https://evil.example/user/MS4wTEST",
                "https://www.douyin.com/user/",
                "https://www.douyin.com/user/MS4wTEST/extra",
            ] {
                assert!(store.subscribe_douyin(invalid, invalid).is_err());
            }
        }
        let store = Store::open(path.to_str().unwrap()).unwrap();
        assert_eq!(
            store.douyin_subscriptions().unwrap(),
            vec![(profile.into(), "https://v.douyin.com/card1/".into())]
        );
    }
}
