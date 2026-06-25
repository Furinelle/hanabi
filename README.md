# Hanabi 🎆

<p align="center">
  <img src="docs/cover.png" alt="cover" width="400">
  <br>
  <sub>Illustration: <a href="https://www.pixiv.net/artworks/141191404">pixiv #141191404</a></sub>
</p>

二次元图片 Telegram 推送 bot，带**人工审批制**。从 Pixiv（关注画师新作 / 排行榜）和 X（List）抓取插画，先发到你的私聊等审批，你点按钮决定是否发布到频道。

## 特性

- 🔍 **多源抓取**：Pixiv 关注画师新作 + Pixiv 周榜（按标签）+ X List
- 🖼️ **人工审批**：作品先发审批私聊（单图直发 / 多图整组 + 控制消息），附 `✅ 发送到频道` `❌ 丢弃` 按钮；点击后整组消息自动删除
- 🪄 **原画质进评论区**：批准/直发后，频道帖发压缩大图，**原画质图作为文件自动投递到该帖评论区**（需频道绑定讨论组、bot 为管理员）；未绑定时自动降级，120s 兜底清理
- ♻️ **自动去重**：sqlite 记录，进过审批的作品永不重复出现
- 🔞 **R18 标记**：敏感内容（Pixiv `x_restrict` / X `sensitive`）进审批并标 🔞，由你人工决定
- 🎯 **分源过滤**：R18 / 收藏数 / 点赞数 / 标签白名单 / 只插画 / 页数上限
- ⌨️ **命令控制**：`/run` 手动抓一轮、`/status`、`/ping`、`/help`
- ⏰ **定时轮询**：`poll_interval_secs` 可配（如一天三次 = 28800），`tz_offset_hours` 可配时区（默认 +8）
- 🐳 **多种部署**：systemd / launchd / Docker（GHCR 镜像，含 gallery-dl）；预编译 x86_64 + aarch64 musl 静态二进制

## 前置依赖

- Rust（`cargo build --release`）
- gallery-dl：`pipx install gallery-dl`
- 一个 Telegram bot（@BotFather 创建，拿 token）

## 认证（`gallery-dl.conf`）

复制 `gallery-dl.conf.example` 为 `gallery-dl.conf`（`chmod 600`，已 gitignore），填入：

- **Pixiv** `extractor.pixiv.refresh-token`：OAuth PKCE 流程获取（浏览器登录授权 → 从回调 URL 的 `code=` 换取 refresh_token）
- **X** `extractor.twitter.cookies`：浏览器（建议小号）登录后从 DevTools → Cookies 复制 `auth_token` 和 `ct0`

> X 还设了 `"videos": false`、`"retweets": false`（只要图片原作）和 `"size": "orig"`（4K 原画质）。

## 配置（`config.toml`）

复制 `config.example.toml` 为 `config.toml`（已 gitignore），关键字段：

```toml
poll_interval_secs = 28800   # 一天三次
tz_offset_hours = 8          # 整点时间槽所用时区（默认 +8，可选）

[telegram]
channel_id = "7794592020"           # 审批私聊（作品先发这里）
publish_channel = "@FurinaDeCanvas" # 批准后发布的频道（绑定讨论组后，原图自动进帖子评论区）

[gallery_dl]
probe_range = "1-50"

# 关注画师新作（不筛标签，全进审批）
[[source]]
name = "following_new"
kind = "pixiv_user"
targets = ["https://www.pixiv.net/bookmark_new_illust.php"]
filters = { r18 = true, illust_only = true, max_pages = 5 }

# 周榜（按标签白名单筛选）
[[source]]
name = "pixiv_ranking_tagged"
kind = "pixiv_ranking"
targets = ["https://www.pixiv.net/ranking.php?mode=weekly&content=illust"]
filters = { r18 = true, illust_only = true, max_pages = 5, min_bookmarks = 2000, tags = ["フリーナ", "原神", "..."] }

# X List
[[source]]
name = "x_artists_list"
kind = "x_list"
targets = ["https://x.com/i/lists/<id>"]
filters = { r18 = true, min_likes = 50 }
```

> `bot_token` **不进**配置文件，走环境变量 `HANABI_BOT_TOKEN`。

## 运行

```bash
export HANABI_BOT_TOKEN="<bot token>"
cargo run --release
```

bot 启动后：抓取循环按 `poll_interval_secs` 定时跑，审批回调任务并发监听按钮/命令。

## 命令（私聊 bot 发送）

| 命令 | 作用 |
|------|------|
| `/run` | 立即手动抓取一轮 |
| `/status` | 待审数 + 运行状态 |
| `/ping` | 存活测试 |
| `/help` | 命令列表 |

> 💡 **直接发链接**给 bot（host 精确识别 Pixiv/X，防伪装域名）：
> - **单作品链接**（`artworks/<id>`、`status/<id>`）→ 跳过审批**直发频道**（手动=已选定）。
> - **多作品链接**（画师主页 / 榜单 / list）→ 逐个**进审批私聊**过按钮，不直发。

## 审批流程

```
抓取 → 过滤/去重 → 下载 → 发审批私聊（图 + caption + 按钮）
                                    │
                        ┌───────────┴───────────┐
                   ✅ 发送到频道             ❌ 丢弃
                        │                       │
              全套图发布到频道              （不发布）
                        └───────────┬───────────┘
                            删除私聊审批消息 + 清理临时文件
```

caption 格式：
```
🔞 R18              （仅敏感内容）
Title: 标题
Tag: #标签 #标签
From <Pixiv|X>(作品链接) By 作者名(作者链接)
```

## 部署

### Linux（systemd，VPS）

见 `deploy/hanabi.service`。将仓库 clone 到 VPS，装 rust + gallery-dl，传入 `gallery-dl.conf` / `config.toml`，配置 systemd service（`HANABI_BOT_TOKEN` 经 service 环境变量注入），`systemctl enable --now hanabi`。

### macOS（launchd）

`deploy/ai.hanabi.plist` → `~/Library/LaunchAgents/`，改占位后 `launchctl load`。

### Docker（GHCR 镜像）

镜像内含 gallery-dl，无需另装：

```bash
docker run -d --name hanabi \
  -e HANABI_BOT_TOKEN="<bot token>" \
  -v $PWD/config.toml:/data/config.toml:ro \
  -v $PWD/gallery-dl.conf:/data/gallery-dl.conf:ro \
  -v $PWD/hanabi.db:/data/hanabi.db \
  ghcr.io/furinelle/hanabi:latest
```

> `config.toml` 里 `gallery_dl.config_path` 设为 `/data/gallery-dl.conf`。镜像随 `v*` tag 自动构建推送到 GHCR。

## 架构

单 Rust 二进制 + 两个并发任务：

- **抓取循环**（`main` loop）：`Source`（gallery-dl 抓取后端）→ `FilterChain` → `TelegramSink`（发审批消息），sqlite 去重（`mark_pushed` 在发审批后执行 = 审过即去重）
- **审批回调任务**（`run_review_loop`）：短轮询 `get_updates`（避代理长连接超时），处理按钮回调（批准→发频道+删私聊）和 `/` 命令；`/run` 经 mpsc 通道触发抓取循环立即跑一轮

两阶段抓取：`probe`（`gallery-dl -j` 拉元数据过滤）→ `download`（只下通过的作品）。设计/计划见 `docs/superpowers/`。
