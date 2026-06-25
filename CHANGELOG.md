# 更新日志

本项目所有重要变更记录于此。格式参考 [Keep a Changelog](https://keepachangelog.com/)。

## [0.4.0] - 2026-06-25

### 新增
- **原画质进评论区**：审批通过 / 手动直发后，频道帖照常发压缩大图，**原画质图作为 document 自动投递到该帖评论区**（需频道绑定讨论组、bot 为管理员）；频道未绑讨论组或 120s 未等到自动转发则兜底清理临时文件，不泄漏。
- **Docker / GHCR 镜像**：多阶段 Dockerfile（alpine musl 静态 + 内置 gallery-dl），打 `v*` tag 自动构建并推送 `ghcr.io/furinelle/hanabi`；README 增 Docker 部署小节。
- **时区可配置**：`tz_offset_hours`（默认 8），整点时间槽不再硬编码 CST。

### 变更
- **手动链接分流**：单作品链接（`artworks/` `status/`）直发频道；画师主页 / 榜单 / list 等多作品链接改走**审批流**逐个过按钮。链接识别改 **host 精确判定**，防 `evil.com/pixiv.net` 子串伪装。
- **缩放保原格式**：超限图缩放后保留原格式与 PNG alpha 通道，不再强转 RGB JPG。

### 修复
- **并发写锁**：Store 连接启用 WAL + `busy_timeout`，消除抓取循环与审批任务并发写 `hanabi.db` 的 "database is locked"。
- **owner 静默失败**：`channel_id` 配成非数字 id 时显式 `error` 告警（此前会静默忽略所有命令/链接）。
- **资源泄漏**：`cleanup_stale` 周期化（每 6h），常驻不重启实例也能清过期 pending 与孤儿临时目录。
- **慢链接阻塞调度**：`handle_link` 移入独立 task，慢链接不再卡定时槽与 `/run`。

### 其他
- 删除死依赖 `thiserror`；clippy 清零（`is_some_and` / `#[allow(deprecated)]` 保留 musl DNS 命脉 `trust_dns`）；CI 增 clippy/fmt lint job 与 push/PR 触发。

## [0.3.1] - 2026-06-10

### 变更
- **手动链接发布后清理私聊**：发作品链接 → 抓取并发布到频道成功后，自动删除「🔗 收到链接,抓取中…」提示**和你发的链接消息**，审批私聊保持干净；若没抓到可发的新图，则把提示改为结果说明(不删,便于知晓)。

## [0.3.0] - 2026-06-10

### 安全
- **命令/链接 owner 校验**：仅审批私聊本人能发命令（`/run` 等）和作品链接，陌生人一律忽略（此前 bot 公开，任何人都能触发抓取、往频道灌图）。

### 新增
- **临时文件自动清理**：bot 启动时清理超期（>7 天）未审 pending（删消息+文件+记录）及 `/tmp/hanabi_*` 孤儿目录（旧版本/重启遗留）。
- **手动链接去重**：发链接前查去重库，已发过的作品跳过，不重复进频道。
- **多实例检测**：`get_updates` 遇 getUpdates 冲突（另一实例在跑）时明确报错提示。
- **配置校验**：`poll_interval_secs` 不能整除 86400 时启动告警（整点时间槽会不均匀）。

### 重构
- 下载逻辑提取为共用 `download_work`（定时抓取 + 手动链接复用）。
- 缩放边长等魔法数字提为具名常量；新增 `extract_supported_url` 单元测试。

## [0.2.2] - 2026-06-10

### 修复
- **审批「该条已失效」**：pending 审批状态从内存 `HashMap` 改为 **sqlite 持久化**（hanabi.db 的 `pending` 表），bot 重启 / 崩溃后旧审批消息的按钮仍有效。
- **限流导致 pending 丢失**：`handle_callback` 改为「操作成功才删 pending」——发布失败（如 Telegram 限流 `Retry after`）时保留 pending，可稍后重点，不再永久失效。
- **限流自动重试**：所有发图/发消息请求遇 429 `RetryAfter` 自动等待后重试（最多 5 次）。

## [0.2.1] - 2026-06-08

### 新增
- **手动链接直发**：私聊给 bot 发 Pixiv / X 作品链接 → 自动 probe + 解析 + 下载 → **直接发布到频道**（跳过审批，手动发=已选定），caption 与自动抓取同格式；发布后写入去重库（自动抓取不会再重复发）。

## [0.2.0] - 2026-06-08

### 新增
- **人工审批制**：抓到的作品先发到审批私聊，附 `✅ 发送到频道` / `❌ 丢弃` 按钮；批准后全套图发布到频道并删除私聊审批消息，丢弃则仅删除。
- **多图整组审批**：多图作品在审批私聊以图组（media group）完整显示，紧跟一条带按钮的控制消息。
- **Slash 命令**：`/run`（立即手动抓取一轮）、`/status`（待审数+运行状态）、`/ping`、`/help`。
- **手动触发通道**：`/run` 经 mpsc 通知抓取循环立即跑一轮（`tokio::select!` 定时器 / 触发二选一）。
- **VPS 部署**：`deploy/hanabi.service`（systemd unit），支持云端常驻。

### 变更
- **caption 格式**：改为 `Title: / Tag: / From <Pixiv|X>(作品链接) By 作者(作者链接)`，敏感内容加 🔞 标记。
- **R18 策略**：R18 / sensitive 不再自动过滤，改为进审批由人工决定（各源 `r18 = true` 放行）。
- **关注画师源**：不再按标签筛选，所有关注画师新作都进审批；排行榜源保持按标签白名单筛选。
- **抓取频率**：一天三次（`poll_interval_secs = 28800`）。
- **网络可靠性**：`get_updates` 改用短轮询（`timeout=0` + 空结果 sleep），规避经代理时长轮询连接被掐断导致按钮无响应。

### 修复
- **X 敏感内容漏过滤**：`parse_twitter` 的 `is_r18` 此前写死 `false`，导致 X 的 `sensitive` 推文绕过过滤；现改为读取 `sensitive` 字段。

## [0.1.0] - 2026-06-08

### 新增
- 初始版本：Pixiv（画师 / 收藏 / 榜单）+ X（List / 为你推荐）抓取后端（gallery-dl 子进程）。
- `Source` → `FilterChain` → `TelegramSink` 模块化管线，sqlite 去重（幂等），tokio 定时轮询。
- 两阶段抓取：`probe`（`-j` 拉元数据过滤）→ `download`（只下通过的作品）。
- 分源过滤：R18 / 收藏数 / 点赞数 / 标签白名单 / 只插画 / 页数上限 / 媒体必需。
- Telegram 相册推送，超限图片自动缩放（>10MB 或宽+高>10000）。
