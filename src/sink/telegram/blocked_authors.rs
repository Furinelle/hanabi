use super::*;
use crate::model::SourceKind;
use crate::store::{BlockedAuthor, Store};

const PAGE_SIZE: usize = 8;

/// 排除作者时顺带在账号侧移除：Pixiv 取消关注、X 移出 List。由 tools/remove_author.py 执行。
#[derive(Clone)]
pub struct AuthorRemoval {
    pub python_command: String,
    pub helper_path: String,
    /// 抓取用的 gallery-dl 配置，Pixiv 取消关注用它的 refresh-token。
    pub pixiv_config: String,
    /// List 所有者的 gallery-dl 配置；为空时不动 X List。
    pub x_owner_config: String,
    pub x_lists: Vec<String>,
}

/// 把待审作品的作者记入排除表，返回该作品和作者其余仍待审的 token。
/// 作品缺作者链接（旧记录无 item_meta 等）时返回 None。
pub(super) async fn block(
    state: &ReviewState,
    item_meta: &str,
    token: i64,
) -> Result<Option<(MediaItem, Vec<i64>)>> {
    let Some(item) = serde_json::from_str::<MediaItem>(item_meta)
        .ok()
        .filter(|item| !item.author.url.is_empty())
    else {
        return Ok(None);
    };
    state.store.lock().await.block_author(&item)?;
    let db = state.db.lock().await;
    let mut statement = db.prepare(
        "SELECT token FROM pending
         WHERE state='pending' AND token<>?1
           AND json_extract(item_meta, '$.source')=?2
           AND json_extract(item_meta, '$.author.url')=?3
         ORDER BY token",
    )?;
    let tokens = statement
        .query_map(
            rusqlite::params![token, item.source.as_str(), item.author.url],
            |row| row.get(0),
        )?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Some((item, tokens)))
}

fn run_helper(removal: &AuthorRemoval, args: &[String]) -> Result<bool> {
    let output = std::process::Command::new(&removal.python_command)
        .arg("-I")
        .arg(&removal.helper_path)
        .args(args)
        .output()
        .with_context(|| format!("启动 {} 失败", removal.helper_path))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{}", stderr.lines().last().unwrap_or("未知错误"));
    }
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    Ok(reply["removed"].as_bool().unwrap_or(false))
}

/// 排除后在来源侧停掉该作者，只在真的移除了或移除失败时发私聊通知。
pub(super) async fn remove_from_source(state: Arc<ReviewState>, item: MediaItem) {
    let name = &item.author.name;
    let id = item
        .author
        .url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("");
    let mut notes = Vec::new();
    match item.source {
        SourceKind::Douyin => {
            match state
                .store
                .lock()
                .await
                .unsubscribe_douyin_profile(&item.author.url)
            {
                Ok(true) => notes.push(format!("🚫 已停止监控抖音作者 {name}")),
                Ok(false) => {}
                Err(e) => notes.push(format!("⚠️ 停止监控抖音作者 {name} 失败：{e}")),
            }
        }
        SourceKind::Pixiv | SourceKind::X => {
            let Some(removal) = state.author_removal.clone() else {
                return;
            };
            let jobs: Vec<(Vec<String>, String)> = if item.source == SourceKind::Pixiv {
                vec![(
                    vec![removal.pixiv_config.clone(), "pixiv".into(), id.into()],
                    format!("在 Pixiv 取消关注 {name}"),
                )]
            } else if removal.x_owner_config.is_empty() {
                vec![]
            } else {
                removal
                    .x_lists
                    .iter()
                    .map(|list| {
                        (
                            vec![
                                removal.x_owner_config.clone(),
                                "x".into(),
                                list.clone(),
                                id.into(),
                            ],
                            format!("把 @{id} 移出 X List {list}"),
                        )
                    })
                    .collect()
            };
            for (args, what) in jobs {
                let removal = removal.clone();
                let result = tokio::task::spawn_blocking(move || run_helper(&removal, &args))
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result);
                match result {
                    Ok(true) => notes.push(format!("🚫 已{what}")),
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "排除作者后自动移除失败");
                        notes.push(format!(
                            "⚠️ {what}失败：{e}\n作者仍在 Hanabi 排除列表，不会再送审"
                        ));
                    }
                }
            }
        }
    }
    if !notes.is_empty() {
        let _ = state
            .bot
            .send_message(state.review_chat.clone(), notes.join("\n"))
            .await;
    }
}

fn author_label(author: &BlockedAuthor) -> String {
    let source = match author.source.as_str() {
        "pixiv" => "Pixiv",
        "x" => "X",
        "douyin" => "抖音",
        other => other,
    };
    let name = author
        .display_name
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let name = if name.is_empty() {
        author
            .author_url
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string()
    } else {
        name
    };
    format!("{source} · {}", name.chars().take(40).collect::<String>())
}

