//! Image choices are independent; only the final choice publishes the selected pages.
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

struct QueuedReview(Arc<ReviewState>);

impl Drop for QueuedReview {
    fn drop(&mut self) {
        self.0.queued_reviews.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(super) fn init_schema(db: &rusqlite::Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS image_review_sessions (
        token INTEGER PRIMARY KEY, payload TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'pending'
    ); UPDATE image_review_sessions SET state='pending' WHERE state='processing';",
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

fn keyboard(token: i64, index: usize, choice: Option<bool>) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback(
            if choice == Some(true) {
                "✅ 已保留"
            } else {
                "✅ 保留"
            },
            format!("image:{token}:keep:{index}"),
        ),
        InlineKeyboardButton::callback(
            if choice == Some(false) {
                "❌ 已丢弃"
            } else {
                "❌ 丢弃"
            },
            format!("image:{token}:discard:{index}"),
        ),
    ]])
}

fn old_keyboard(token: i64, index: usize, deleted: bool) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        if deleted {
            "🗑 已删除 · /undo 可撤销"
        } else {
            "🗑 删除这张旧图"
        },
        format!("image:{token}:delete:{index}"),
    )]])
}

async fn send_card(
    state: &Arc<ReviewState>,
    path: &Path,
    caption: String,
    buttons: InlineKeyboardMarkup,
) -> Result<i32> {
    let msg = if any_oversized_for_photo(&[path.to_path_buf()]) {
        tg_retry(|| {
            state
                .bot
                .send_document(state.review_chat.clone(), InputFile::file(path))
                .caption(caption.clone())
                .parse_mode(ParseMode::Html)
                .reply_markup(buttons.clone())
        })
        .await?
    } else {
        tg_retry(|| {
            state
                .bot
                .send_photo(state.review_chat.clone(), InputFile::file(path))
                .caption(caption.clone())
                .parse_mode(ParseMode::Html)
                .reply_markup(buttons.clone())
        })
        .await?
    };
    Ok(msg.id.0)
}

pub(super) async fn queue(
    state: &Arc<ReviewState>,
    item: &MediaItem,
    files: &[PathBuf],
    fingerprints: &[ImageFingerprint],
    matches: &[SimilarImage],
) -> Result<()> {
    queue_inner(state, item, files, fingerprints, matches, None).await
}

