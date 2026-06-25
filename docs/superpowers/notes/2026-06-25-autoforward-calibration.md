# auto-forward 字段校准（teloxide 0.13 / teloxide-core 0.10.1）

> 本应是真机 spike，实际从 `~/.cargo/registry/.../teloxide-core-0.10.1/src` 源码确认，
> 真机仅作最后验证。

## 结论

频道帖自动转发到绑定讨论组后，讨论组里那条 `Message`：

- `msg.is_automatic_forward() -> bool`：true。
- `msg.forward_origin() -> Option<&MessageOrigin>`：`Some(MessageOrigin::Channel { chat, message_id, author_signature, date })`。
- ⚠️ **便捷访问器 `forward_from_chat()` 只匹配 `MessageOrigin::Chat` 变体，对 `Channel` 返回 None**。
  频道来源必须直接 `match` `MessageOrigin::Channel { chat, message_id, .. }` 取 `.chat` 与 `.message_id`。
- `MessageId(pub i32)`，`.0` 取整数；序列化形状为 `{"message_id": n}`。

## forward_origin JSON 形状（channel）

`MessageOrigin` 为 `#[serde(tag = "type", rename_all = "snake_case")]`：

```json
"forward_origin": {
  "type": "channel",
  "date": 1700000000,
  "chat": { "id": -100…, "title": "…", "type": "channel", "username": "FurinaDeCanvas" },
  "message_id": 789
}
```

## 投递评论区用到的 API

- reply：`ReplyParameters::new(MessageId)`（Bot API 7.0），`send_media_group(...).reply_parameters(rp)`。
- 文档：`InputMedia::Document(InputMediaDocument::new(InputFile::file(path)))`。
- 投递目标 chat = 那条 auto-forward 消息的 `msg.chat.id`（讨论组），reply_to = `msg.id`。

## 真机验证清单

1. 审批通过 → 频道出压缩大图；讨论组评论区出现整组原画质 document。
2. 频道未绑讨论组 / 120s 未等到 auto-forward → 临时文件被兜底清理，无残留。
3. 多图作品：仅对「首条频道帖 msg_id」登记一次，避免 N 条 auto-forward 重复投递。