fn page(store: &Store, requested: usize) -> Result<(String, InlineKeyboardMarkup)> {
    let authors = store.list_blocked_authors()?;
    let pages = authors.len().div_ceil(PAGE_SIZE).max(1);
    let page = requested.min(pages - 1);
    let mut rows = Vec::new();
    for author in authors.iter().skip(page * PAGE_SIZE).take(PAGE_SIZE) {
        rows.push(vec![
            InlineKeyboardButton::url(
                author_label(author),
                reqwest::Url::parse(&author.author_url)?,
            ),
            InlineKeyboardButton::callback("✅ 解除", format!("bl:remove:{}:{page}", author.id)),
        ]);
    }
    let mut navigation = Vec::new();
    if page > 0 {
        navigation.push(InlineKeyboardButton::callback(
            "上一页",
            format!("bl:list:{}", page - 1),
        ));
    }
    navigation.push(InlineKeyboardButton::callback(
        "刷新",
        format!("bl:list:{page}"),
    ));
    if page + 1 < pages {
        navigation.push(InlineKeyboardButton::callback(
            "下一页",
            format!("bl:list:{}", page + 1),
        ));
    }
    rows.push(navigation);
    let text = format!(
        "已排除作者：{} 位\n第 {}/{} 页\n\n这些作者的作品不再进入定时抓取的审批。点名字打开主页，点 ✅ 解除恢复。\n排除时已丢弃的作品不会重新送审；手动发送的链接不受影响。",
        authors.len(), page + 1, pages
    );
    Ok((text, InlineKeyboardMarkup::new(rows)))
}

pub(super) async fn send(state: &ReviewState) -> Result<()> {
    let (text, keyboard) = page(&*state.store.lock().await, 0)?;
    state
        .bot
        .send_message(state.review_chat.clone(), text)
        .reply_markup(keyboard)
        .await?;
    Ok(())
}

pub(super) async fn callback(state: &ReviewState, q: CallbackQuery) -> Result<()> {
    // 与 /douyin 相同：只认审批人本人的私聊。
    if state.owner <= 0
        || q.from.id.0 != state.owner as u64
        || q.message
            .as_ref()
            .is_none_or(|m| m.chat().id.0 != state.owner)
    {
        state.bot.answer_callback_query(q.id).text("无权限").await?;
        return Ok(());
    }
    let data = q.data.as_deref().unwrap_or("");
    let parts: Vec<_> = data.split(':').collect();
    let (requested, removed) = {
        let store = state.store.lock().await;
        match parts.as_slice() {
            ["bl", "list", page] => (page.parse::<usize>().ok(), None),
            ["bl", "remove", id, page] => match (id.parse::<i64>(), page.parse::<usize>()) {
                (Ok(id), Ok(page)) => (Some(page), Some(store.unblock_author(id)?)),
                _ => (None, None),
            },
            _ => (None, None),
        }
    };
    let Some(requested) = requested else {
        state
            .bot
            .answer_callback_query(q.id)
            .text("无效操作")
            .await?;
        return Ok(());
    };
    state
        .bot
        .answer_callback_query(q.id)
        .text(match removed {
            Some(true) => "已解除排除，下轮抓取恢复送审",
            Some(false) => "该作者已不在排除列表",
            None => "已刷新",
        })
        .await?;
    let (text, keyboard) = page(&*state.store.lock().await, requested)?;
    let message = q.message.unwrap();
    if let Err(error) = state
        .bot
        .edit_message_text(message.chat().id, message.id(), text)
        .reply_markup(keyboard)
        .await
    {
        if !error.to_string().contains("message is not modified") {
            return Err(error.into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Author, SourceKind};

    #[test]
    fn page_labels_source_and_falls_back_to_profile_id() {
        let store = Store::open_in_memory().unwrap();
        assert!(page(&store, usize::MAX)
            .unwrap()
            .0
            .contains("0 位\n第 1/1 页"));
        let item = |source, name: &str, url: &str| MediaItem {
            source,
            source_id: "1".into(),
            author: Author {
                name: name.into(),
                url: url.into(),
            },
            title: None,
            url: String::new(),
            tags: vec![],
            bookmark_count: None,
            is_r18: false,
            pixiv_type: None,
            page_count: 1,
            images: vec![],
            origin: String::new(),
        };
        store
            .block_author(&item(SourceKind::X, "高产 画师", "https://x.com/a"))
            .unwrap();
        store
            .block_author(&item(
                SourceKind::Pixiv,
                "",
                "https://www.pixiv.net/users/7",
            ))
            .unwrap();
        let id = store.list_blocked_authors().unwrap()[0].id;
        let (text, keyboard) = page(&store, 0).unwrap();
        assert!(text.contains("2 位"));
        let rows = serde_json::to_value(keyboard).unwrap()["inline_keyboard"].clone();
        assert_eq!(rows[0][0]["text"], "X · 高产 画师");
        assert_eq!(rows[0][1]["callback_data"], format!("bl:remove:{id}:0"));
        assert_eq!(rows[1][0]["text"], "Pixiv · 7");
        assert_eq!(rows[1][0]["url"], "https://www.pixiv.net/users/7");
    }
}
