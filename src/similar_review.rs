use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimilarReviewImage {
    pub image_id: String,
    pub r2_key: String,
    pub label: String,
}

impl SimilarReviewImage {
    pub fn work_id(&self) -> &str {
        self.image_id
            .rsplit_once('#')
            .map_or(self.image_id.as_str(), |(work_id, _)| work_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimilarReviewGroup {
    pub group_key: String,
    pub images: Vec<SimilarReviewImage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimilarReviewPost {
    pub work_id: String,
    pub image_indices: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkPruneRequest {
    pub keep_work_id: String,
    pub remove_work_ids: Vec<String>,
}

pub fn work_prune_plan(
    group: &SimilarReviewGroup,
    keep_post_index: usize,
) -> Option<WorkPruneRequest> {
    let posts = group.posts();
    let keep_post = posts.get(keep_post_index.checked_sub(1)?)?;
    let mut remove_work_ids = Vec::new();
    for post in &posts {
        if post.work_id == keep_post.work_id {
            continue;
        }
        if !remove_work_ids.contains(&post.work_id) {
            remove_work_ids.push(post.work_id.clone());
        }
    }
    if remove_work_ids.is_empty() || remove_work_ids.len() > 20 {
        return None;
    }
    Some(WorkPruneRequest {
        keep_work_id: keep_post.work_id.clone(),
        remove_work_ids,
    })
}

impl SimilarReviewGroup {
    pub fn posts(&self) -> Vec<SimilarReviewPost> {
        let mut posts: Vec<SimilarReviewPost> = Vec::new();
        for (index, image) in self.images.iter().enumerate() {
            let work_id = image.work_id();
            if let Some(post) = posts.iter_mut().find(|post| post.work_id == work_id) {
                post.image_indices.push(index);
            } else {
                posts.push(SimilarReviewPost {
                    work_id: work_id.into(),
                    image_indices: vec![index],
                });
            }
        }
        posts
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimilarDecision {
    KeepAll,
    SelectKeep(usize),
    ConfirmKeep(usize),
    Cancel,
}

impl SimilarDecision {
    fn persisted(self) -> String {
        match self {
            Self::KeepAll => "keep_all".into(),
            Self::ConfirmKeep(index) => format!("keep:{index}"),
            Self::SelectKeep(index) => format!("select:{index}"),
            Self::Cancel => "cancel".into(),
        }
    }
}

pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS similar_reviews(
            token              INTEGER PRIMARY KEY AUTOINCREMENT,
            group_key          TEXT NOT NULL UNIQUE,
            payload_json       TEXT NOT NULL,
            state              TEXT NOT NULL DEFAULT 'pending',
            decision           TEXT,
            media_message_ids  TEXT NOT NULL DEFAULT '[]',
            control_message_id INTEGER,
            created_at         INTEGER NOT NULL,
            decided_at         INTEGER
         );
         CREATE INDEX IF NOT EXISTS idx_similar_reviews_state
           ON similar_reviews(state, created_at);",
    )?;
    conn.execute(
        "UPDATE similar_reviews SET state='pending',decision=NULL WHERE state='processing'",
        [],
    )?;
    Ok(())
}

pub fn register_review(conn: &Connection, group: &SimilarReviewGroup) -> Result<i64> {
    if group.images.len() < 2 {
        bail!("相似图审批至少需要两张图片");
    }
    let payload = serde_json::to_string(group)?;
    let now = now_secs();
    conn.execute(
        "INSERT INTO similar_reviews(group_key,payload_json,created_at)
         VALUES(?1,?2,?3)
         ON CONFLICT(group_key) DO UPDATE SET payload_json=excluded.payload_json",
        params![group.group_key, payload, now],
    )?;
    Ok(conn.query_row(
        "SELECT token FROM similar_reviews WHERE group_key=?1",
        params![group.group_key],
        |row| row.get(0),
    )?)
}

pub fn set_review_messages(
    conn: &Connection,
    token: i64,
    media_message_ids: &[i32],
    control_message_id: i32,
) -> Result<()> {
    conn.execute(
        "UPDATE similar_reviews SET media_message_ids=?2,control_message_id=?3 WHERE token=?1",
        params![
            token,
            serde_json::to_string(media_message_ids)?,
            control_message_id
        ],
    )?;
    Ok(())
}

pub fn review_messages(conn: &Connection, token: i64) -> Result<Option<(Vec<i32>, Option<i32>)>> {
    conn.query_row(
        "SELECT media_message_ids,control_message_id FROM similar_reviews WHERE token=?1",
        params![token],
        |row| Ok((row.get::<_, String>(0)?, row.get(1)?)),
    )
    .optional()?
    .map(|(ids, control)| Ok((serde_json::from_str(&ids)?, control)))
    .transpose()
}

pub fn load_review(conn: &Connection, token: i64) -> Result<Option<SimilarReviewGroup>> {
    conn.query_row(
        "SELECT payload_json FROM similar_reviews WHERE token=?1 AND state='pending'",
        params![token],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .map(|payload| Ok(serde_json::from_str(&payload)?))
    .transpose()
}

pub fn claim_review(
    conn: &Connection,
    token: i64,
    decision: SimilarDecision,
) -> Result<Option<SimilarReviewGroup>> {
    let keep_index = match decision {
        SimilarDecision::KeepAll => None,
        SimilarDecision::ConfirmKeep(index) => Some(index),
        _ => return Ok(None),
    };
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload_json FROM similar_reviews WHERE token=?1 AND state='pending'",
            params![token],
            |row| row.get(0),
        )
        .optional()?;
    let Some(payload) = payload else {
        return Ok(None);
    };
    let group: SimilarReviewGroup = serde_json::from_str(&payload)?;
    if let Some(index) = keep_index {
        if index == 0 || index > group.posts().len() {
            return Ok(None);
        }
    }
    let changed = conn.execute(
        "UPDATE similar_reviews SET state='processing',decision=?2
         WHERE token=?1 AND state='pending'",
        params![token, decision.persisted()],
    )?;
    Ok((changed == 1).then_some(group))
}

pub fn finish_review(conn: &Connection, token: i64, decision: SimilarDecision) -> Result<()> {
    conn.execute(
        "UPDATE similar_reviews
         SET state='decided',decision=?2,decided_at=?3
         WHERE token=?1 AND state='processing'",
        params![token, decision.persisted(), now_secs()],
    )?;
    Ok(())
}

pub fn restore_review(conn: &Connection, token: i64) -> Result<()> {
    conn.execute(
        "UPDATE similar_reviews SET state='pending',decision=NULL WHERE token=?1 AND state='processing'",
        params![token],
    )?;
    Ok(())
}

pub fn parse_callback(value: &str) -> Option<(i64, SimilarDecision)> {
    let mut parts = value.split(':');
    if parts.next()? != "similar" {
        return None;
    }
    let token = parts.next()?.parse().ok()?;
    let action = parts.next()?;
    let decision = match action {
        "all" if parts.next().is_none() => SimilarDecision::KeepAll,
        "keep" => SimilarDecision::SelectKeep(parts.next()?.parse().ok()?),
        "confirm" => SimilarDecision::ConfirmKeep(parts.next()?.parse().ok()?),
        "cancel" if parts.next().is_none() => SimilarDecision::Cancel,
        _ => return None,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((token, decision))
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}
