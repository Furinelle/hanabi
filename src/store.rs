use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::model::MediaItem;

pub struct Store {
    conn: Connection,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DouyinSubscription {
    pub id: i64,
    pub profile_url: String,
    pub shared_url: String,
    pub display_name: String,
}

fn douyin_monitor_target(raw: &str) -> Option<String> {
    if let Some(profile) = crate::source::douyin::canonical_user_profile(raw) {
        return Some(profile);
    }
    let url = reqwest::Url::parse(raw).ok()?;
    let parts: Vec<_> = url
        .path_segments()?
        .filter(|part| !part.is_empty())
        .collect();
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str() != Some("v.douyin.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || parts.len() != 1
        || !parts[0]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return None;
    }
    Some(format!("https://v.douyin.com/{}/", parts[0]))
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
                 shared_url TEXT NOT NULL,
                 enabled INTEGER NOT NULL DEFAULT 1,
                 display_name TEXT NOT NULL DEFAULT ''
             );
             CREATE TABLE IF NOT EXISTS douyin_shared_works (
                 source_id TEXT PRIMARY KEY,
                 profile_url TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS douyin_shared_works_author
                 ON douyin_shared_works(profile_url);",
        )?;
        let migration = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        let columns = migration
            .prepare("PRAGMA table_info(douyin_subscriptions)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (name, definition) in [
            ("enabled", "INTEGER NOT NULL DEFAULT 1"),
            ("display_name", "TEXT NOT NULL DEFAULT ''"),
        ] {
            if !columns.iter().any(|column| column == name) {
                migration.execute_batch(&format!(
                    "ALTER TABLE douyin_subscriptions ADD COLUMN {name} {definition}"
                ))?;
            }
        }
        migration.commit()?;
        Ok(())
    }

    pub fn subscribe_douyin(&self, profile_url: &str, shared_url: &str) -> Result<bool> {
        let profile_url = crate::source::douyin::canonical_user_profile(profile_url)
            .ok_or_else(|| anyhow::anyhow!("不是有效的抖音作者主页"))?;
        self.register_douyin(&profile_url, shared_url, "", true)
    }

    /// Config reloads must not undo an owner's explicit removal.
    pub fn register_configured_douyin(
        &self,
        target: &str,
        shared_url: &str,
        display_name: &str,
    ) -> Result<()> {
        self.register_douyin(target, shared_url, display_name, false)?;
        Ok(())
    }

    fn register_douyin(
        &self,
        target: &str,
        shared_url: &str,
        display_name: &str,
        restore: bool,
    ) -> Result<bool> {
        let target = douyin_monitor_target(target)
            .ok_or_else(|| anyhow::anyhow!("不是有效的抖音作者主页或短链"))?;
        let shared_key = douyin_monitor_target(shared_url).unwrap_or_else(|| shared_url.into());
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let candidates = {
            let mut statement = transaction.prepare(
                "SELECT rowid, profile_url, shared_url, display_name, enabled
                 FROM douyin_subscriptions
                 WHERE profile_url IN (?1, ?2) OR shared_url IN (?1, ?2, ?3)
                    OR profile_url IN (SELECT shared_url FROM douyin_subscriptions
                                       WHERE profile_url IN (?1, ?2))
                 ORDER BY rowid",
            )?;
            let rows = statement.query_map(params![target, shared_key, shared_url], |row| {
                Ok((
                    DouyinSubscription {
                        id: row.get(0)?,
                        profile_url: row.get(1)?,
                        shared_url: row.get(2)?,
                        display_name: row.get(3)?,
                    },
                    row.get::<_, bool>(4)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let is_profile = crate::source::douyin::canonical_user_profile(&target).is_some();
        let primary = candidates
            .iter()
            .find(|(row, _)| row.profile_url == target && is_profile)
            .or_else(|| {
                candidates.iter().find(|(row, _)| {
                    !is_profile
                        && crate::source::douyin::canonical_user_profile(&row.profile_url).is_some()
                })
            })
            .or_else(|| {
                candidates.iter().find(|(row, _)| {
                    crate::source::douyin::canonical_user_profile(&row.profile_url).is_none()
                        && (row.profile_url == target || row.profile_url == shared_key)
                })
            });
        let created_or_restored;
        if let Some((primary, enabled)) = primary {
            let profile = if is_profile {
                &target
            } else {
                &primary.profile_url
            };
            let aliases: Vec<_> = candidates
                .iter()
                .filter(|(row, _)| {
                    row.id != primary.id
                        && crate::source::douyin::canonical_user_profile(&row.profile_url).is_none()
                })
                .collect();
            // Retired short-link rows point to the surviving profile, preserving callback IDs.
            let disabled = !enabled
                || aliases
                    .iter()
                    .any(|(row, enabled)| !enabled && row.shared_url != *profile);
            let name = if primary.display_name.is_empty() {
                aliases
                    .iter()
                    .find_map(|(row, _)| {
                        (!row.display_name.is_empty()).then_some(row.display_name.as_str())
                    })
                    .unwrap_or(display_name)
            } else {
                primary.display_name.as_str()
            };
            let shared = aliases
                .first()
                .map(|(alias, _)| alias.profile_url.as_str())
                .unwrap_or(&primary.shared_url);
            transaction.execute(
                "UPDATE douyin_subscriptions SET profile_url = ?1, enabled = ?2,
                     display_name = ?3, shared_url = ?4 WHERE rowid = ?5",
                params![profile, restore || !disabled, name, shared, primary.id],
            )?;
            for (alias, _) in aliases {
                transaction.execute(
                    "UPDATE douyin_subscriptions SET enabled = 0, shared_url = ?1 WHERE rowid = ?2",
                    params![profile, alias.id],
                )?;
            }
            created_or_restored = restore && disabled;
        } else {
            transaction.execute(
                "INSERT INTO douyin_subscriptions (profile_url, shared_url, display_name)
                 VALUES (?1, ?2, ?3)",
                params![target, shared_url, display_name],
            )?;
            created_or_restored = true;
        }
        transaction.commit()?;
        Ok(created_or_restored)
    }

    pub fn list_douyin_monitors(&self) -> Result<Vec<DouyinSubscription>> {
        let mut statement = self.conn.prepare(
            "SELECT rowid, profile_url, shared_url, display_name
             FROM douyin_subscriptions WHERE enabled = 1 ORDER BY rowid",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(DouyinSubscription {
                id: row.get(0)?,
                profile_url: row.get(1)?,
                shared_url: row.get(2)?,
                display_name: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn unsubscribe_douyin(&self, id: i64) -> Result<bool> {
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let alias: Option<(String, String)> = transaction
            .query_row(
                "SELECT profile_url, shared_url FROM douyin_subscriptions WHERE rowid = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((profile, shared)) = alias {
            if crate::source::douyin::canonical_user_profile(&profile).is_none()
                && crate::source::douyin::canonical_user_profile(&shared).is_some()
            {
                let removed = transaction.execute(
                    "UPDATE douyin_subscriptions SET enabled = 0 WHERE profile_url = ?1 AND enabled = 1",
                    params![shared],
                )? > 0;
                transaction.commit()?;
                return Ok(removed);
            }
        }
        let removed = transaction.execute(
            "UPDATE douyin_subscriptions SET enabled = 0 WHERE rowid = ?1 AND enabled = 1",
            params![id],
        )? > 0;
        transaction.commit()?;
        Ok(removed)
    }

    pub fn remember_douyin_author(&self, profile_url: &str, display_name: &str) -> Result<()> {
        let Some(profile) = crate::source::douyin::canonical_user_profile(profile_url) else {
            return Ok(());
        };
        if !display_name.trim().is_empty() {
            self.conn.execute(
                "UPDATE douyin_subscriptions SET display_name = ?1 WHERE profile_url = ?2",
                params![display_name, profile],
            )?;
        }
        Ok(())
    }

    pub fn is_douyin_disabled(&self, target: &str) -> Result<bool> {
        let key = douyin_monitor_target(target).unwrap_or_else(|| target.into());
        let row: Option<(String, String, bool)> = self
            .conn
            .query_row(
                "SELECT profile_url, shared_url, enabled FROM douyin_subscriptions
             WHERE profile_url = ?1 OR shared_url = ?1 OR shared_url = ?2
             ORDER BY (profile_url = ?1) DESC, rowid LIMIT 1",
                params![key, target],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        match row {
            Some((profile, shared, false))
                if crate::source::douyin::canonical_user_profile(&profile).is_none()
                    && crate::source::douyin::canonical_user_profile(&shared).is_some() =>
            {
                Ok(self
                    .conn
                    .query_row(
                        "SELECT enabled = 0 FROM douyin_subscriptions WHERE profile_url = ?1",
                        params![shared],
                        |row| row.get(0),
                    )
                    .optional()?
                    .unwrap_or(true))
            }
            Some((_, _, enabled)) => Ok(!enabled),
            None => Ok(false),
        }
    }

    pub fn douyin_subscriptions(&self) -> Result<Vec<(String, String)>> {
        let mut statement = self
            .conn
            .prepare("SELECT profile_url, shared_url FROM douyin_subscriptions WHERE enabled = 1 ORDER BY rowid")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Only owner-submitted work links call this; fetched author feeds do not count.
    /// Returns true exactly when the tenth distinct work creates a subscription.
    pub fn record_manual_douyin_work(&self, profile_url: &str, source_id: &str) -> Result<bool> {
        let profile_url = crate::source::douyin::canonical_user_profile(profile_url)
            .ok_or_else(|| anyhow::anyhow!("不是有效的抖音作者主页"))?;
        anyhow::ensure!(
            !source_id.is_empty() && source_id.bytes().all(|b| b.is_ascii_digit()),
            "不是有效的抖音作品 ID"
        );
        let transaction = self.conn.unchecked_transaction()?;
        transaction.execute(
            "INSERT OR IGNORE INTO douyin_shared_works (source_id, profile_url) VALUES (?1, ?2)",
            params![source_id, profile_url],
        )?;
        let subscribed = transaction.execute(
            "INSERT OR IGNORE INTO douyin_subscriptions (profile_url, shared_url)
             SELECT ?1, ?1 WHERE
                 (SELECT COUNT(*) FROM douyin_shared_works WHERE profile_url = ?1) >= 10",
            params![profile_url],
        )? > 0;
        transaction.commit()?;
        Ok(subscribed)
    }

    pub fn already_pushed(&self, item: &MediaItem) -> Result<bool> {
        let (kind, id) = item.dedup_key();
        self.already_pushed_key(&kind, &id)
    }

    pub fn already_pushed_key(&self, kind: &str, id: &str) -> Result<bool> {
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

    #[test]
    fn ten_distinct_manual_works_subscribe_and_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.db");
        let profile = "https://www.douyin.com/user/MS4wTEST";
        {
            let store = Store::open(path.to_str().unwrap()).unwrap();
            for n in 1..10 {
                assert!(!store
                    .record_manual_douyin_work(profile, &n.to_string())
                    .unwrap());
            }
            // Feed/published dedup is independent from links submitted by the owner.
            let mut published = item("10");
            published.source = SourceKind::Douyin;
            store.mark_pushed(&published).unwrap();
            assert!(store.douyin_subscriptions().unwrap().is_empty());
        }
        let store = Store::open(path.to_str().unwrap()).unwrap();
        assert!(!store
            .record_manual_douyin_work("https://www.iesdouyin.com/share/user/MS4wTEST/?s=1", "9")
            .unwrap());
        assert!(!store
            .record_manual_douyin_work("https://www.douyin.com/user/MS4wOTHER", "1")
            .unwrap());
        assert!(store.record_manual_douyin_work(profile, "10").unwrap());
        assert!(!store.record_manual_douyin_work(profile, "10").unwrap());
        assert!(!store.record_manual_douyin_work(profile, "11").unwrap());
        assert_eq!(
            store.douyin_subscriptions().unwrap(),
            vec![(profile.into(), profile.into())]
        );
        for (author, id) in [
            ("https://evil.example/user/MS4wTEST", "12"),
            (profile, "../12"),
        ] {
            assert!(store.record_manual_douyin_work(author, id).is_err());
        }
    }

    #[test]
    fn old_subscription_schema_migrates_without_changing_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        let profile = "https://www.douyin.com/user/MS4wOLD";
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE douyin_subscriptions (
                    profile_url TEXT PRIMARY KEY, shared_url TEXT NOT NULL);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO douyin_subscriptions (rowid, profile_url, shared_url) VALUES (42, ?1, ?1)",
                params![profile],
            ).unwrap();
        }
        for _ in 0..2 {
            let store = Store::open(path.to_str().unwrap()).unwrap();
            assert_eq!(
                store.list_douyin_monitors().unwrap(),
                vec![DouyinSubscription {
                    id: 42,
                    profile_url: profile.into(),
                    shared_url: profile.into(),
                    display_name: String::new(),
                }]
            );
        }
    }

    #[test]
    fn removal_preserves_manual_count_and_only_explicit_subscribe_restores() {
        let store = Store::open_in_memory().unwrap();
        let profile = "https://www.douyin.com/user/MS4wTEST";
        for n in 1..=10 {
            assert_eq!(
                store
                    .record_manual_douyin_work(profile, &n.to_string())
                    .unwrap(),
                n == 10
            );
        }
        let id = store.list_douyin_monitors().unwrap()[0].id;
        store.remember_douyin_author(profile, "测试作者").unwrap();
        assert!(store.unsubscribe_douyin(id).unwrap());
        assert!(!store.unsubscribe_douyin(id).unwrap());
        assert!(store
            .is_douyin_disabled("https://www.iesdouyin.com/share/user/MS4wTEST/?s=1")
            .unwrap());
        assert!(!store.record_manual_douyin_work(profile, "11").unwrap());
        store
            .register_configured_douyin(profile, profile, "测试作者")
            .unwrap();
        store.remember_douyin_author(profile, "更新名称").unwrap();
        store.remember_douyin_author("unknown", "忽略").unwrap();
        assert!(store.list_douyin_monitors().unwrap().is_empty());
        assert!(store.douyin_subscriptions().unwrap().is_empty());
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM douyin_shared_works", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 11);
        assert!(store.subscribe_douyin(profile, profile).unwrap());
        assert!(!store.subscribe_douyin(profile, profile).unwrap());
        let monitors = store.list_douyin_monitors().unwrap();
        assert_eq!(monitors[0].id, id);
        assert_eq!(monitors[0].display_name, "更新名称");
        assert!(!store.is_douyin_disabled(profile).unwrap());
        store
            .remember_douyin_author("https://www.douyin.com/user/MS4wOTHER", "不自动订阅")
            .unwrap();
        assert_eq!(store.list_douyin_monitors().unwrap().len(), 1);
    }

    #[test]
    fn configured_short_link_resolves_in_place_and_removal_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("configured.db");
        let short = "https://v.douyin.com/card1/";
        let profile = "https://www.douyin.com/user/MS4wTEST";
        let id;
        {
            let store = Store::open(path.to_str().unwrap()).unwrap();
            store
                .register_configured_douyin(short, short, "配置作者")
                .unwrap();
            id = store.list_douyin_monitors().unwrap()[0].id;
            assert!(store.unsubscribe_douyin(id).unwrap());
        }
        let store = Store::open(path.to_str().unwrap()).unwrap();
        store
            .register_configured_douyin(short, short, "配置作者")
            .unwrap();
        assert!(store
            .is_douyin_disabled("http://v.douyin.com/card1/?share=1")
            .unwrap());
        store
            .register_configured_douyin(profile, short, "配置作者")
            .unwrap();
        assert!(store.is_douyin_disabled(profile).unwrap());
        assert!(store.is_douyin_disabled(short).unwrap());
        assert!(store.subscribe_douyin(profile, short).unwrap());
        assert_eq!(store.list_douyin_monitors().unwrap()[0].id, id);
        store
            .register_configured_douyin(short, short, "配置作者")
            .unwrap();
        assert_eq!(store.list_douyin_monitors().unwrap().len(), 1);
        assert!(!store.is_douyin_disabled(short).unwrap());
        for invalid in [
            "https://evil.example/card/",
            "https://v.douyin.com/",
            "https://www.douyin.com/note/123",
        ] {
            assert!(store
                .register_configured_douyin(invalid, invalid, "")
                .is_err());
        }
    }

    #[test]
    fn canonical_and_short_alias_merge_preserves_removal_and_old_callbacks() {
        let store = Store::open_in_memory().unwrap();
        let short = "https://v.douyin.com/card1/";
        let profile = "https://www.douyin.com/user/MS4wTEST";
        store
            .register_configured_douyin(short, short, "短链作者")
            .unwrap();
        let old_id = store.list_douyin_monitors().unwrap()[0].id;
        assert!(store.unsubscribe_douyin(old_id).unwrap());
        store.subscribe_douyin(profile, profile).unwrap();
        let canonical_id = store.list_douyin_monitors().unwrap()[0].id;
        store
            .register_configured_douyin(profile, short, "")
            .unwrap();
        assert!(store.list_douyin_monitors().unwrap().is_empty());
        assert!(store.is_douyin_disabled(profile).unwrap());
        assert!(store.is_douyin_disabled(short).unwrap());
        assert!(store.subscribe_douyin(profile, short).unwrap());
        store
            .register_configured_douyin(profile, short, "")
            .unwrap();
        store.register_configured_douyin(short, short, "").unwrap();
        let monitors = store.list_douyin_monitors().unwrap();
        assert_eq!(monitors.len(), 1);
        assert_eq!(monitors[0].id, canonical_id);
        assert_eq!(monitors[0].display_name, "短链作者");
        assert_eq!(monitors[0].shared_url, short);
        assert!(!store.is_douyin_disabled(short).unwrap());
        assert!(store.unsubscribe_douyin(old_id).unwrap());
        assert!(store.is_douyin_disabled(profile).unwrap());
        let rows: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM douyin_subscriptions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 2);
        store
            .subscribe_douyin("https://www.douyin.com/user/MS4wOTHER", profile)
            .unwrap();
        assert!(store.list_douyin_monitors().unwrap()[0].id > canonical_id);
    }
}
