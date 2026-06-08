# 更新日志

本项目所有重要变更记录于此。格式参考 [Keep a Changelog](https://keepachangelog.com/)。

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