async fn queue_inner(
    state: &Arc<ReviewState>,
    item: &MediaItem,
    files: &[PathBuf],
    fingerprints: &[ImageFingerprint],
    matches: &[SimilarImage],
    legacy: Option<(i64, i64)>,
) -> Result<()> {
    let gallery = state
        .gallery
        .as_ref()
        .context("相似图审批需要已配置 Vitrine")?;
    let token = i64::try_from(state.next_token())?;
    let root = crate::util::pending_root().join(format!("hanabi_image_review_{token}"));
    std::fs::create_dir_all(&root)?;
    let mut session = Session {
        token,
        item: item.clone(),
        originals: vec![],
        prepared: vec![],
        choices: vec![None; files.len()],
        messages: vec![],
        control_message: None,
        old: vec![],
    };
    let result: Result<()> = async {
        for (index, file) in files.iter().enumerate() {
            let ext = file.extension().and_then(|v| v.to_str()).unwrap_or("jpg");
            let dest = root.join(format!("candidate-{index}.{ext}"));
            std::fs::copy(file, &dest)?;
            session.originals.push(dest);
        }
        let originals = session.originals.clone();
        session.prepared = tokio::task::spawn_blocking(move || prepare_all(&originals)).await??;
        let mut seen = HashSet::new();
        for matched in matches {
            let work_id = format!(
                "{}:{}",
                matched.existing_work.source.as_str(),
                matched.existing_work.source_id
            );
            if !seen.insert(work_id.clone()) {
                continue;
            }
            let downloaded = gallery
                .download_work_images(&work_id, &root.join("download"))
                .await?;
            for image in downloaded {
                let dest = root.join(format!(
                    "old-{}.{}",
                    session.old.len(),
                    image
                        .path
                        .extension()
                        .and_then(|v| v.to_str())
                        .unwrap_or("jpg")
                ));
                std::fs::rename(image.path, &dest)?;
                session.old.push(OldImage {
                    image_id: format!("{work_id}#{}", image.page_index),
                    r2_key: image.r2_key,
                    path: dest,
                    message_id: 0,
                    deleted: false,
                });
            }
        }
        // Show the actual matched pages immediately before their candidate, not entire albums.
        let mut shown = HashSet::new();
        for (index, fingerprint) in fingerprints.iter().enumerate() {
            for matched in matches.iter().filter(|m| m.current_index == index) {
                let work_prefix = format!(
                    "{}:{}#",
                    matched.existing_work.source.as_str(),
                    matched.existing_work.source_id
                );
                if let Some((old_index, old)) = session
                    .old
                    .iter_mut()
                    .enumerate()
                    .filter(|(_, old)| old.image_id.starts_with(&work_prefix))
                    .nth(matched.existing_index)
                {
                    if shown.insert(old_index) {
                        let path = old.path.clone();
                        let prepared =
                            tokio::task::spawn_blocking(move || prepare_all(&[path])).await??;
                        old.message_id = send_card(
                            state,
                            &prepared[0],
                            format!(
                                "【相似旧图 · 默认保留】\n{}\n对应候选第 {} 张 · {} · {}",
                                crate::sink::html_escape(&old.image_id),
                                index + 1,
                                matched.existing.dimensions_label(),
                                review_bytes(matched.existing.bytes)
                            ),
                            old_keyboard(token, old_index, false),
                        )
                        .await?;
                    }
                }
            }
            let message = send_card(
                state,
                &session.prepared[index],
                format!(
                    "【候选 {}/{}】\n{}\n{}\n\n逐张保留／丢弃，审完自动发送并入库。误点可 /undo。",
                    index + 1,
                    files.len(),
                    render_caption(item),
                    review_image_label("原图", index, fingerprint)
                ),
                keyboard(token, index, None),
            )
            .await?;
            session.messages.push(message);
        }
        let db = state.db.lock().await;
        let tx = db.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO image_review_sessions(token,payload) VALUES(?1,?2)",
            rusqlite::params![token, serde_json::to_string(&session)?],
        )?;
        insert_pending(&tx, &session)?;
        record_work(&tx, item, fingerprints, WorkStatus::Pending)?;
        if let Some((review_token, pending_token)) = legacy {
            if tx.execute(
                "DELETE FROM pending WHERE token=?1 AND state='similar'",
                [pending_token],
            )? != 1
            {
                anyhow::bail!("旧审批状态已变化");
            }
            tx.execute(
                "UPDATE similar_reviews SET state='migrated' WHERE token=?1 AND state='pending'",
                [review_token],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    .await;
    if let Err(error) = result {
        let ids: Vec<MessageId> = session.all_messages().into_iter().map(MessageId).collect();
        cleanup_review_messages(state, &ids).await;
        remove_dir_all_bg(root);
        return Err(error);
    }
    cleanup(files);
    Ok(())
}

impl Session {
    fn all_messages(&self) -> Vec<i32> {
        self.messages
            .iter()
            .copied()
            .chain(self.old.iter().map(|i| i.message_id).filter(|id| *id > 0))
            .chain(self.control_message)
            .collect()
    }

    fn row(&self, selected_only: bool) -> Result<PendingRow> {
        let indices: Vec<_> = (0..self.originals.len())
            .filter(|i| !selected_only || self.choices[*i] == Some(true))
            .collect();
        let files: Vec<_> = if self.originals.is_empty() {
            self.old.iter().map(|i| &i.path).collect()
        } else {
            indices.iter().map(|i| &self.prepared[*i]).collect()
        };
        let originals: Vec<_> = indices.iter().map(|i| &self.originals[*i]).collect();
        let mut item = self.item.clone();
        if selected_only {
            item.page_count = indices.len() as u32;
            item.images = indices
                .iter()
                .filter_map(|i| self.item.images.get(*i).cloned())
                .collect();
        }
        Ok((
            serde_json::to_string(&files)?,
            render_caption(&item),
            serde_json::to_string(&self.all_messages())?,
            serde_json::to_string(&originals)?,
            item.is_r18,
            serde_json::to_string(&item)?,
        ))
    }
}

fn insert_pending(db: &rusqlite::Connection, session: &Session) -> Result<()> {
    let row = session.row(false)?;
    db.execute("INSERT OR REPLACE INTO pending(token,files,caption,msg_ids,originals,created_at,state,is_r18,item_meta) VALUES(?1,?2,?3,?4,?5,?6,'similar',?7,?8)", rusqlite::params![session.token,row.0,row.1,row.2,row.3,now_secs(),row.4,row.5])?;
    Ok(())
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

fn record_choice(db: &rusqlite::Connection, before: &Session, after: &Session) -> Result<()> {
    let row = before.row(false)?;
    db.execute("INSERT INTO review_actions(action,token,files,originals,item_meta,acted_at,state) VALUES('image_review',?1,?2,?3,?4,?5,'available')", rusqlite::params![before.token,row.0,row.3,serde_json::to_string(before)?,now_secs()])?;
    // Older journals are retired by the common cleanup after the current task releases its files.
    db.execute("UPDATE review_actions SET state='superseded' WHERE state='available' AND id<>last_insert_rowid()", [])?;
    db.execute(
        "UPDATE image_review_sessions SET payload=?2 WHERE token=?1",
        rusqlite::params![after.token, serde_json::to_string(after)?],
    )?;
    Ok(())
}

async fn refresh(state: &Arc<ReviewState>, session: &Session, decided: bool) -> Result<()> {
    for (index, id) in session.messages.iter().enumerate() {
        let buttons = if decided {
            InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new())
        } else {
            keyboard(session.token, index, session.choices[index])
        };
        // A repeated refresh after a network timeout may legitimately find the same markup.
        if let Err(error) = tg_retry(|| {
            state
                .bot
                .edit_message_reply_markup(state.review_chat.clone(), MessageId(*id))
                .reply_markup(buttons.clone())
        })
        .await
        {
            if !error.to_string().contains("message is not modified") {
                return Err(error.into());
            }
        }
    }
    Ok(())
}

pub(super) async fn handle(state: &Arc<ReviewState>, q: CallbackQuery, data: &str) -> Result<()> {
    let parts: Vec<_> = data.split(':').collect();
    let parsed = (|| {
        Some((
            parts.get(1)?.parse::<i64>().ok()?,
            *parts.get(2)?,
            parts.get(3)?.parse::<usize>().ok()?,
        ))
    })();
    let Some((token, action, index)) = parsed.filter(|_| parts.len() == 4) else {
        state
            .bot
            .answer_callback_query(q.id)
            .text("无效图片操作")
            .await?;
        return Ok(());
    };
    state
        .bot
        .answer_callback_query(q.id)
        .text("已接收，按点击顺序处理")
        .await?;
    let state = state.clone();
    let action = action.to_string();
    state.queued_reviews.fetch_add(1, Ordering::SeqCst);
    let queued = QueuedReview(state.clone());
    // Register with Tokio's FIFO mutex before returning to the next callback.
    let mut lock = Box::pin(state.review_gate.clone().lock_owned());
    let ready = std::future::poll_fn(|cx| {
        std::task::Poll::Ready(match std::future::Future::poll(lock.as_mut(), cx) {
            std::task::Poll::Ready(guard) => Some(guard),
            std::task::Poll::Pending => None,
        })
    })
    .await;
    tokio::spawn(async move {
        let _queued = queued;
        let guard = match ready {
            Some(guard) => guard,
            None => lock.await,
        };
        if let Err(error) = apply_choice(&state, token, &action, index, guard).await {
            let _ = state
                .bot
                .send_message(
                    state.review_chat.clone(),
                    format!("⚠️ 图片操作未完成：{error}"),
                )
                .await;
        }
    });
    Ok(())
}

async fn apply_choice(
    state: &Arc<ReviewState>,
    token: i64,
    action: &str,
    index: usize,
    guard: tokio::sync::OwnedMutexGuard<()>,
) -> Result<()> {
    {
        let db = state.db.lock().await;
        if needs_recovery(&db)? {
            anyhow::bail!("上一张旧图尚未恢复，请先 /undo");
        }
    }
    let loaded = {
        let db = state.db.lock().await;
        load(&db, token)?
    };
    let Some((mut session, status)) = loaded else {
        anyhow::bail!("审批已过期");
    };
    if action == "finish" && session.choices.is_empty() && status == "pending" {
        {
            let db = state.db.lock().await;
            let tx = db.unchecked_transaction()?;
            record_choice(&tx, &session, &session)?;
            tx.execute(
                "DELETE FROM pending WHERE token=?1 AND state='similar'",
                [token],
            )?;
            tx.execute(
                "UPDATE image_review_sessions SET state='decided' WHERE token=?1",
                [token],
            )?;
            tx.commit()?;
        }

        return Ok(());
    }
    if action == "delete" {
        return delete_old(state, session, status, index, guard).await;
    }
    if status != "pending"
        || index >= session.choices.len()
        || !matches!(action, "keep" | "discard")
    {
        anyhow::bail!("该审批已完成或操作无效");
    }
    let keep = action == "keep";
    let retry = session.choices.iter().all(Option::is_some);
    if session.choices[index] == Some(keep) && !retry {
        return Ok(());
    }
    {
        let db = state.db.lock().await;
        let tx = db.unchecked_transaction()?;
        let before = session.clone();
        session.choices[index] = Some(keep);
        if before.choices[index] != session.choices[index] {
            record_choice(&tx, &before, &session)?;
        }
        tx.commit()?;
    }
    let state = state.clone();
    tokio::spawn(async move {
        let _guard = guard;
        let result: Result<()> = async {
            if session.choices.iter().any(Option::is_none) {
                state
                    .bot
                    .edit_message_reply_markup(
                        state.review_chat.clone(),
                        MessageId(session.messages[index]),
                    )
                    .reply_markup(keyboard(token, index, session.choices[index]))
                    .await?;
                return Ok(());
            }
            let selected = session.choices.iter().filter(|c| **c == Some(true)).count();
            {
                let db = state.db.lock().await;
                if !matches!(claim_similar_pending(&db, token)?, PendingClaim::Claimed(_)) {
                    anyhow::bail!("候选正在处理或已失效");
                }
                db.execute(
                    "UPDATE image_review_sessions SET state='processing' WHERE token=?1",
                    [token],
                )?;
            }
            let row = session.row(selected > 0)?;
            let outcome =
                finish_claimed(&state, token, row, selected > 0, selected > 0, false).await?;
            if selected > 0 {
                let paths: Vec<_> = session
                    .originals
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| session.choices[*i] == Some(true))
                    .map(|(_, p)| p.clone())
                    .collect();
                let fps = tokio::task::spawn_blocking(move || {
                    paths
                        .iter()
                        .map(|p| inspect_image(p))
                        .collect::<Result<Vec<_>>>()
                })
                .await??;
                let db = state.db.lock().await;
                record_work(&db, &session.item, &fps, WorkStatus::Published)?;
            }
            refresh(&state, &session, true).await?;
            let text = if outcome.partial_publish {
                "⚠️ 仅部分图片发送成功，可 /undo 撤回后重审".to_string()
            } else if outcome.archive_failed {
                "⚠️ 频道已发，图库入库未完成；可 /undo 撤回".to_string()
            } else {
                format!(
                    "✅ 已处理：保留 {selected} 张，丢弃 {} 张。旧图保留。误点可 /undo。",
                    session.choices.len() - selected
                )
            };
            state
                .bot
                .send_message(state.review_chat.clone(), text)
                .await?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            let db = state.db.lock().await;
            let _ = db.execute("UPDATE image_review_sessions SET state='pending' WHERE token=?1 AND state='processing'", [token]);
            let _ = restore_claimed_pending(&db, token);
            drop(db);
            tracing::warn!(token, %error, "逐图审批未完成");
            let _ = state
                .bot
                .send_message(
                    state.review_chat.clone(),
                    format!("⚠️ 未完成：{error}。选择已保留，再点最后一张可重试，也可 /undo。"),
                )
                .await;
        }
    });
    Ok(())
}

pub(super) async fn undo(state: &Arc<ReviewState>, action: &UndoAction) -> Result<()> {
    let session: Session = serde_json::from_str(&action.row.5)?;
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
    let paths = session.originals.clone();
    let fps = tokio::task::spawn_blocking(move || {
        paths
            .iter()
            .map(|p| inspect_image(p))
            .collect::<Result<Vec<_>>>()
    })
    .await??;
    {
        let db = state.db.lock().await;
        let tx = db.unchecked_transaction()?;
        insert_pending(&tx, &session)?;
        tx.execute(
            "UPDATE image_review_sessions SET payload=?2,state='pending' WHERE token=?1",
            rusqlite::params![session.token, serde_json::to_string(&session)?],
        )?;
        if !session.originals.is_empty() {
            record_work(&tx, &session.item, &fps, WorkStatus::Pending)?;
        }
        tx.commit()?;
    }
    refresh(state, &session, false).await?;
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

fn fingerprint_snapshot(
    db: &rusqlite::Connection,
    image_id: &str,
) -> Result<Option<(Vec<String>, Vec<serde_json::Value>)>> {
    let (work, index) = image_id.rsplit_once('#').context("旧图页码无效")?;
    let (source, id) = work.split_once(':').context("旧图作品编号无效")?;
    let mut stmt = db.prepare(
        "SELECT * FROM image_fingerprints WHERE source_kind=?1 AND source_id=?2 AND image_index=?3",
    )?;
    let columns = stmt
        .column_names()
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>();
    let count = columns.len();
    let values = stmt
        .query_row(rusqlite::params![source, id, index], |row| {
            (0..count)
                .map(|i| {
                    Ok(match row.get_ref(i)? {
                        rusqlite::types::ValueRef::Null => serde_json::Value::Null,
                        rusqlite::types::ValueRef::Integer(n) => serde_json::json!(n),
                        rusqlite::types::ValueRef::Real(n) => serde_json::json!(n),
                        rusqlite::types::ValueRef::Text(s) => {
                            serde_json::json!(String::from_utf8_lossy(s))
                        }
                        _ => return Err(rusqlite::Error::InvalidQuery),
                    })
                })
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .optional()?;
    Ok(values.map(|values| (columns, values)))
}

async fn delete_old(
    state: &Arc<ReviewState>,
    mut session: Session,
    status: String,
    index: usize,
    guard: tokio::sync::OwnedMutexGuard<()>,
) -> Result<()> {
    let Some(old) = session
        .old
        .get(index)
        .filter(|old| !old.deleted && matches!(status.as_str(), "pending" | "decided"))
    else {
        anyhow::bail!("该旧图已删除或审批已失效");
    };
    let gallery = state.gallery.as_ref().context("图库未配置")?.clone();
    let mut journal = DeleteUndo {
        session_state: status,
        token: session.token,
        index,
        decision_id: format!(
            "hanabi-image-{}-{}-{}",
            session.token,
            index,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ),
        r2_key: old.r2_key.clone(),
        path: old.path.clone(),
        targets: vec![],
        restored: vec![],
        fingerprint: None,
    };

    let state = state.clone();
    tokio::spawn(async move {
        let _guard = guard;
        let mut action_id = None;
        let result: Result<()> = async {
            let response=gallery.review_image(&serde_json::json!({"decision_id":journal.decision_id,"r2_key":journal.r2_key,"action":"prepare"})).await?;
            journal.targets=serde_json::from_value(response["targets"].clone())?;
            if journal.targets.is_empty() { anyhow::bail!("未找到这张图片的频道消息，未执行删除"); }
            {
                let db=state.db.lock().await;
                let tx=db.unchecked_transaction()?;
                journal.fingerprint=fingerprint_snapshot(&tx,&session.old[index].image_id)?;
                tx.execute("INSERT INTO review_actions(action,token,files,originals,item_meta,acted_at,state) VALUES('image_delete',?1,?2,?2,?3,?4,'available')",rusqlite::params![session.token,serde_json::to_string(&vec![&journal.path])?,serde_json::to_string(&journal)?,now_secs()])?;
                action_id=Some(tx.last_insert_rowid());
                tx.execute("UPDATE review_actions SET state='superseded' WHERE state='available' AND id<>last_insert_rowid()",[])?;
                tx.execute("UPDATE image_review_sessions SET state='deleting' WHERE token=?1",[session.token])?;
                tx.commit()?;
            }
            gallery.review_image(&serde_json::json!({"decision_id":journal.decision_id,"r2_key":journal.r2_key,"action":"delete"})).await?;
            for target in &journal.targets {
                let publication=GalleryPublication { chat_id:target.chat_id,message_ids:vec![target.message_id],publish_state:GalleryPublishState::Full };
                delete_undo_telegram_messages(&state.bot,&publication).await?;
            }
            session.old[index].deleted=true;
            {
                let db=state.db.lock().await;
                let tx=db.unchecked_transaction()?;
                let (work,page)=session.old[index].image_id.rsplit_once('#').context("旧图页码无效")?;
                let (source,id)=work.split_once(':').context("旧图作品编号无效")?;
                tx.execute("DELETE FROM image_fingerprints WHERE source_kind=?1 AND source_id=?2 AND image_index=?3",rusqlite::params![source,id,page])?;
                tx.execute("UPDATE image_review_sessions SET payload=?2,state=?3 WHERE token=?1",rusqlite::params![session.token,serde_json::to_string(&session)?,journal.session_state])?;
                tx.commit()?;
            }
            state.bot.edit_message_reply_markup(state.review_chat.clone(),MessageId(session.old[index].message_id)).reply_markup(old_keyboard(session.token,index,true)).await?;
            Ok(())
        }.await;
        if let Err(error) = result {
            tracing::warn!(%error, "旧图删除未完成");
            let suffix = if let Some(id) = action_id {
                let action = UndoAction {
                    id,
                    action: "image_delete".into(),
                    row: (
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        false,
                        serde_json::to_string(&journal).unwrap(),
                    ),
                    publication: String::new(),
                };
                {
                    let db = state.db.lock().await;
                    let _ = db.execute(
                        "UPDATE review_actions SET state='restoring' WHERE id=?1",
                        [id],
                    );
                }
                match undo_delete(&state, &action).await {
                    Ok(()) => {
                        let db = state.db.lock().await;
                        let _ = finish_undo(&db, id);
                        let _=db.execute("UPDATE review_actions SET state='available' WHERE id=(SELECT MAX(id) FROM review_actions WHERE state='superseded')",[]);
                        "已自动恢复图片及图库，之前的审批选择保持不变。"
                    }
                    Err(rollback_error) => {
                        tracing::warn!(%rollback_error,"自动恢复旧图未完成");
                        let db = state.db.lock().await;
                        let _ = restore_undo(&db, id);
                        "恢复记录已保存，请用 /undo 恢复后再操作。"
                    }
                }
            } else {
                "图片未删除。"
            };
            let _ = state
                .bot
                .send_message(
                    state.review_chat.clone(),
                    format!("⚠️ 删除未完成：{error}。{suffix}"),
                )
                .await;
        }
    });
    Ok(())
}

pub(super) async fn undo_delete(state: &Arc<ReviewState>, action: &UndoAction) -> Result<()> {
    let mut journal: DeleteUndo = serde_json::from_str(&action.row.5)?;
    let gallery = state.gallery.as_ref().context("图库未配置")?;
    let response=gallery.review_image(&serde_json::json!({"decision_id":journal.decision_id,"r2_key":journal.r2_key,"action":"prepare"})).await?;
    if response["state"] == "deleted" {
        if !journal.path.is_file() {
            anyhow::bail!("恢复原图已过期");
        }
        let path = journal.path.clone();
        let prepared = tokio::task::spawn_blocking(move || prepare_all(&[path])).await??;
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
        gallery.review_image(&serde_json::json!({"decision_id":journal.decision_id,"r2_key":journal.r2_key,"action":"restore","restored":journal.restored})).await?;
    }
    let session = {
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
        session
    };
    if let Err(error) = state
        .bot
        .edit_message_reply_markup(
            state.review_chat.clone(),
            MessageId(session.old[journal.index].message_id),
        )
        .reply_markup(old_keyboard(journal.token, journal.index, false))
        .await
    {
        if !error.to_string().contains("message is not modified") {
            return Err(error.into());
        }
    }
    Ok(())
}

pub(super) async fn cleanup_unreferenced(state: &Arc<ReviewState>, paths: &[PathBuf]) {
    let Some(parent) = paths.first().and_then(|p| p.parent()) else {
        return;
    };
    let db = state.db.lock().await;
    let referenced = (|| -> rusqlite::Result<bool> {
        let mut stmt = db.prepare("SELECT files FROM pending UNION ALL SELECT files FROM review_actions WHERE state IN ('available','restoring')")?;
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
            let mut stmt=tx.prepare("SELECT payload FROM image_review_sessions s WHERE NOT EXISTS(SELECT 1 FROM pending p WHERE p.token=s.token) AND NOT EXISTS(SELECT 1 FROM review_actions a WHERE a.token=s.token AND a.state IN ('available','restoring'))")?;
            let rows=stmt.query_map([],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().map(|raw| serde_json::from_str(&raw)).collect::<serde_json::Result<Vec<Session>>>()?
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

pub(super) async fn migrate_legacy(state: &Arc<ReviewState>, token: i64) -> Result<bool> {
    let loaded = {
        let db = state.db.lock().await;
        let Some(group) = load_similar_review(&db, token)? else {
            return Ok(false);
        };
        let Some(candidate) = group.pending_candidate else {
            drop(db);
            return migrate_gallery_legacy(state, token, &group).await;
        };
        let row: Option<PendingRow> = db.query_row("SELECT files,caption,msg_ids,originals,is_r18,item_meta FROM pending WHERE token=?1 AND state='similar'",[candidate.pending_token],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
        row.map(|row| (candidate.pending_token, candidate.work_id, row))
    };
    let Some((pending_token, expected_work_id, row)) = loaded else {
        let group = {
            let db = state.db.lock().await;
            if !candidate_was_published(&db, token)? {
                return Ok(false);
            }
            load_similar_review(&db, token)?
        };
        return match group {
            Some(group) => migrate_gallery_legacy(state, token, &group).await,
            None => Ok(false),
        };
    };
    let work = decode_pending(row)?;
    let item: MediaItem = serde_json::from_str(&work.item_meta)?;
    if format!("{}:{}", item.source.as_str(), item.source_id) != expected_work_id {
        anyhow::bail!("旧审批引用了不同的候选作品，已停止迁移");
    }
    let paths = if work.originals.is_empty() {
        work.files
    } else {
        work.originals
    };
    let scan_paths = paths.clone();
    let fps = tokio::task::spawn_blocking(move || {
        scan_paths
            .iter()
            .map(|p| inspect_image(p))
            .collect::<Result<Vec<_>>>()
    })
    .await??;
    let matches = {
        let db = state.db.lock().await;
        evaluate_work(&db, &item, &fps)?.similar
    };
    queue_inner(
        state,
        &item,
        &paths,
        &fps,
        &matches,
        Some((token, pending_token)),
    )
    .await?;
    cleanup_review_messages(
        state,
        &work.msg_ids.into_iter().map(MessageId).collect::<Vec<_>>(),
    )
    .await;
    Ok(true)
}

async fn migrate_gallery_legacy(
    state: &Arc<ReviewState>,
    legacy_token: i64,
    group: &SimilarReviewGroup,
) -> Result<bool> {
    if group.group_key.starts_with("exact:") {
        return Ok(false);
    }
    let gallery = state.gallery.as_ref().context("图库未配置")?;
    let token = i64::try_from(state.next_token())?;
    let root = crate::util::pending_root().join(format!("hanabi_image_review_{token}"));
    std::fs::create_dir_all(&root)?;
    let first = group.images.first().context("旧审批没有图片")?;
    let (source, source_id) = first.work_id().split_once(':').context("旧作品编号无效")?;
    let item = MediaItem {
        source: serde_json::from_value(serde_json::json!(source))?,
        source_id: source_id.into(),
        title: None,
        url: String::new(),
        author: crate::model::Author {
            name: String::new(),
            url: String::new(),
        },
        tags: vec![],
        bookmark_count: None,
        is_r18: false,
        pixiv_type: None,
        page_count: 0,
        images: vec![],
        origin: "image_review".into(),
    };
    let mut session = Session {
        token,
        item,
        originals: vec![],
        prepared: vec![],
        choices: vec![],
        messages: vec![],
        control_message: None,
        old: vec![],
    };
    let result: Result<()> = async {
        for post in group.posts() {
            let downloaded = gallery
                .download_work_images(&post.work_id, &root.join("download"))
                .await?;
            for image in downloaded {
                let image_id = format!("{}#{}", post.work_id, image.page_index);
                if !group.images.iter().any(|i| {
                    i.r2_key == image.r2_key || (i.r2_key.is_empty() && i.image_id == image_id)
                }) {
                    continue;
                }
                let dest = root.join(format!(
                    "old-{}.{}",
                    session.old.len(),
                    image
                        .path
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("jpg")
                ));
                std::fs::rename(image.path, &dest)?;
                let path = dest.clone();
                let prepared = tokio::task::spawn_blocking(move || prepare_all(&[path])).await??;
                let message_id = send_card(
                    state,
                    &prepared[0],
                    format!(
                        "【旧图 · 默认保留】\n{}\n删除只影响这一张；误点可 /undo。",
                        crate::sink::html_escape(&image_id)
                    ),
                    old_keyboard(token, session.old.len(), false),
                )
                .await?;
                session.old.push(OldImage {
                    image_id,
                    r2_key: image.r2_key,
                    path: dest,
                    message_id,
                    deleted: false,
                });
            }
        }
        if session.old.is_empty() {
            anyhow::bail!("旧审批图片已被替换，未关闭原审批");
        }
        let control = state
            .bot
            .send_message(
                state.review_chat.clone(),
                "旧审批已改为逐张处理；需要删哪张，就点那张图下的删除。",
            )
            .reply_markup(InlineKeyboardMarkup::new(vec![vec![
                InlineKeyboardButton::callback(
                    "✅ 保留其余旧图并结束",
                    format!("image:{token}:finish:0"),
                ),
            ]]))
            .await?;
        session.control_message = Some(control.id.0);
        let db = state.db.lock().await;
        let tx = db.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO image_review_sessions(token,payload) VALUES(?1,?2)",
            rusqlite::params![token, serde_json::to_string(&session)?],
        )?;
        insert_pending(&tx, &session)?;
        tx.execute(
            "UPDATE similar_reviews SET state='migrated' WHERE token=?1 AND state='pending'",
            [legacy_token],
        )?;
        tx.commit()?;
        Ok(())
    }
    .await;
    if let Err(error) = result {
        cleanup_review_messages(
            state,
            &session
                .all_messages()
                .into_iter()
                .map(MessageId)
                .collect::<Vec<_>>(),
        )
        .await;
        remove_dir_all_bg(root);
        return Err(error);
    }
    let old_ids = {
        let db = state.db.lock().await;
        similar_review_messages(&db, legacy_token)?
    };
    if let Some((ids, control)) = old_ids {
        cleanup_review_messages(
            state,
            &similar_review_cleanup_message_ids(&ids, control)
                .into_iter()
                .map(MessageId)
                .collect::<Vec<_>>(),
        )
        .await;
    }
    Ok(true)
}

pub(super) async fn migrate_pending(state: Arc<ReviewState>) {
    let tokens = {
        let db = state.db.lock().await;
        let result = (|| -> rusqlite::Result<Vec<i64>> {
            let mut stmt = db.prepare(
                "SELECT token FROM similar_reviews WHERE state='pending' ORDER BY token",
            )?;
            let rows = stmt.query_map([], |r| r.get(0))?.collect();
            rows
        })();
        result.unwrap_or_default()
    };
    for token in tokens {
        let _guard = state.review_gate.lock().await;
        if let Err(error) = migrate_legacy(&state, token).await {
            tracing::warn!(token,%error,"迁移旧相似图审批失败，旧记录保留");
        }
    }
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
                    url: "https://example.org".into(),
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
            choices: vec![None; 3],
            messages: vec![10, 11, 12],
            control_message: None,
            old: vec![],
        }
    }

    #[test]
    fn independent_choices_keep_siblings_and_journal_the_exact_pre_click_state() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("hanabi.db");
        let _sink = TelegramSink::new(
            "123:abc".into(),
            "123".into(),
            "@channel".into(),
            db_path.to_str().unwrap(),
            None,
        )
        .unwrap();
        let mut db = rusqlite::Connection::open(&db_path).unwrap();
        let before = session();
        db.execute(
            "INSERT INTO image_review_sessions(token,payload) VALUES(?1,?2)",
            rusqlite::params![before.token, serde_json::to_string(&before).unwrap()],
        )
        .unwrap();
        insert_pending(&db, &before).unwrap();
        let mut selected = before.clone();
        for (index, keep) in [true, false, true].into_iter().enumerate() {
            let snapshot = selected.clone();
            selected.choices[index] = Some(keep);
            let tx = db.transaction().unwrap();
            record_choice(&tx, &snapshot, &selected).unwrap();
            tx.commit().unwrap();
        }
        let row = selected.row(true).unwrap();
        let work = decode_pending(row.clone()).unwrap();
        assert_eq!(
            work.originals,
            vec![before.originals[0].clone(), before.originals[2].clone()]
        );
        let item: MediaItem = serde_json::from_str(&work.item_meta).unwrap();
        assert_eq!(item.source_id, "123");
        assert_eq!(item.page_count, 2);
        assert_eq!(item.images[1].url, "https://example.org/2.png");
        assert!(matches!(
            claim_similar_pending(&db, before.token).unwrap(),
            PendingClaim::Claimed(_)
        ));
        assert!(matches!(
            claim_similar_pending(&db, before.token).unwrap(),
            PendingClaim::Publishing
        ));
        db.execute(
            "UPDATE image_review_sessions SET state='processing' WHERE token=?1",
            [before.token],
        )
        .unwrap();
        let publication = GalleryPublication {
            chat_id: -100123,
            message_ids: vec![101, 102],
            publish_state: GalleryPublishState::Full,
        };
        complete_and_record_action(
            &mut db,
            before.token,
            "publish_archive",
            Some(&row),
            Some(&publication),
        )
        .unwrap();
        let UndoClaim::Claimed(action) = claim_latest_undo(&mut db).unwrap() else {
            panic!("last image must be undoable");
        };
        assert_eq!(action.action, "image_review");
        let snapshot: Session = serde_json::from_str(&action.row.5).unwrap();
        assert_eq!(snapshot.choices, vec![Some(true), Some(false), None]);
        assert_eq!(snapshot.originals, before.originals);
        assert_eq!(
            decode_undo_publication(&action.publication),
            Some(publication)
        );
        assert!(matches!(
            claim_latest_undo(&mut db).unwrap(),
            UndoClaim::Empty
        ));
        // Restore the stored state directly, without deduplication or reindexing the source.
        insert_pending(&db, &snapshot).unwrap();
        assert_eq!(
            decode_pending(snapshot.row(false).unwrap())
                .unwrap()
                .originals
                .len(),
            3
        );
    }

    #[test]
    fn image_buttons_are_direct_and_old_deletion_is_separate() {
        let candidate = serde_json::to_value(keyboard(50, 1, None)).unwrap();
        assert_eq!(
            candidate["inline_keyboard"][0][0]["callback_data"],
            "image:50:keep:1"
        );
        assert_eq!(
            candidate["inline_keyboard"][0][1]["callback_data"],
            "image:50:discard:1"
        );
        let old = serde_json::to_value(old_keyboard(50, 1, false)).unwrap();
        assert_eq!(
            old["inline_keyboard"][0][0]["callback_data"],
            "image:50:delete:1"
        );
        assert!(!candidate.to_string().contains("confirm"));
        assert!(!old.to_string().contains("confirm"));
    }
    #[tokio::test]
    async fn publish_selected_pages_then_undo_restores_the_unfiltered_review() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let server_stop = stop.clone();
        let server_calls = calls.clone();
        let server = std::thread::spawn(move || {
            while !server_stop.load(Ordering::SeqCst) {
                let (mut socket, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut raw = Vec::new();
                let mut buf = [0; 8192];
                let end = loop {
                    let count = socket.read(&mut buf).unwrap();
                    assert!(count > 0);
                    raw.extend_from_slice(&buf[..count]);
                    if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let header = String::from_utf8_lossy(&raw[..end]);
                let route = header
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_string();
                let length = header
                    .lines()
                    .find_map(|line| {
                        line.to_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                let chunked = header.to_lowercase().contains("transfer-encoding: chunked");
                while raw.len() < end + length || (chunked && !raw[end..].ends_with(b"0\r\n\r\n")) {
                    let count = socket.read(&mut buf).unwrap_or_else(|e| panic!("mock request {route}: {e}; received={} header={end} content={length} chunked={chunked}",raw.len()));
                    if count == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..count]);
                }
                server_calls.lock().unwrap().push(route.clone());
                let result = if route.starts_with("/api/") {
                    serde_json::json!({"ok":true})
                } else if route.ends_with("DeleteMessage")
                    || route.ends_with("deleteMessage")
                    || route.to_lowercase().ends_with("answercallbackquery")
                {
                    serde_json::json!({"ok":true,"result":true})
                } else {
                    serde_json::json!({"ok":true,"result":{"message_id":101,"date":1,"chat":{"id":-100123,"type":"channel"},"text":"test"}})
                };
                let body = result.to_string();
                write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("hanabi.db");
        let mut sink = TelegramSink::new(
            "123:fake".into(),
            "123".into(),
            "-100123".into(),
            db_path.to_str().unwrap(),
            Some(GalleryClient::new(base.clone(), "fake".into()).unwrap()),
        )
        .unwrap();
        let inner = Arc::get_mut(&mut sink.state).unwrap();
        inner.bot = inner
            .bot
            .clone()
            .set_api_url(reqwest::Url::parse(&base).unwrap());
        let state = sink.state();
        let mut review = session();
        review.choices = vec![None; 2];
        review.messages = vec![10, 11];
        review.originals = (0..2)
            .map(|i| dir.path().join(format!("{i}.png")))
            .collect();
        for path in &review.originals {
            image::RgbImage::from_fn(4, 4, |x, y| {
                image::Rgb([(x * 60) as u8, (y * 60) as u8, 80])
            })
            .save(path)
            .unwrap();
        }
        review.prepared = review.originals.clone();
        {
            let db = state.db.lock().await;
            db.execute(
                "INSERT INTO image_review_sessions(token,payload) VALUES(?1,?2)",
                rusqlite::params![review.token, serde_json::to_string(&review).unwrap()],
            )
            .unwrap();
            insert_pending(&db, &review).unwrap();
        }
        for (index, decision) in ["keep", "discard"].into_iter().enumerate() {
            let guard = state.review_gate.clone().lock_owned().await;
            apply_choice(&state, 50, decision, index, guard)
                .await
                .unwrap();
            drop(state.review_gate.lock().await);
        }
        let action = {
            let mut db = state.db.lock().await;
            let UndoClaim::Claimed(action) = claim_latest_undo(&mut db).unwrap() else {
                panic!("publication must be undoable")
            };
            action
        };
        assert_eq!(
            decode_undo_publication(&action.publication)
                .unwrap()
                .message_ids,
            vec![101]
        );
        undo(&state, &action).await.unwrap();
        let (restored, status) = {
            let db = state.db.lock().await;
            load(&db, 50).unwrap().unwrap()
        };
        assert_eq!(status, "pending");
        assert_eq!(restored.choices, vec![Some(true), None]);
        assert!(restored.originals.iter().all(|p| p.is_file()));
        let paths = calls.lock().unwrap().clone();
        assert_eq!(
            paths
                .iter()
                .filter(|p| p.to_lowercase().ends_with("sendphoto"))
                .count(),
            1
        );
        assert_eq!(
            paths.iter().filter(|p| p.as_str() == "/api/ingest").count(),
            1
        );
        assert_eq!(
            paths
                .iter()
                .filter(|p| p.as_str() == "/api/catalog/retract")
                .count(),
            1
        );
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
    }
}
