use super::*;
use crate::store::{DouyinSubscription, Store};

const PAGE_SIZE: usize = 8;

fn author_label(author: &DouyinSubscription) -> String {
    if author.display_name.trim().is_empty() {
        let id = author
            .profile_url
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("");
        let suffix: String = id
            .chars()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("未缓存昵称 · {suffix}")
    } else {
        author
            .display_name
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(40)
            .collect()
    }
}

fn page(store: &Store, requested: usize) -> Result<(String, InlineKeyboardMarkup)> {
    let authors = store.list_douyin_monitors()?;
    let pages = authors.len().div_ceil(PAGE_SIZE).max(1);
    let page = requested.min(pages - 1);
    let mut rows = Vec::new();
    for author in authors.iter().skip(page * PAGE_SIZE).take(PAGE_SIZE) {
        rows.push(vec![
            InlineKeyboardButton::url(
                author_label(author),
                reqwest::Url::parse(&author.profile_url)?,
            ),
            InlineKeyboardButton::callback("❌ 删除", format!("dy:remove:{}:{page}", author.id)),
        ]);
    }
    let mut navigation = Vec::new();
    if page > 0 {
        navigation.push(InlineKeyboardButton::callback(
            "上一页",
            format!("dy:list:{}", page - 1),
        ));
    }
    navigation.push(InlineKeyboardButton::callback(
        "刷新",
        format!("dy:list:{page}"),
    ));
    if page + 1 < pages {
        navigation.push(InlineKeyboardButton::callback(
            "下一页",
            format!("dy:list:{}", page + 1),
        ));
    }
    rows.push(navigation);
    let text = format!(
        "抖音作者监控：{} 位\n第 {}/{} 页\n\n点昵称打开主页，点 ❌ 删除停止监控。\n删除不清理已有待审或已发布作品，已开始抓取的作者本轮可能继续，下轮停止。\n重新发送作者名片可恢复；累计作品链接不会自动恢复。",
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
    // Management always requires the owner's private chat, including malformed/group configs.
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
            ["dy", "list", page] => (page.parse::<usize>().ok(), None),
            ["dy", "remove", id, page] => match (id.parse::<i64>(), page.parse::<usize>()) {
                (Ok(id), Ok(page)) if id > 0 => (Some(page), Some(store.unsubscribe_douyin(id)?)),
                _ => (None, None),
            },
            _ => (None, None),
        }
    };
    let Some(requested) = requested else {
        state
            .bot
            .answer_callback_query(q.id)
            .text("无效监控操作")
            .await?;
        return Ok(());
    };
    state
        .bot
        .answer_callback_query(q.id)
        .text(match removed {
            Some(true) => "已删除作者监控",
            Some(false) => "该作者已不在监控中",
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
        // Refreshing an unchanged page is a successful no-op in Telegram.
        if !error.to_string().contains("message is not modified") {
            return Err(error.into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_use_stable_ids_and_handle_empty_or_deleted_last_page() {
        let store = Store::open_in_memory().unwrap();
        assert!(page(&store, usize::MAX)
            .unwrap()
            .0
            .contains("0 位\n第 1/1 页"));
        for i in 0..9 {
            let url = format!("https://www.douyin.com/user/MS4wTEST{i}");
            store.subscribe_douyin(&url, &url).unwrap();
            store
                .remember_douyin_author(&url, &format!("作者 {i}"))
                .unwrap();
        }
        let authors = store.list_douyin_monitors().unwrap();
        let (text, keyboard) = page(&store, 1).unwrap();
        assert!(text.contains("9 位\n第 2/2 页"));
        let buttons = serde_json::to_value(keyboard).unwrap();
        assert_eq!(
            buttons["inline_keyboard"][0][0]["url"],
            authors[8].profile_url
        );
        assert_eq!(
            buttons["inline_keyboard"][0][1]["callback_data"],
            format!("dy:remove:{}:1", authors[8].id)
        );
        store.unsubscribe_douyin(authors[8].id).unwrap();
        assert!(page(&store, 1).unwrap().0.contains("8 位\n第 1/1 页"));
        store.unsubscribe_douyin(authors[0].id).unwrap();
        assert_eq!(store.list_douyin_monitors().unwrap()[0].id, authors[1].id);
    }
}
