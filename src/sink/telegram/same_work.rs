//! 重复作品排除:不论来源,除画质外完全相同的作品只保留画质较高的一份。
//! 图库(Vitrine)仍负责入库后的图片级去重;这里只在送审前按整件作品比对,
//! 页数相同且逐页同图才算重复,拿不准的一律照常送审。
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::gallery::GalleryPublication;
use crate::image_dedup::ImageFingerprint;
use crate::model::MediaItem;

/// 同一作品在各平台的发布时间一般相差不到几天;超过窗口的旧作品不再比对。
const WINDOW_SECS: i64 = 30 * 24 * 3600;
/// 缩略图逐通道容差。实测重压缩、缩放后最大差 1–2,改动 0.3% 画面的差分 ≥ 25。
const THUMB_TOLERANCE: u8 = 8;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PagePrint {
    thumb: Vec<u8>,
    width: u32,
    height: u32,
    bytes: u64,
}

impl From<&ImageFingerprint> for PagePrint {
    fn from(fp: &ImageFingerprint) -> Self {
        Self {
            thumb: fp.thumb.clone(),
            width: fp.width,
            height: fp.height,
            bytes: fp.bytes,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verdict {
    /// 已有的那份画质不低于新抓到的:新作品不必再处理。
    KeepOld,
    /// 新抓到的画质更高且页数相同:替换已有的那份。
    ReplaceOld,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum TwinStatus {
    Pending(i64),
    Published {
        publication: GalleryPublication,
        archived: bool,
    },
    /// 审批过但没有发布(丢弃、排除作者)。
    Discarded,
}

#[derive(Debug)]
pub(super) struct Twin {
    pub source_kind: String,
    pub source_id: String,
    pub verdict: Verdict,
    pub status: TwinStatus,
}

fn same_page(a: &PagePrint, b: &PagePrint) -> bool {
    // 交叉相乘比较宽高比,容差 1%。
    let (left, right) = (
        u64::from(a.width) * u64::from(b.height),
        u64::from(b.width) * u64::from(a.height),
    );
    left.abs_diff(right) * 100 <= left.max(right)
        && !a.thumb.is_empty()
        && a.thumb.len() == b.thumb.len()
        && a.thumb
            .iter()
            .zip(&b.thumb)
            .all(|(x, y)| x.abs_diff(*y) <= THUMB_TOLERANCE)
}

/// 页数相同、每一页都在另一份里有各自对应的页才算重复。画质按像素数、再按
/// 字节数比较,与图库去重的取舍一致;持平时保留先到的。
pub(super) fn compare(new: &[PagePrint], old: &[PagePrint]) -> Option<Verdict> {
    if new.is_empty() || new.len() != old.len() {
        return None;
    }
    let quality = |page: &PagePrint| (u64::from(page.width) * u64::from(page.height), page.bytes);
    let mut used = vec![false; old.len()];
    let (mut new_quality, mut old_quality) = ((0, 0), (0, 0));
    for page in new {
        let index = (0..old.len()).find(|&i| !used[i] && same_page(page, &old[i]))?;
        used[index] = true;
        let (new_page, old_page) = (quality(page), quality(&old[index]));
        new_quality = (new_quality.0 + new_page.0, new_quality.1 + new_page.1);
        old_quality = (old_quality.0 + old_page.0, old_quality.1 + old_page.1);
    }
    Some(if new_quality <= old_quality {
        Verdict::KeepOld
    } else {
        Verdict::ReplaceOld
    })
}

pub(super) fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS work_prints(
            source_kind TEXT NOT NULL,
            source_id   TEXT NOT NULL,
            pages       TEXT NOT NULL,
            publication TEXT NOT NULL DEFAULT '',
            archived    INTEGER NOT NULL DEFAULT 0,
            created_at  INTEGER NOT NULL,
            PRIMARY KEY (source_kind, source_id)
         );",
    )
}

/// 登记一件作品的指纹。`publication` 为 None 表示刚送审,尚未发布。
pub(super) fn record(
    conn: &Connection,
    item: &MediaItem,
    pages: &[PagePrint],
    publication: Option<&GalleryPublication>,
    archived: bool,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO work_prints(source_kind, source_id, pages, publication, archived, created_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            item.source.as_str(),
            item.source_id,
            serde_json::to_string(pages)?,
            publication.map(serde_json::to_string).transpose()?.unwrap_or_default(),
            archived,
            super::now_secs(),
        ],
    )?;
    Ok(())
}

/// 审批结束:发布则记下频道消息,丢弃则清空(留着指纹,重复的作品不再送审)。
pub(super) fn set_outcome(
    conn: &Connection,
    item: &MediaItem,
    publication: Option<&GalleryPublication>,
    archived: bool,
) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE work_prints SET publication=?3, archived=?4 WHERE source_kind=?1 AND source_id=?2",
        params![
            item.source.as_str(),
            item.source_id,
            publication
                .map(serde_json::to_string)
                .transpose()?
                .unwrap_or_default(),
            archived,
        ],
    )?;
    Ok(())
}

