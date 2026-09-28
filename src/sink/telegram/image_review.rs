//! Recover historical image reviews and retire them into ordinary approvals.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
struct OldImage {
    image_id: String,
    r2_key: String,
    path: PathBuf,
    message_id: i32,
    deleted: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct Session {
    token: i64,
    item: MediaItem,
    originals: Vec<PathBuf>,
    prepared: Vec<PathBuf>,
    choices: Vec<Option<bool>>,
    messages: Vec<i32>,
    #[serde(default)]
    control_message: Option<i32>,
    old: Vec<OldImage>,
}

pub(super) fn init_schema(db: &rusqlite::Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS image_review_sessions (
        token INTEGER PRIMARY KEY, payload TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'pending'
    );
    CREATE TABLE IF NOT EXISTS image_review_retire_controls (
        token INTEGER PRIMARY KEY, owner TEXT NOT NULL, owner_token INTEGER NOT NULL,
        message_id INTEGER, media INTEGER NOT NULL
    );
    UPDATE image_review_sessions SET state='pending' WHERE state='processing';",
    )?;
    Ok(())
}

pub(super) fn needs_recovery(db: &rusqlite::Connection) -> rusqlite::Result<bool> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM image_review_sessions WHERE state='deleting')",
        [],
        |r| r.get(0),
    )
}

async fn save_cards(state: &Arc<ReviewState>, session: &Session) -> Result<()> {
    let db = state.db.lock().await;
    let tx = db.unchecked_transaction()?;
    tx.execute(
        "UPDATE image_review_sessions SET payload=?2 WHERE token=?1",
        rusqlite::params![session.token, serde_json::to_string(session)?],
    )?;
    tx.execute(
        "UPDATE pending SET msg_ids=?2 WHERE token=?1",
        rusqlite::params![
            session.token,
            serde_json::to_string(&session.all_messages())?
        ],
    )?;
    tx.commit()?;
    Ok(())
}

async fn cleanup_cards(state: &Arc<ReviewState>, session: &mut Session, all: bool) -> Result<()> {
    let mut ids = Vec::new();
    if all {
        ids.extend(session.messages.iter().copied().filter(|id| *id > 0));
    }
    ids.extend(
        session
            .old
            .iter()
            .filter(|old| all || old.deleted)
            .map(|old| old.message_id)
            .filter(|id| *id > 0),
    );
    if all {
        ids.extend(session.control_message.filter(|id| *id > 0));
    }
    let mut removed = HashSet::new();
    for id in ids {
        let result = tg_retry(|| {
            state
                .bot
                .delete_message(state.review_chat.clone(), MessageId(id))
        })
        .await;
        match result {
            Ok(_) => {
                removed.insert(id);
            }
            Err(error) if error.to_string().contains("message to delete not found") => {
                removed.insert(id);
            }
            Err(error) => {
                tracing::warn!(%error, token=session.token, message_id=id, "清理相似图审批消息失败，保留消息编号供重试")
            }
        }
    }
    for id in &mut session.messages {
        if removed.contains(id) {
            *id = -1;
        }
    }
    for old in &mut session.old {
        if removed.contains(&old.message_id) {
            old.message_id = -1;
        }
    }
    if session
        .control_message
        .is_some_and(|id| removed.contains(&id))
    {
        session.control_message = Some(-1);
    }
    save_cards(state, session).await?;
    if !removed.is_empty() {
        tracing::info!(
            token = session.token,
            removed = removed.len(),
            "已清理相似图审批消息"
        );
    }
    Ok(())
}

impl Session {
    fn all_messages(&self) -> Vec<i32> {
        let ids: Vec<i32> = self
            .messages
            .iter()
            .copied()
            .chain(self.old.iter().map(|i| i.message_id).filter(|id| *id > 0))
            .chain(self.control_message)
            .filter(|id| *id > 0)
            .collect();
        let mut seen = HashSet::new();
        ids.into_iter().filter(|id| seen.insert(*id)).collect()
    }

    fn retained_row(&self) -> Result<Option<PendingRow>> {
        let indices: Vec<_> = (0..self.originals.len())
            .filter(|i| self.choices.get(*i) != Some(&Some(false)))
            .collect();
        if indices.is_empty() {
            return Ok(None);
        }
        let files: Vec<_> = indices
            .iter()
            .map(|i| self.prepared.get(*i).context("历史审批缺少预备图片"))
            .collect::<Result<_>>()?;
        let originals: Vec<_> = indices.iter().map(|i| &self.originals[*i]).collect();
        let mut item = self.item.clone();
        item.page_count = indices.len() as u32;
        item.images = indices
            .iter()
            .filter_map(|i| self.item.images.get(*i).cloned())
            .collect();
        Ok(Some((
            serde_json::to_string(&files)?,
            render_caption(&item),
            serde_json::to_string(&self.all_messages())?,
            serde_json::to_string(&originals)?,
            item.is_r18,
            serde_json::to_string(&item)?,
        )))
    }
}

