# Hanabi 改进升级设计（阶段一 + 阶段二）

> 日期：2026-06-25 · 状态：待实施
> 来源：对整库代码审查 + 同类项目（pixivdaily-rust / Pixiv_bot / WetPics / gallery-dl）调研后的改进方案。
> 本 spec 覆盖**阶段一（稳定性与质量）**与**阶段二（交付能力）**；阶段三仅列总路线占位，各自独立立项。

## 目标

在不破坏现有「人工审批制」架构（`Source → FilterChain → TelegramSink`，sqlite 去重，抓取循环与 `run_review_loop` 并发）的前提下：

1. 修掉一批已定位的稳定性/质量缺陷（并发锁、静默失败、资源泄漏、死依赖、CI 短板）。
2. 增强交付能力：**原画质图发到频道帖的评论区**、收紧手动链接的爆炸半径、提供 **Docker/GHCR 镜像**。

非目标（本轮不做）：R18 独立频道路由（保持现状，所有帖子进唯一主频道）、ugoira→mp4、booru 源、AI 初筛、Web 面板、pHash 去重——后四项归阶段三。

---

## 阶段一 · 稳定性与质量

纯修复 + 清理，除新增告警外不改用户可见行为。

| # | 改动 | 位置 | 做法 |
|---|---|---|---|
| 1 | sqlite 加锁 | `src/store.rs` `Store::open` | 加 `PRAGMA busy_timeout=5000;` + `PRAGMA journal_mode=WAL;`，与 sink 端连接对齐，消除两连接并发写 `hanabi.db` 的 "database is locked" 风险 |
| 2 | owner 解析告警 | `src/sink/telegram.rs` `TelegramSink::new`（`review_chat_id.parse().unwrap_or(0)`） | 解析失败时 `tracing::error!` 明确报错：命令/链接功能要求 `channel_id` 为数字 id，否则将静默忽略所有命令 |
| 3 | 周期清理 | `run_review_loop` | `cleanup_stale` 除启动调用外，主轮询循环每 6h 触发一次（用计时变量判断），常驻不重启实例也能清过期 pending 与孤儿目录 |
| 4 | 缩放保 alpha | `src/sink/telegram.rs` `prepare`（`to_rgb8().save(".scaled.jpg")`） | 超限图缩放后按**原扩展名/格式**保存，保留 PNG 透明通道；仅必要时落地缩放副本 |
| 5 | 删死依赖 | `Cargo.toml` | 移除 `thiserror`（全库零引用，已确认） |
| 6 | handle_link 异步化 | `src/main.rs` `tokio::select!` 的 `link_rx` 分支 | 把 probe+download+publish `tokio::spawn` 出去，慢链接不再阻塞定时槽与 `/run` |
| 7 | clippy 清零 | `src/sink/telegram.rs:336` / `:98` | `map_or(false, …)` → `is_some_and(…)`；`trust_dns(true)` **保留不动**，仅加 `#[allow(deprecated)]`——这是 musl 静态二进制 DNS 解析的命脉，不为消 lint 冒险换 `hickory_dns` |
| 8 | 时区配置化 | `src/main.rs` `secs_until_next_slot`（硬编码 `+8*3600`） | 顶层配置新增 `tz_offset_hours: i64`（默认 8），时间槽计算读配置而非写死 CST |
| 9 | CI 强化 | `.github/workflows/build.yml`（或新增 `ci.yml`） | 新增 `cargo clippy -D warnings` + `cargo fmt --check` job，并在 push/PR 触发；Release 增加 `aarch64-unknown-linux-musl` 产物 |

### 阶段一验收
- `cargo test` 全绿、`cargo clippy` 零告警、`cargo fmt --check` 通过。
- 配 `channel_id = "@xxx"`（非数字）启动时能看到 error 级日志。
- 配 `tz_offset_hours` 改变后，日志里「下次抓取」时间随之偏移。

---

## 阶段二 · 交付能力

### 2.1 原图进评论区（核心，最高风险项）

**目标**：频道帖照常发压缩 photo（保留相册大图预览），同时把**原画质 document** 发进该帖的评论区。

**原理**：频道帖绑定讨论组后，Telegram 自动把帖子转发到讨论组形成评论锚点（`message.is_automatic_forward == true`）。bot 捕获这条 auto-forward，以 `reply_to_message_id` 把原图 document 回复上去，即出现在帖子评论区。

**前置条件**（已确认满足）：`publish_channel` 已绑讨论组、bot 是该讨论组管理员、`get_updates` 的 `allowed_updates` 含 `Message`。

**数据结构改动**
- `pending` 表**新增 `originals TEXT` 列**（JSON 数组，存原始下载文件路径）。现存 `files` 列继续存缩放后的 `prepared`（发 photo 用）；多数图未超限时两者相同。
- 旧库兼容：`ALTER TABLE pending ADD COLUMN originals TEXT`（已存在则忽略错误），同 `created_at` 的兼容写法。
- 进程内新增映射 `PendingComments: Map<i32(频道帖首条 msg_id), CommentJob{ originals: Vec<PathBuf>, temp_dir: PathBuf, created_at: i64 }>`，由 `ReviewState` 持有（`Mutex`/`tokio::sync::Mutex`）。

