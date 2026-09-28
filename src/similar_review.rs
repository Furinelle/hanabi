//! Compatibility storage for retiring historical Telegram image reviews.
use anyhow::Result;
use rusqlite::{params, Connection};

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
            candidate_published INTEGER NOT NULL DEFAULT 0,
            created_at         INTEGER NOT NULL,
            decided_at         INTEGER
         );
         CREATE INDEX IF NOT EXISTS idx_similar_reviews_state
           ON similar_reviews(state, created_at);",
    )?;
    conn.execute_batch(
        "UPDATE similar_reviews
         SET state=CASE
               WHEN decision LIKE 'manual:keep:%' THEN 'manual_pending'
               ELSE 'pending'
             END,
             decision=CASE
               WHEN decision LIKE 'manual:keep:%' THEN decision
               ELSE NULL
             END
         WHERE state='processing';",
    )?;
    let _ = conn.execute(
        "ALTER TABLE similar_reviews ADD COLUMN candidate_published INTEGER NOT NULL DEFAULT 0",
        [],
    );
    Ok(())
}

pub fn remove_pending_candidate_reviews(conn: &Connection, pending_token: i64) -> Result<usize> {
    let rows: Vec<(i64, String)> = {
        let mut stmt =
            conn.prepare("SELECT token,payload_json FROM similar_reviews WHERE state='pending'")?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        rows
    };
    let tokens: Vec<i64> = rows
        .into_iter()
        .filter_map(|(token, payload)| {
            serde_json::from_str::<serde_json::Value>(&payload)
                .ok()
                .and_then(|group| {
                    (group["pending_candidate"]["pending_token"].as_i64()? == pending_token)
                        .then_some(token)
                })
        })
        .collect();
    let mut removed = 0;
    for token in tokens {
        removed += conn.execute(
            "DELETE FROM similar_reviews WHERE token=?1 AND state='pending'",
            params![token],
        )?;
    }
    Ok(removed)
}