fn load(db: &rusqlite::Connection, token: i64) -> Result<Option<(Session, String)>> {
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT payload,state FROM image_review_sessions WHERE token=?1",
            [token],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(payload, state)| Ok((serde_json::from_str(&payload)?, state)))
        .transpose()
}

pub(super) fn complete(
    db: &rusqlite::Connection,
    token: i64,
    publication: &str,
) -> rusqlite::Result<bool> {
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM image_review_sessions WHERE token=?1 AND state='processing')",
        [token],
        |r| r.get(0),
    )?;
    if exists {
        let changed = db.execute("UPDATE review_actions SET publication=?2 WHERE token=?1 AND action='image_review' AND state='available'", rusqlite::params![token,publication])?;
        if changed != 1 {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        db.execute(
            "UPDATE image_review_sessions SET state='decided' WHERE token=?1",
            [token],
        )?;
    }
    Ok(exists)
}

pub(super) async fn undo(state: &Arc<ReviewState>, action: &UndoAction) -> Result<()> {
    let mut session: Session = serde_json::from_str(&action.row.5)?;
    {
        let db = state.db.lock().await;
        if let Some((current, _)) = load(&db, session.token)? {
            session.messages = current.messages;
            session.control_message = current.control_message;
            for old in &mut session.old {
                if let Some(now) = current
                    .old
                    .iter()
                    .find(|now| now.image_id == old.image_id && now.r2_key == old.r2_key)
                {
                    old.message_id = now.message_id;
                }
            }
        }
    }
    if let Some(publication) = decode_undo_publication(&action.publication) {
        if let Some(first) = publication.message_ids.first() {
            state.pending_comments.lock().await.remove(first);
        }
        delete_undo_telegram_messages(&state.bot, &publication).await?;
        cancel_undone_gallery_outbox(state, &session.item);
        retract_undone_gallery(state, action.id, &session.item).await?;
    }
    if session.originals.iter().any(|p| !p.is_file()) {
        anyhow::bail!("原图文件已过期");
    }
    let paths: Vec<_> = session
        .originals
        .iter()
        .enumerate()
        .filter(|(i, _)| session.choices.get(*i) != Some(&Some(false)))
        .map(|(_, path)| path.clone())
        .collect();
    let scan_paths = paths.clone();
    let fps = tokio::task::spawn_blocking(move || {
        scan_paths
            .iter()
            .map(|p| inspect_image(p))
            .collect::<Result<Vec<_>>>()
    })
    .await??;
    {
        let db = state.db.lock().await;
        let tx = db.unchecked_transaction()?;
        if let Some(row) = session.retained_row()? {
            tx.execute("INSERT OR REPLACE INTO pending(token,files,caption,msg_ids,originals,created_at,state,is_r18,item_meta) VALUES(?1,?2,?3,?4,?5,?6,'pending',?7,?8)",
                rusqlite::params![session.token,row.0,row.1,row.2,row.3,now_secs(),row.4,row.5])?;
            queue_control(&tx, session_control(&session))?;
        }
        tx.execute(
            "UPDATE image_review_sessions SET payload=?2,state='retired' WHERE token=?1",
            rusqlite::params![session.token, serde_json::to_string(&session)?],
        )?;
        if !paths.is_empty() {
            record_work(&tx, &session.item, &fps, WorkStatus::Pending)?;
        } else if !session.originals.is_empty() {
            remove_work(&tx, &session.item)?;
        }
        tx.commit()?;
    }
    retire_pending(state).await?;
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
struct Target {
    publication_id: String,
    chat_id: i64,
    message_id: i32,
}

#[derive(Clone, Serialize, Deserialize)]
struct Restored {
    publication_id: String,
    message_id: i32,
}

#[derive(Serialize, Deserialize)]
struct DeleteUndo {
    session_state: String,
    token: i64,
    index: usize,
    decision_id: String,
    r2_key: String,
    path: PathBuf,
    targets: Vec<Target>,
    #[serde(default)]
    restored: Vec<Restored>,
    fingerprint: Option<(Vec<String>, Vec<serde_json::Value>)>,
}

pub(super) async fn undo_delete(state: &Arc<ReviewState>, action: &UndoAction) -> Result<()> {
    let mut journal: DeleteUndo = serde_json::from_str(&action.row.5)?;
    let gallery = state.gallery.as_ref().context("图库未配置")?;
    let response=gallery.review_image(&serde_json::json!({"decision_id":journal.decision_id,"r2_key":journal.r2_key,"action":"prepare"})).await?;
    if response["state"] == "deleted" {
        for target in journal.targets.clone() {
            if journal
                .restored
                .iter()
                .any(|r| r.publication_id == target.publication_id)
            {
                continue;
            }
            // A failed deletion may have left the original intact. Probe it before republishing.
            let message_id = match tg_retry(|| {
                state.bot.copy_message(
                    state.review_chat.clone(),
                    ChatId(target.chat_id),
                    MessageId(target.message_id),
                )
            })
            .await
            {
                Ok(probe) => {
                    cleanup_review_messages(state, &[probe]).await;
                    target.message_id
                }
                Err(error)
                    if error
                        .to_string()
                        .to_lowercase()
                        .contains("message to copy not found") =>
                {
                    if !journal.path.is_file() {
                        anyhow::bail!("恢复原图已过期");
                    }
                    let path = journal.path.clone();
                    let prepared =
                        tokio::task::spawn_blocking(move || prepare_all(&[path])).await??;
                    let mut sent = vec![];
                    let mut publication = TelegramPublicationBuilder::default();
                    send_group(
                        &state.bot,
                        &Recipient::Id(ChatId(target.chat_id)),
                        &prepared,
                        "↩️ 恢复误删图片",
                        false,
                        &mut sent,
                        &mut publication,
                        &mut || {},
                    )
                    .await?;
                    sent.first().context("恢复图片未返回频道消息")?.0
                }
                Err(error) => return Err(error.into()),
            };
            journal.restored.push(Restored {
                publication_id: target.publication_id,
                message_id,
            });
            let db = state.db.lock().await;
            db.execute(
                "UPDATE review_actions SET item_meta=?2 WHERE id=?1 AND state='restoring'",
                rusqlite::params![action.id, serde_json::to_string(&journal)?],
            )?;
        }
    }
    // Confirm cancellation even when deletion is still prepared on the server.
    gallery.review_image(&serde_json::json!({"decision_id":journal.decision_id,"r2_key":journal.r2_key,"action":"restore","restored":journal.restored})).await?;
    {
        let db = state.db.lock().await;
        let tx = db.unchecked_transaction()?;
        let (mut session, _) = load(&tx, journal.token)?.context("审批记录已过期")?;
        session
            .old
            .get_mut(journal.index)
            .context("旧图序号失效")?
            .deleted = false;
        if let Some((columns, values)) = &journal.fingerprint {
            let params = values.iter().map(|v| match v {
                serde_json::Value::String(s) => rusqlite::types::Value::Text(s.clone()),
                serde_json::Value::Number(n) if n.is_i64() => {
                    rusqlite::types::Value::Integer(n.as_i64().unwrap())
                }
                serde_json::Value::Number(n) => rusqlite::types::Value::Real(n.as_f64().unwrap()),
                _ => rusqlite::types::Value::Null,
            });
            // Column names come exclusively from SQLite's own schema, never callback text.
            tx.execute(
                &format!(
                    "INSERT OR IGNORE INTO image_fingerprints({}) VALUES({})",
                    columns
                        .iter()
                        .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
                        .collect::<Vec<_>>()
                        .join(","),
                    vec!["?"; columns.len()].join(",")
                ),
                rusqlite::params_from_iter(params),
            )?;
        }
        tx.execute(
            "UPDATE image_review_sessions SET payload=?2,state=?3 WHERE token=?1",
            rusqlite::params![
                session.token,
                serde_json::to_string(&session)?,
                journal.session_state
            ],
        )?;
        tx.commit()?;
    };
    // The historical delete journal is still recoverable, but no image buttons return.
    if journal.session_state == "pending" {
        retire_pending(state).await?;
    }
    Ok(())
}

pub(super) async fn cleanup_unreferenced(state: &Arc<ReviewState>, paths: &[PathBuf]) {
    let Some(parent) = paths.first().and_then(|p| p.parent()) else {
        return;
    };
    let db = state.db.lock().await;
    let referenced = (|| -> rusqlite::Result<bool> {
        let mut stmt = db.prepare("SELECT files FROM pending UNION ALL SELECT files FROM review_actions WHERE state IN ('available','restoring') UNION ALL SELECT json_array(COALESCE(json_extract(payload,'$.originals[0]'),json_extract(payload,'$.old[0].path'))) FROM image_review_sessions")?;
        for row in stmt.query_map([], |row| row.get::<_,String>(0))? {
            let Ok(files) = serde_json::from_str::<Vec<PathBuf>>(&row?) else { return Ok(true); };
            if files.iter().any(|p| p.parent() == Some(parent)) { return Ok(true); }
        }
        Ok(false)
    })().unwrap_or(true);
    drop(db);
    if !referenced {
        cleanup(paths);
    }
}

pub(super) async fn collect_retired(state: &Arc<ReviewState>) {
    let result: Result<(Vec<Vec<PathBuf>>,Vec<Session>)> = async {
        let db=state.db.lock().await;
        let tx=db.unchecked_transaction()?;
        let files={
            let mut stmt=tx.prepare("SELECT files FROM review_actions WHERE state='superseded'")?;
            let rows=stmt.query_map([],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().map(|raw| serde_json::from_str(&raw)).collect::<serde_json::Result<Vec<Vec<PathBuf>>>>()?
        };
        tx.execute("DELETE FROM review_actions WHERE state='superseded'",[])?;
        let sessions={
            let mut stmt=tx.prepare("SELECT payload FROM image_review_sessions s WHERE s.state!='deleting' AND NOT EXISTS(SELECT 1 FROM pending p WHERE p.token=s.token) AND NOT EXISTS(SELECT 1 FROM review_actions a WHERE a.token=s.token AND a.state IN ('available','restoring'))")?;
            let rows=stmt.query_map([],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().map(|raw| serde_json::from_str(&raw)).collect::<serde_json::Result<Vec<Session>>>()?.into_iter().filter(|session| session.all_messages().is_empty()).collect::<Vec<_>>()
        };
        for session in &sessions { tx.execute("DELETE FROM image_review_sessions WHERE token=?1",[session.token])?; }
        tx.commit()?;
        Ok((files,sessions))
    }.await;
    match result {
        Ok((files, sessions)) => {
            for files in files {
                cleanup_unreferenced(state, &files).await;
            }
            for session in sessions {
                let messages = session
                    .all_messages()
                    .into_iter()
                    .map(MessageId)
                    .collect::<Vec<_>>();
                cleanup_review_messages(state, &messages).await;
                cleanup_unreferenced(state, &session.originals).await;
            }
        }
        Err(error) => tracing::warn!(%error,"清理已结束逐图审批失败"),
    }
}

#[derive(Clone)]
enum ControlOwner {
    Session(i64),
    Legacy(i64),
    Pending,
}

struct Control {
    token: i64,
    owner: ControlOwner,
    message_id: Option<i32>,
    media: bool,
}

fn queue_control(db: &rusqlite::Connection, control: Control) -> Result<()> {
    let (owner, owner_token) = match control.owner {
        ControlOwner::Session(token) => ("session", token),
        ControlOwner::Legacy(token) => ("legacy", token),
        ControlOwner::Pending => ("pending", control.token),
    };
    db.execute("INSERT OR IGNORE INTO image_review_retire_controls(token,owner,owner_token,message_id,media) VALUES(?1,?2,?3,?4,?5)",
        rusqlite::params![control.token, owner, owner_token, control.message_id, control.media])?;
    Ok(())
}

fn session_control(session: &Session) -> Control {
    let existing = session.control_message.filter(|id| *id > 0);
    Control {
        token: session.token,
        owner: ControlOwner::Session(session.token),
        message_id: existing.or_else(|| session.messages.iter().copied().find(|id| *id > 0)),
        media: existing.is_none(),
    }
}

fn retire_session(db: &rusqlite::Connection, session: &Session) -> Result<bool> {
    let Some(row) = session.retained_row()? else {
        db.execute(
            "DELETE FROM pending WHERE token=?1 AND state='similar'",
            [session.token],
        )?;
        db.execute(
            "UPDATE image_review_sessions SET state='decided' WHERE token=?1",
            [session.token],
        )?;
        if !session.originals.is_empty() {
            remove_work(db, &session.item)?;
        }
        return Ok(false);
    };
    let changed = db.execute(
        "UPDATE pending SET files=?2,caption=?3,msg_ids=?4,originals=?5,state='pending',is_r18=?6,item_meta=?7,created_at=?8 WHERE token=?1 AND state IN ('similar','pending')",
        rusqlite::params![session.token,row.0,row.1,row.2,row.3,row.4,row.5,now_secs()],
    )?;
    if changed == 1 {
        // Pending page indices may change after discarded pages are removed.
        // Gallery sync rebuilds fingerprints from the actual archived pages.
        db.execute(
            "DELETE FROM image_fingerprints WHERE source_kind=?1 AND source_id=?2 AND status='pending'",
            rusqlite::params![session.item.source.as_str(), session.item.source_id],
        )?;
    }
    db.execute(
        "UPDATE image_review_sessions SET state=?2 WHERE token=?1",
        rusqlite::params![
            session.token,
            if changed == 1 { "retired" } else { "decided" }
        ],
    )?;
    Ok(changed == 1)
}

async fn ensure_control(state: &Arc<ReviewState>, control: Control) -> Result<()> {
    let caption = "【待审】历史相似图已转为普通审批，请选择整个作品的处理方式。";
    let keyboard = review_keyboard(control.token, state.gallery.is_some());
    if let Some(id) = control.message_id {
        let result = if control.media {
            tg_retry(|| {
                state
                    .bot
                    .edit_message_caption(state.review_chat.clone(), MessageId(id))
                    .caption(caption)
                    .reply_markup(keyboard.clone())
            })
            .await
            .map(|_| ())
        } else {
            tg_retry(|| {
                state
                    .bot
                    .edit_message_text(state.review_chat.clone(), MessageId(id), caption)
                    .reply_markup(keyboard.clone())
            })
            .await
            .map(|_| ())
        };
        match result {
            Ok(()) => {
                let db = state.db.lock().await;
                db.execute(
                    "DELETE FROM image_review_retire_controls WHERE token=?1",
                    [control.token],
                )?;
                return Ok(());
            }
            Err(error) if error.to_string().contains("message is not modified") => {
                let db = state.db.lock().await;
                db.execute(
                    "DELETE FROM image_review_retire_controls WHERE token=?1",
                    [control.token],
                )?;
                return Ok(());
            }
            Err(error) if error.to_string().contains("message to edit not found") => {}
            Err(error) => return Err(error.into()),
        }
    }
    let sent = tg_retry(|| {
        state
            .bot
            .send_message(state.review_chat.clone(), caption)
            .reply_markup(keyboard.clone())
    })
    .await?;
    let db = state.db.lock().await;
    let tx = db.unchecked_transaction()?;
    match control.owner {
        ControlOwner::Session(token) => {
            if let Some((mut session, _)) = load(&tx, token)? {
                session.control_message = Some(sent.id.0);
                tx.execute(
                    "UPDATE image_review_sessions SET payload=?2 WHERE token=?1",
                    rusqlite::params![token, serde_json::to_string(&session)?],
                )?;
                tx.execute(
                    "UPDATE pending SET msg_ids=?2 WHERE token=?1 AND state='pending'",
                    rusqlite::params![token, serde_json::to_string(&session.all_messages())?],
                )?;
            }
        }
        ControlOwner::Legacy(review_token) => {
            tx.execute(
                "UPDATE similar_reviews SET control_message_id=?2 WHERE token=?1",
                rusqlite::params![review_token, sent.id.0],
            )?;
            append_pending_message(&tx, control.token, sent.id.0)?;
        }
        ControlOwner::Pending => append_pending_message(&tx, control.token, sent.id.0)?,
    }
    tx.execute(
        "DELETE FROM image_review_retire_controls WHERE token=?1",
        [control.token],
    )?;
    tx.commit()?;
    Ok(())
}

fn append_pending_message(db: &rusqlite::Connection, token: i64, id: i32) -> Result<()> {
    let raw: String = db.query_row("SELECT msg_ids FROM pending WHERE token=?1", [token], |r| {
        r.get(0)
    })?;
    let mut ids: Vec<i32> = serde_json::from_str(&raw)?;
    if !ids.contains(&id) {
        ids.push(id);
    }
    db.execute(
        "UPDATE pending SET msg_ids=?2 WHERE token=?1",
        rusqlite::params![token, serde_json::to_string(&ids)?],
    )?;
    Ok(())
}

pub(super) async fn retire_pending(state: &Arc<ReviewState>) -> Result<()> {
    let cleanup = {
        let db = state.db.lock().await;
        let tx = db.unchecked_transaction()?;
        let sessions: Vec<(String, String)> = {
            let mut stmt = tx.prepare("SELECT payload,state FROM image_review_sessions WHERE state IN ('pending','retired','decided')")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            rows
        };
        let mut cleanup = Vec::new();
        for (payload, status) in sessions {
            let session: Session = serde_json::from_str(&payload)?;
            if status == "pending" && retire_session(&tx, &session)? {
                queue_control(&tx, session_control(&session))?;
            } else if status == "retired" {
                let active: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM pending WHERE token=?1 AND state='pending')",
                    [session.token],
                    |r| r.get(0),
                )?;
                if !active {
                    cleanup.push(session);
                }
            } else if status == "decided" {
                cleanup.push(session);
            }
        }
        let legacy: Vec<(i64, String, String, Option<i32>)> = {
            let mut stmt = tx.prepare("SELECT token,payload_json,media_message_ids,control_message_id FROM similar_reviews WHERE state IN ('pending','manual_pending')")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<_>>()?;
            rows
        };
        for (review_token, payload, media_raw, control_id) in legacy {
            let candidate_token = serde_json::from_str::<serde_json::Value>(&payload)?
                ["pending_candidate"]["pending_token"]
                .as_i64();
            if let Some(token) = candidate_token {
                let pending: Option<(String, String)> = tx
                    .query_row(
                        "SELECT state,msg_ids FROM pending WHERE token=?1",
                        [token],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                if let Some((pending_state, raw)) =
                    pending.filter(|(s, _)| s == "similar" || s == "pending")
                {
                    let mut ids: Vec<i32> = serde_json::from_str(&raw)?;
                    let media_ids: Vec<i32> = serde_json::from_str(&media_raw)?;
                    for id in media_ids
                        .iter()
                        .copied()
                        .chain(control_id)
                        .filter(|id| *id > 0)
                    {
                        if !ids.contains(&id) {
                            ids.push(id);
                        }
                    }
                    tx.execute("UPDATE pending SET state='pending',msg_ids=?2,created_at=?3 WHERE token=?1",
                        rusqlite::params![token,serde_json::to_string(&ids)?,now_secs()])?;
                    if pending_state == "similar" || control_id.is_some() || !media_ids.is_empty() {
                        queue_control(
                            &tx,
                            Control {
                                token,
                                owner: ControlOwner::Legacy(review_token),
                                message_id: control_id
                                    .filter(|id| *id > 0)
                                    .or_else(|| media_ids.first().copied()),
                                media: control_id.filter(|id| *id > 0).is_none(),
                            },
                        )?;
                    }
                }
            }
            tx.execute(
                "UPDATE similar_reviews SET state='retired' WHERE token=?1",
                [review_token],
            )?;
        }
        // Earlier versions can have a similar pending row without a surviving review group.
        let orphans: Vec<(i64, String, String)> = {
            let mut stmt =
                tx.prepare("SELECT token,item_meta,msg_ids FROM pending WHERE state='similar'")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            rows
        };
        for (token, item_meta, raw) in orphans {
            let _: MediaItem = serde_json::from_str(&item_meta)
                .with_context(|| format!("历史候选 {token} 元数据损坏，保留待审记录"))?;
            let ids: Vec<i32> = serde_json::from_str(&raw)?;
            tx.execute(
                "UPDATE pending SET state='pending',created_at=?2 WHERE token=?1",
                rusqlite::params![token, now_secs()],
            )?;
            queue_control(
                &tx,
                Control {
                    token,
                    owner: ControlOwner::Pending,
                    message_id: ids.iter().copied().find(|id| *id > 0),
                    media: true,
                },
            )?;
        }
        tx.commit()?;
        cleanup
    };
    let controls = {
        let db = state.db.lock().await;
        let mut stmt = db.prepare("SELECT token,owner,owner_token,message_id,media FROM image_review_retire_controls ORDER BY token")?;
        let rows = stmt
            .query_map([], |r| {
                let owner: String = r.get(1)?;
                let owner_token: i64 = r.get(2)?;
                Ok(Control {
                    token: r.get(0)?,
                    owner: match owner.as_str() {
                        "session" => ControlOwner::Session(owner_token),
                        "legacy" => ControlOwner::Legacy(owner_token),
                        _ => ControlOwner::Pending,
                    },
                    message_id: r.get(3)?,
                    media: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    for control in controls {
        let pending = {
            let db = state.db.lock().await;
            db.query_row(
                "SELECT EXISTS(SELECT 1 FROM pending WHERE token=?1 AND state='pending')",
                [control.token],
                |r| r.get::<_, bool>(0),
            )?
        };
        if !pending {
            let db = state.db.lock().await;
            db.execute(
                "DELETE FROM image_review_retire_controls WHERE token=?1",
                [control.token],
            )?;
            continue;
        }
        ensure_control(state, control).await?;
    }
    for mut session in cleanup {
        cleanup_cards(state, &mut session, true).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        Session {
            token: 50,
            item: MediaItem {
                source: crate::model::SourceKind::Pixiv,
                source_id: "123".into(),
                author: crate::model::Author {
                    name: "artist".into(),
                    url: String::new(),
                },
                title: None,
                url: "https://example.org/work".into(),
                tags: vec![],
                bookmark_count: None,
                is_r18: false,
                pixiv_type: None,
                page_count: 3,
                images: (0..3)
                    .map(|i| crate::model::ImageRef {
                        url: format!("https://example.org/{i}.png"),
                        referer: None,
                        fallback_urls: vec![],
                    })
                    .collect(),
                origin: "test".into(),
            },
            originals: (0..3)
                .map(|i| PathBuf::from(format!("/tmp/review/{i}.png")))
                .collect(),
            prepared: (0..3)
                .map(|i| PathBuf::from(format!("/tmp/review/{i}-small.png")))
                .collect(),
            choices: vec![None, Some(false), Some(true)],
            messages: vec![10, 11, 12],
            control_message: None,
            old: vec![],
        }
    }

    fn db() -> rusqlite::Connection {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE pending (
            token INTEGER PRIMARY KEY, files TEXT, caption TEXT, msg_ids TEXT,
            originals TEXT, created_at INTEGER, state TEXT, is_r18 INTEGER, item_meta TEXT
        );",
        )
        .unwrap();
        init_schema(&db).unwrap();
        init_image_dedup_schema(&db).unwrap();
        db
    }

    #[test]
    fn retire_keeps_unselected_pages_and_is_idempotent() {
        let db = db();
        let session = session();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fingerprint.png");
        image::RgbImage::new(4, 4).save(&path).unwrap();
        let fingerprint = inspect_image(&path).unwrap();
        record_work(
            &db,
            &session.item,
            std::slice::from_ref(&fingerprint),
            WorkStatus::Pending,
        )
        .unwrap();
        let mut published = session.item.clone();
        published.source_id = "already-archived".into();
        record_work(&db, &published, &[fingerprint], WorkStatus::Published).unwrap();
        db.execute(
            "INSERT INTO image_review_sessions(token,payload) VALUES(?1,?2)",
            rusqlite::params![session.token, serde_json::to_string(&session).unwrap()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO pending(token,state) VALUES(?1,'similar')",
            [session.token],
        )
        .unwrap();
        assert!(retire_session(&db, &session).unwrap());
        assert!(retire_session(&db, &session).unwrap());
        queue_control(&db, session_control(&session)).unwrap();
        init_schema(&db).unwrap();
        queue_control(&db, session_control(&session)).unwrap();
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM image_review_retire_controls",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM image_fingerprints WHERE status='pending'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM image_fingerprints WHERE status='published'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        let raw: (String, String, String, String) = db
            .query_row(
                "SELECT files,originals,item_meta,state FROM pending WHERE token=?1",
                [session.token],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(raw.3, "pending");
        assert_eq!(
            serde_json::from_str::<Vec<PathBuf>>(&raw.0).unwrap(),
            vec![session.prepared[0].clone(), session.prepared[2].clone()]
        );
        assert_eq!(
            serde_json::from_str::<Vec<PathBuf>>(&raw.1).unwrap(),
            vec![session.originals[0].clone(), session.originals[2].clone()]
        );
        let item: MediaItem = serde_json::from_str(&raw.2).unwrap();
        assert_eq!(item.page_count, 2);
        assert_eq!(item.images[1].url, "https://example.org/2.png");
        let state: String = db
            .query_row(
                "SELECT state FROM image_review_sessions WHERE token=?1",
                [session.token],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "retired");
    }

    #[test]
    fn gallery_only_session_cannot_enter_publish_queue() {
        let db = db();
        let mut session = session();
        session.originals.clear();
        session.prepared.clear();
        session.choices.clear();
        db.execute(
            "INSERT INTO image_review_sessions(token,payload) VALUES(?1,?2)",
            rusqlite::params![session.token, serde_json::to_string(&session).unwrap()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO pending(token,state) VALUES(?1,'similar')",
            [session.token],
        )
        .unwrap();
        assert!(!retire_session(&db, &session).unwrap());
        assert!(!db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pending WHERE token=?1)",
                [session.token],
                |r| r.get::<_, bool>(0)
            )
            .unwrap());
        assert_eq!(
            db.query_row(
                "SELECT state FROM image_review_sessions WHERE token=?1",
                [session.token],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "decided"
        );
    }
    #[tokio::test]
    async fn undo_old_image_confirms_remote_restore_before_changing_local_state() {
        use std::io::{BufRead, Read, Write};
        for (remote_state, reject) in [
            ("prepared", true),
            ("prepared", false),
            ("deleted", false),
            ("restored", false),
            ("unexpected", true),
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                for step in 0..if reject { 2 } else { 3 } {
                    let (mut socket, _) = listener.accept().unwrap();
                    socket
                        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                        .unwrap();
                    let mut reader = std::io::BufReader::new(&mut socket);
                    let mut route = String::new();
                    reader.read_line(&mut route).unwrap();
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" {
                            break;
                        }
                        if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                            length = value.trim().parse().unwrap();
                        }
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    let (status, response) = if step < 2 {
                        assert!(route.starts_with("POST /api/catalog/image-review "));
                        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        assert_eq!(
                            body["action"],
                            if step == 0 { "prepare" } else { "restore" }
                        );
                        if step == 1 {
                            assert_eq!(
                                body["restored"].as_array().unwrap().len(),
                                usize::from(remote_state == "deleted")
                            );
                        }
                        if step == 1 && reject {
                            (
                                "409 Conflict",
                                serde_json::json!({"ok":false,"error":"decision changed"}),
                            )
                        } else {
                            (
                                "200 OK",
                                serde_json::json!({"ok":true,"state":if step == 0 { remote_state } else { "restored" }}),
                            )
                        }
                    } else {
                        assert!(route.to_lowercase().contains("editmessagecaption"));
                        (
                            "200 OK",
                            serde_json::json!({"ok":true,"result":{"message_id":10,"date":1,"chat":{"id":123,"type":"private"},"text":"review"}}),
                        )
                    };
                    let body = response.to_string();
                    write!(socket,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
                }
            });
            let dir = tempfile::tempdir().unwrap();
            let mut sink = TelegramSink::new(
                "123:fake".into(),
                "123".into(),
                "-100123".into(),
                dir.path().join("hanabi.db").to_str().unwrap(),
                Some(GalleryClient::new(base.clone(), "fake".into()).unwrap()),
            )
            .unwrap();
            let inner = Arc::get_mut(&mut sink.state).unwrap();
            inner.bot = inner
                .bot
                .clone()
                .set_api_url(reqwest::Url::parse(&base).unwrap());
            let state = sink.state();
            let path = dir.path().join("old.png");
            image::RgbImage::new(4, 4).save(&path).unwrap();
            let mut review = session();
            review.old = vec![OldImage {
                image_id: "pixiv:321#0".into(),
                r2_key: "old.png".into(),
                path: path.clone(),
                message_id: 10,
                deleted: true,
            }];
            let journal = DeleteUndo {
                session_state: "pending".into(),
                token: review.token,
                index: 0,
                decision_id: "test-decision".into(),
                r2_key: "old.png".into(),
                path,
                targets: vec![Target {
                    publication_id: "publication".into(),
                    chat_id: -100123,
                    message_id: 11,
                }],
                restored: if remote_state == "deleted" {
                    vec![Restored {
                        publication_id: "publication".into(),
                        message_id: 12,
                    }]
                } else {
                    vec![]
                },
                fingerprint: None,
            };
            let action = {
                let mut db = state.db.lock().await;
                db.execute("INSERT INTO image_review_sessions(token,payload,state) VALUES(?1,?2,'deleting')",rusqlite::params![review.token,serde_json::to_string(&review).unwrap()]).unwrap();
                db.execute("INSERT INTO pending(token,files,caption,msg_ids,originals,created_at,state,is_r18,item_meta) VALUES(?1,'[]','','[]','[]',1,'similar',0,'{}')", [review.token]).unwrap();
                db.execute("INSERT INTO review_actions(action,token,item_meta,files,originals,acted_at) VALUES('image_delete',?1,?2,?3,?3,1)",rusqlite::params![review.token,serde_json::to_string(&journal).unwrap(),serde_json::to_string(&vec![&journal.path]).unwrap()]).unwrap();
                let UndoClaim::Claimed(action) = claim_latest_undo(&mut db).unwrap() else {
                    panic!("image deletion must be undoable")
                };
                action
            };
            let result = undo_delete(&state, &action).await;
            server.join().unwrap();
            let mut db = state.db.lock().await;
            let (review, status) = load(&db, review.token).unwrap().unwrap();
            assert_eq!(
                result.is_err(),
                reject,
                "remote_state={remote_state}: {result:?}"
            );
            assert_eq!(review.old[0].deleted, reject);
            assert_eq!(status, if reject { "deleting" } else { "retired" });
            let pending_state: String = db
                .query_row(
                    "SELECT state FROM pending WHERE token=?1",
                    [review.token],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(pending_state, if reject { "similar" } else { "pending" });
            if reject {
                restore_undo(&db, action.id).unwrap();
                assert!(matches!(
                    claim_latest_undo(&mut db).unwrap(),
                    UndoClaim::Claimed(_)
                ));
            } else {
                assert_eq!(finish_undo(&db, action.id).unwrap(), 1);
            }
        }
    }
}