pub(super) fn forget(
    conn: &Connection,
    source_kind: &str,
    source_id: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM work_prints WHERE source_kind=?1 AND source_id=?2",
        params![source_kind, source_id],
    )
}

/// 待审记录被整条清掉(过期、被高画质版本顶替)时调用,须在删 pending 行之前。
pub(super) fn forget_pending(conn: &Connection, token: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM work_prints WHERE (source_kind, source_id) IN (
            SELECT json_extract(item_meta, '$.source'), json_extract(item_meta, '$.source_id')
            FROM pending WHERE token=?1)",
        [token],
    )
}

pub(super) fn prune(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM work_prints WHERE created_at < ?1",
        [super::now_secs() - WINDOW_SECS],
    )
}

/// 在近期的其他作品里找重复。同时存在多份时,优先返回让新作品让位的那份。
pub(super) fn find_twin(
    conn: &Connection,
    item: &MediaItem,
    pages: &[PagePrint],
) -> anyhow::Result<Option<Twin>> {
    let mut statement = conn.prepare(
        "SELECT source_kind, source_id, pages, publication, archived FROM work_prints
         WHERE created_at >= ?1 AND json_array_length(pages) = ?2
           AND NOT (source_kind = ?3 AND source_id = ?4)
         ORDER BY created_at DESC, source_kind, source_id",
    )?;
    let rows = statement.query_map(
        params![
            super::now_secs() - WINDOW_SECS,
            pages.len(),
            item.source.as_str(),
            item.source_id
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, bool>(4)?,
            ))
        },
    )?;
    let mut replaceable = None;
    for row in rows {
        let (source_kind, source_id, old_pages, publication, archived) = row?;
        let Ok(old_pages) = serde_json::from_str::<Vec<PagePrint>>(&old_pages) else {
            continue;
        };
        let Some(verdict) = compare(pages, &old_pages) else {
            continue;
        };
        if verdict == Verdict::ReplaceOld && replaceable.is_some() {
            continue;
        }
        let token: Option<i64> = conn
            .query_row(
                "SELECT token FROM pending
                 WHERE json_extract(item_meta, '$.source')=?1
                   AND json_extract(item_meta, '$.source_id')=?2",
                params![source_kind, source_id],
                |row| row.get(0),
            )
            .optional()?;
        let status = match (
            token,
            serde_json::from_str::<GalleryPublication>(&publication),
        ) {
            (Some(token), _) => TwinStatus::Pending(token),
            (None, Ok(publication)) => TwinStatus::Published {
                publication,
                archived,
            },
            (None, Err(_)) => TwinStatus::Discarded,
        };
        let twin = Twin {
            source_kind,
            source_id,
            verdict,
            status,
        };
        if verdict == Verdict::KeepOld {
            return Ok(Some(twin));
        }
        replaceable = Some(twin);
    }
    Ok(replaceable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gallery::GalleryPublishState;
    use crate::model::{Author, SourceKind};

    fn page(seed: u8, width: u32, height: u32, bytes: u64) -> PagePrint {
        PagePrint {
            thumb: (0..768u32).map(|i| (i as u8).wrapping_mul(seed)).collect(),
            width,
            height,
            bytes,
        }
    }

    fn one(page: &PagePrint) -> &[PagePrint] {
        std::slice::from_ref(page)
    }

    fn item(source: SourceKind, id: &str) -> MediaItem {
        MediaItem {
            source,
            source_id: id.into(),
            author: Author {
                name: "画师".into(),
                url: "u".into(),
            },
            title: None,
            url: "w".into(),
            tags: vec![],
            bookmark_count: None,
            is_r18: false,
            pixiv_type: None,
            page_count: 1,
            images: vec![],
            origin: "test".into(),
        }
    }

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE pending(token INTEGER PRIMARY KEY, item_meta TEXT NOT NULL);",
        )
        .unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn recompressed_copy_matches_and_local_edit_does_not() {
        let original = page(7, 2000, 3000, 9_000_000);
        // X 版:缩小、重压缩,缩略图只有细微抖动。
        let mut smaller = page(7, 1366, 2048, 800_000);
        for value in smaller.thumb.iter_mut().step_by(5) {
            *value = value.saturating_add(2);
        }
        assert_eq!(
            compare(one(&smaller), one(&original)),
            Some(Verdict::KeepOld)
        );
        assert_eq!(
            compare(one(&original), one(&smaller)),
            Some(Verdict::ReplaceOld)
        );

        // 差分:只有几格明显不同。
        let mut variant = original.clone();
        variant.thumb[300] = variant.thumb[300].wrapping_add(40);
        assert_eq!(compare(&[variant], one(&original)), None);
        // 构图不同(宽高比差超过 1%)。
        assert_eq!(compare(&[page(7, 2000, 2900, 1)], one(&original)), None);
        // 同画质时保留先到的。
        assert_eq!(
            compare(one(&original), one(&original)),
            Some(Verdict::KeepOld)
        );
    }

    #[test]
    fn multi_page_works_need_one_to_one_pages() {
        let (a_low, b_low) = (page(3, 1000, 1000, 1), page(5, 1000, 1000, 1));
        let (a_high, b_high) = (page(3, 2000, 2000, 9), page(5, 2000, 2000, 9));
        assert_eq!(
            compare(
                &[a_high.clone(), b_high.clone()],
                &[a_low.clone(), b_low.clone()]
            ),
            Some(Verdict::ReplaceOld)
        );
        // 页数不同就不是除画质外完全相同的作品。
        assert_eq!(
            compare(one(&a_low), &[a_high.clone(), b_high.clone()]),
            None
        );
        assert_eq!(compare(&[a_high.clone(), b_high], one(&a_low)), None);
        // 两页都像旧作品的同一页,不能算覆盖了整件旧作品。
        assert_eq!(compare(&[a_high.clone(), a_high], &[a_low, b_low]), None);
    }

    #[test]
    fn twin_lookup_covers_every_source_and_tracks_review_state() {
        let conn = db();
        let low = [page(7, 1000, 1500, 1)];
        let high = [page(7, 2000, 3000, 9)];
        let x = item(SourceKind::X, "100");
        let pixiv = item(SourceKind::Pixiv, "200");
        record(&conn, &x, &low, None, false).unwrap();

        // 不分来源:同平台和抖音的重复同样算;作品不和自己比。
        for other in [item(SourceKind::X, "101"), item(SourceKind::Douyin, "1")] {
            assert_eq!(
                find_twin(&conn, &other, &high).unwrap().unwrap().source_id,
                "100"
            );
        }
        assert!(find_twin(&conn, &x, &high).unwrap().is_none());

        // 还在待审。
        conn.execute(
            "INSERT INTO pending(token, item_meta) VALUES(42, ?1)",
            [serde_json::to_string(&x).unwrap()],
        )
        .unwrap();
        let twin = find_twin(&conn, &pixiv, &high).unwrap().unwrap();
        assert_eq!(
            (twin.source_id.as_str(), twin.verdict),
            ("100", Verdict::ReplaceOld)
        );
        assert_eq!(twin.status, TwinStatus::Pending(42));

        // 已发布并入库。
        conn.execute("DELETE FROM pending", []).unwrap();
        let publication = GalleryPublication {
            chat_id: -100123,
            message_ids: vec![7, 8],
            publish_state: GalleryPublishState::Full,
        };
        set_outcome(&conn, &x, Some(&publication), true).unwrap();
        let twin = find_twin(&conn, &pixiv, &high).unwrap().unwrap();
        assert_eq!(
            twin.status,
            TwinStatus::Published {
                publication,
                archived: true
            }
        );

        // 撤销后又丢弃:指纹留着,状态变为已丢弃。
        set_outcome(&conn, &x, None, false).unwrap();
        assert_eq!(
            find_twin(&conn, &pixiv, &high).unwrap().unwrap().status,
            TwinStatus::Discarded
        );

        // 待审过期被清理后不再拦截重复的作品。
        conn.execute(
            "INSERT INTO pending(token, item_meta) VALUES(43, ?1)",
            [serde_json::to_string(&x).unwrap()],
        )
        .unwrap();
        assert_eq!(forget_pending(&conn, 43).unwrap(), 1);
        assert!(find_twin(&conn, &pixiv, &high).unwrap().is_none());

        // 超出时间窗口的记录不参与比对,并会被清理。
        record(&conn, &x, &low, None, false).unwrap();
        conn.execute(
            "UPDATE work_prints SET created_at = created_at - ?1",
            [WINDOW_SECS + 60],
        )
        .unwrap();
        assert!(find_twin(&conn, &pixiv, &high).unwrap().is_none());
        assert_eq!(prune(&conn).unwrap(), 1);
    }

    #[test]
    fn an_existing_better_copy_wins_over_a_replaceable_one() {
        let conn = db();
        record(
            &conn,
            &item(SourceKind::X, "a_low"),
            &[page(7, 1000, 1500, 1)],
            None,
            false,
        )
        .unwrap();
        record(
            &conn,
            &item(SourceKind::X, "best"),
            &[page(7, 4000, 6000, 9)],
            None,
            false,
        )
        .unwrap();
        let twin = find_twin(
            &conn,
            &item(SourceKind::Pixiv, "1"),
            &[page(7, 2000, 3000, 5)],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (twin.source_id.as_str(), twin.verdict),
            ("best", Verdict::KeepOld)
        );
    }
}
