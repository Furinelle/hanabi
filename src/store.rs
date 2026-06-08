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
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS pushed (
                 source_kind TEXT NOT NULL,
                 source_id   TEXT NOT NULL,
                 pushed_at   INTEGER NOT NULL,
                 PRIMARY KEY (source_kind, source_id)
             );",
        )?;
        Ok(())
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
            }],
            origin: "s".into(),
        }
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
}