**流程（发布到频道时触发——`handle_callback` ok 分支与 `publish_direct` 共用）**
1. 照常 `send_group(photo)` 发频道，拿到频道帖首条 `message_id`。
2. 把 `(首条 msg_id → CommentJob{originals, temp_dir, now})` 存入 `PendingComments`。**不立即 cleanup**。
3. `run_review_loop` 收到讨论组里 `is_automatic_forward` 的消息时：取 `forward_origin`（指向 `publish_channel`）与被转发的 `message_id`，在 `PendingComments` 中查首条 msg_id；命中则 `sendMediaGroup(InputMediaDocument)`（`reply_to_message_id = 该 auto-forward 消息 id`）把 `originals` 整组发进评论区，成功后 `cleanup(temp_dir)` 并移除映射。
4. **兜底超时**：每轮清理时扫描 `PendingComments`，对 `now - created_at > 120s` 仍未配对的条目，直接 `cleanup` 并移除（兼容「频道没绑讨论组 / auto-forward 丢失」的情况，避免临时文件泄漏）——也作为 R18 不分流后单频道场景的安全网。

**纯函数拆分（便于单测）**
- `match_auto_forward(msg, publish_channel) -> Option<i32(被转发首条 msg_id)>`：从 auto-forward 消息里解析出对应频道帖 msg_id。真机先抓一条样本校准字段（`forward_origin` 的 teloxide 0.13 结构）。
- 发送 document 组、清理为 IO，留集成/真机验证。

**风险**：auto-forward 异步到达且字段形状需真机确认。实施第一步必须用真实频道抓一条 auto-forward 的 `Update` JSON 做 fixture，再写 `match_auto_forward`——同当初校准 gallery-dl `-j` 输出的做法。

### 2.2 手动链接收紧

落实「单作品直发、多作品进审批」。

- `extract_supported_url` 改用 **host 判定**（解析 URL host 是否属于 `pixiv.net`/`x.com`/`twitter.com`），杜绝 `evil.com/pixiv.net` 子串误判。
- 新增 `classify_link(url) -> LinkKind`：
  - `Single`：`pixiv.net/artworks/\d+`、`pixiv.net/i/\d+`、`(x|twitter).com/<user>/status/\d+` → 直发频道（现状保留，含已发去重）。
  - `Multi`：主页 `users/\d+`、`ranking.php`、`i/lists/\d+` 等受支持域名的非单作品 URL → 下载后逐个 `sink.deliver` 进**审批私聊**；「抓取中」提示回执改为「已转 N 个进审批」。
  - 不支持域名 → 忽略。
- `handle_link`（`src/main.rs`）据 `classify_link` 分流；`probe_range` 对 Multi 仍用配置值。
- 纯函数 `classify_link` 单测（各类 URL 样例）。

### 2.3 Docker / GHCR 镜像

对标 pixivdaily-rust 的 GHCR 分发，部署从「装 rust+pipx」降到 `docker run`。

- 新增 `Dockerfile`（multi-stage）：builder 阶段用 rust 编 `x86_64-unknown-linux-musl` 静态二进制；runtime 阶段 `alpine` + 拷入二进制 + 安装 `gallery-dl`（`pip install --break-system-packages gallery-dl` 或 apk）。
- 运行约定：`config.toml` / `gallery-dl.conf` 经 volume 挂载到 `WORKDIR`，`HANABI_BOT_TOKEN` 经 `-e` 注入，`HANABI_CONFIG` 指向挂载路径。
- CI：打 `v*` tag 时 `docker build` 并 push `ghcr.io/furinelle/hanabi:<tag>` 与 `:latest`（`packages: write` 权限；GHCR 镜像名须小写）。
- README 增「Docker 部署」小节。

### 阶段二验收
- 真机：审批通过后，频道出现压缩大图，**评论区**出现整组原画质文件（多图为一条 media-group document）。
- 频道未绑讨论组（或 120s 未等到 auto-forward）时，photo 正常发出、临时文件被兜底清理、无残留。
- 发画师主页/榜单链接 → 作品逐个进审批私聊；发单作品链接 → 直发频道。
- `docker run` 可起来并正常抓取/审批/发布。

---

## 阶段三 · 占位（本轮不细化）

各自独立立项，按需 brainstorm：

- **booru 源**：`SourceKind::Booru` + `parse_booru`，`rating` q/e→`is_r18`、`score`→打分阈值（`min_score`）、`tags` 映射；侵入 `model`/`filter`/`caption`，单独 PR。
- **失败投递持久重试队列**：交付失败从「下轮重试」升级为显式持久队列。
- **pHash 跨源去重**：同图 pixiv 与 X 转载的感知哈希去重。
- **AI 美学初筛**：用视觉模型对进审批的图打分/粗筛，降低人工审批量——强化「人工审批」这一差异点。
- **Web 仪表盘 / 统计**。

---

## 实施顺序建议

1. 阶段一（9 项，一个 PR，纯修复，先落地稳住）。
2. 阶段二 2.2 手动链接收紧（小、独立）。
3. 阶段二 2.3 Docker/GHCR（独立、不碰业务逻辑）。
4. 阶段二 2.1 原图进评论区（最大、最高风险，单独 PR，先真机校准 auto-forward 字段）。

## 测试策略

- 纯函数单测：`tz_offset` 时间槽、`classify_link`、`match_auto_forward`（fixture 来自真机 auto-forward）、（阶段三）`parse_booru`。
- 现有 20+ 单测 + 集成测试保持绿。
- 真机验证清单：缩放保 alpha 的 IO、Telegram 网络发送、评论区 auto-forward 实际字段、Docker 启动链路。
