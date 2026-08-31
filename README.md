# Hanabi 🎆

<p align="center">
  <img src="docs/cover.png" alt="cover" width="400">
  <br>
  <sub>Illustration: <a href="https://www.pixiv.net/artworks/141191404">pixiv #141191404</a></sub>
</p>

二次元图片 Telegram 推送 bot，带**人工审批制**。从 Pixiv（关注画师新作 / 排行榜）、X（List）和抖音作者主页抓取插画，先发到你的私聊等审批，你点按钮决定是否发布到频道。

## 特性

- 🔍 **多源抓取**：Pixiv 关注画师新作 + Pixiv 周榜（按标签）+ X List + 抖音作者图文作品
- 🖼️ **人工审批**：作品先发审批私聊（单图直发 / 多图整组 + 控制消息），附 `✅ 发送到频道` / `📦 发送并入库`（配置 Vitrine 时）/ `❌ 丢弃`；同一按钮连点只会执行一次，第二次会显示「已在发布中」；点击后整组消息自动删除
- 📦 **图库入库**：可选对接 Cloudflare 图库 [Vitrine](https://github.com/Furinelle/vitrine)：审批「发送并入库」或手动链接直发频道成功后，用帖子自带标签写入图库
- ✅ **一键批准剩余项**：先逐条点 `❌ 丢弃` 排除不想推送的作品，再点命令菜单中的 `/approve`（仅发布）或 `/approve_archive`（发布并入库），按审批顺序处理所有剩余待审项；重复执行不会重复发布
- 🪄 **原画质进评论区**：批准/直发后，频道帖发压缩大图，**原画质图作为文件自动投递到该帖评论区**（需频道绑定讨论组、bot 为管理员）；未绑定时自动降级，120s 兜底清理
- ♻️ **跨帖子图片去重**：除来源 ID 去重外，还会为 Pixiv / X / 抖音图片建立持久感知指纹；同一平台的同一帖子内所有图片一律视为差分内容，不互相判重或判相似；只有跨平台帖子，或同平台的不同帖子之间才比较。结构与低频颜色都严格一致时才自动去重，整图近似或“完整原图被拆成二/三分栏、2×2、3×3 局部图”的候选都不会自动删除，而是在审批卡片标出类型、双方来源、分辨率和文件大小；人工取舍也以帖子全组为单位，保留某帖时会保留它的全部差分页
- 🔞 **R18 处理**：敏感内容（Pixiv `x_restrict` / X `sensitive`）进审批标 🔞 由你人工决定；发布到频道时图片加**剧透遮罩**（spoiler，点开才显示），审批私聊不打码便于审
- 🤖 **AI 生成标签**：Pixiv `illust_ai_type==2` 的作品 caption 标签首位自动加 `#AI生成`
- 🎯 **分源过滤**：R18 / 收藏数 / 点赞数 / 标签白名单 / 只插画 / 页数上限
- 🧯 **失败可见**：手动链接抓取/发布失败时，「抓取中」提示会改写成具体原因（解析为空、下载 0 张、部分批次失败、队列已满），不静默装成功；超 10 张的作品审批与发布自动按 10 分批
- 💾 **待审图片跨重启保留**：下载文件默认保存在工作目录的 `pending/`，不再依赖会被系统重启清空的 `/tmp`；可用 `HANABI_PENDING_ROOT` 指向单独的持久卷
- ↩️ **误丢弃可撤销**：`/undo` 恢复最近一次误点 `❌ 丢弃` 的作品并重新生成审批卡片；最近动作已是发布时不会误撤销更早记录
- ⌨️ **命令控制**：`/run` 手动抓一轮、`/approve` 一键批准发布、`/approve_archive` 一键批准并入库（配置 Vitrine 时）、`/undo`、`/status`、`/ping`、`/help`
- ⏰ **定时轮询**：`poll_interval_secs` 可配（如一天三次 = 28800），`tz_offset_hours` 可配时区（默认 +8）
- 🐳 **多种部署**：systemd / launchd / Docker（含 gallery-dl）；Docker 镜像在实际部署主机上原生构建，只生成该主机对应的平台

## 前置依赖

- Rust（`cargo build --release`）
- gallery-dl：`pipx install gallery-dl`
- `douyin_user` 来源：Python 3.9+ 与固定版本的 [`jiji262/douyin-downloader`](https://github.com/jiji262/douyin-downloader)（Docker 镜像已内置）
- 一个 Telegram bot（@BotFather 创建，拿 token）

## 认证（`gallery-dl.conf`）

复制 `gallery-dl.conf.example` 为 `gallery-dl.conf`（`chmod 600`，已 gitignore），填入：

- **Pixiv** `extractor.pixiv.refresh-token`：OAuth PKCE 流程获取（浏览器登录授权 → 从回调 URL 的 `code=` 换取 refresh_token）
- **X** `extractor.twitter.cookies`：浏览器（建议小号）登录后从 DevTools → Cookies 复制 `auth_token` 和 `ct0`

> X 还设了 `"videos": false`、`"retweets": false`（只要图片原作）和 `"size": "orig"`（4K 原画质）。

抖音作者源的登录态不进 TOML。按需设置 Cookie header 或 JSON Cookie 文件：

```bash
export HANABI_DOUYIN_COOKIE='ttwid=...; msToken=...; passport_csrf_token=...'
# 或
export HANABI_DOUYIN_COOKIE_FILE=/root/hanabi/.douyin-cookies.json
```

非 Docker 安装桥接依赖：

```bash
python3 -m venv .venv
.venv/bin/pip install aiohttp pyyaml gmssl==3.2.2
.venv/bin/pip install --no-deps \
  'git+https://github.com/jiji262/douyin-downloader.git@ad7338fdc474c14e2063370540a362cd8a953b43'
```

## 配置（`config.toml`）

复制 `config.example.toml` 为 `config.toml`（已 gitignore），关键字段：

```toml
poll_interval_secs = 28800   # 一天三次
tz_offset_hours = 8          # 整点时间槽所用时区（默认 +8，可选）

[telegram]
channel_id = "<你的私聊 chat_id>"     # 审批私聊（作品先发这里）
publish_channel = "@your_channel"   # 批准后发布的频道（绑定讨论组后，原图自动进帖子评论区）

[gallery_dl]
config_path = "gallery-dl.conf"  # 必填：gallery-dl 认证配置路径
probe_range = "1-50"

[douyin]
python_command = ".venv/bin/python"
helper_path = "tools/douyin_user_feed.py"
max_pages = 3                 # 每页 20 条
cookie_file = ".douyin-cookies.json" # 仅路径；Cookie 文件 chmod 600 且不提交
browser_fallback = false      # API 翻页受限时可启用 Playwright 兜底
browser_headless = false

# 可选：X 图片下载画质。download 阶段以 -o extractor.twitter.size=orig 追加；
# 与 gallery-dl.conf 里的 "size" 各管一段（probe 用 conf、download 用这里），两处都设才稳。
[x_image]
size = "orig"

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

# 抖音作者主页/作者短链；纯视频跳过，图集和 Live Photo 的静态图片进入审批。
[[source]]
name = "douyin_artist"
kind = "douyin_user"
targets = ["https://www.douyin.com/user/<sec_user_id>"]
filters = { r18 = false, require_media = true }
```

手动把抖音作者主页或作者短链发给 bot 时，会像 Pixiv/X 作者主页一样批量抓取图文作品并进入审批；单个抖音图文链接仍直接发布到频道。

> `bot_token` **不进**配置文件，走环境变量 `HANABI_BOT_TOKEN`。

## 运行

```bash
export HANABI_BOT_TOKEN="<bot token>"
cargo run --release
```

bot 启动后：抓取循环按 `poll_interval_secs` 定时跑，审批回调任务并发监听按钮/命令。

图片指纹保存在同一个 `hanabi.db` 的 `image_fingerprints` 表。升级启动时会为仍存在原文件的旧待审项自动补建指纹；后续进入审批或手动直发成功的作品都会持续登记。已经发布到频道的旧版本不会被后台静默删除，因此“高清替换低清”只自动作用于尚未审批的严格同图；已发布记录优先用于阻止再次重复发布。

对已经存在于图库中的历史原图，可先导出图片元数据并把对象下载到本地，再用同一套指纹规则生成只读整理报告：

```bash
cargo run --release --bin gallery_catalog -- manifest.json report.json
```

历史频道帖的 Telegram mapping 只能按精确来源链接回填。先用已登录的 Telegram 客户端导出发布频道（`tdl chat export --all --with-content --raw`）；必须包含 raw 字段，默认最小导出缺少媒体组和 chat ID，会被工具拒绝。再对照 Vitrine catalog JSON：

```bash
python3 tools/backfill_telegram_publications.py \
  --export channel-export.json \
  --catalog catalog.json
```

默认 dry-run，只输出 matched / ambiguous / missing 的数量和 work ID，不写 D1。`--apply` 只提交唯一精确匹配，且要求 `HANABI_GALLERY_TOKEN`；任一当前作品 ambiguous 时拒绝 apply。不要用标题、作者或时间做模糊删除。

`manifest.json` 是 `CatalogImage` 数组，包含来源、作品、R2 key 与本地原图路径。报告把结果严格分为 `strict_groups` 和 `similar_pairs`：前者按总像素数、文件大小择优并列出可移除副本；后者用 `kind=visual|partial` 区分整图相似与疑似拆分局部，绝不自动删除，供人工审批。工具本身不写 D1、R2 或 Telegram，实际整理必须先备份并单独执行。

待审原图和 Telegram 发送文件默认保存在当前工作目录的 `pending/`。生产环境必须让该目录与 `hanabi.db` 一样落在持久磁盘；也可以显式指定绝对路径：

```bash
export HANABI_PENDING_ROOT=/var/lib/hanabi/pending
```

如果旧版本因重启清空 `/tmp/hanabi_*`，先停止 Hanabi，再用数据库内保存的作品元数据重新下载 Pixiv/X 待审图片：

```bash
cargo run --release --bin restore_pending --            # 恢复全部待审项
cargo run --release --bin restore_pending -- 306 310    # 只恢复指定 token
```

恢复工具复用 `config.toml` 与 gallery-dl 登录态，并原子更新 pending 文件路径；当前不支持恢复抖音待审项。需要按已确认的审批记录定向发布并入库时，可在单次启动前设置 `HANABI_PUBLISH_PENDING_TOKENS=306,310`，处理完成后必须立即移除该环境变量，避免后续重启重复触发。

频道已经发布、但 Vitrine 入库失败时，Hanabi 会把 `item_meta` 与独立图片副本登记到同一个 `hanabi.db` 的 `gallery_outbox`，默认副本目录为数据库旁的 `gallery-outbox/`（可用 `HANABI_GALLERY_OUTBOX_ROOT` 覆盖）。后台每分钟检查，首次等待 5 分钟后按指数退避重试（最长 6 小时）；成功前不会删除队列和副本，也不会重发 Telegram 或改写 `pushed`。

单独修复一条抖音作品时，先 dry-run，再执行补图库；此工具同样不会发布 Telegram 或修改 `pushed`。可重试的上传失败会由后台继续，永久错误转入 dead-letter；两者都保留 outbox 副本：

```bash
cargo run --release --bin gallery_repair -- --dry-run douyin 7671195794388553011
cargo run --release --bin gallery_repair -- douyin 7671195794388553011
```

## 命令（私聊 bot 发送）

| 命令 | 作用 |
|------|------|
| `/run` | 立即手动抓取一轮 |
| `/approve` | 批准并发布全部剩余待审项 |
| `/approve_archive` | 批准、发布并入库全部剩余待审项（需配置 Vitrine） |
| `/undo` | 撤销最近一次误丢弃，重新生成该作品的审批卡片（保留 7 天） |
| `/status` | 待审数 + 运行状态 |
| `/ping` | 存活测试 |
| `/help` | 命令列表 |

> 💡 **直接发链接**给 bot（host 精确识别 Pixiv/X/抖音，防伪装域名）：
> - **单作品链接**（`artworks/<id>`、`status/<id>`）→ 跳过审批**直发频道**（手动=已选定）。
> - **多作品链接**（画师主页 / 榜单 / list）→ 逐个**进审批私聊**过按钮，不直发。
> - **抖音图文**（短链 `v.douyin.com`，或「复制打开抖音…」整段分享文本）→ 解析下载**无水印原图**直发频道，`From 抖音 By 作者`；单张失败会轮换备用 CDN 并重试，仍不完整则整条不发布、可重新发送链接重试。

## 图库（可选 · Vitrine）

```toml
[gallery]
endpoint = "https://vitrine.<subdomain>.workers.dev"
# token 推荐环境变量 HANABI_GALLERY_TOKEN，与 Worker secret INGEST_TOKEN 一致
token = ""
```

| 操作 | 频道 | 图库 |
|---|---|---|
| ✅ 发送到频道 | ✓ | |
| 📦 发送并入库 | ✓ | ✓（帖子自带 tags） |
| `/approve` 一键批准 | ✓ | |
| `/approve_archive` 一键批准并入库 | ✓ | ✓（帖子自带 tags） |
| `/undo` 撤销误丢弃 | 恢复审批卡片 | 恢复后由新选择决定 |
| 手动单作品链接直发 | ✓ | ✓（已配置 gallery） |

## 审批流程

```
抓取 → 过滤/去重 → 下载 → 发审批私聊（图 + caption + 按钮）
                                    │
              ┌─────────────────────┼─────────────────────┐
         ✅ 发送到频道         📦 发送并入库           ❌ 丢弃
              │                     │                     │
        发布到频道            发布到频道+图库          （不发布）
              └─────────────────────┬─────────────────────┘
                                    │
                           删除私聊审批消息
                                    │
                         丢弃后 7 天内可用 /undo
```

> 误点 `❌ 丢弃` 后发送 `/undo`，bot 会保留原文件并重新生成一张可审批卡片。系统会记录最近审批动作：如果丢弃之后又完成了发送或发送并入库，`/undo` 会明确提示最近动作已产生外部发布，不会误恢复更早的丢弃。新的审批动作会替换旧的撤销记录并清理旧文件，最多保留 7 天。

> 推荐批量审批流程：先给不想推送的作品逐条点 `❌ 丢弃`，再从 Telegram 命令菜单点 `/approve`（仅发布）或 `/approve_archive`（发布并入库，仅在配置 Vitrine 时显示）。命令会一次性原子抢占当时所有剩余待审项并按顺序处理；正在丢弃、单独批准或已被另一条批量命令抢占的记录不会重复发布。单条发送失败会自动恢复为待审，可再次执行对应命令重试。

> 同一审批记录会先被原子标记为「发布中」：手滑连点或连续收到同一 callback 时，只有第一次会上传到频道；若发送失败，记录自动恢复为待审，可直接再点一次。

caption 格式：
```
🔞 R18                       （仅敏感内容，发频道时图片打剧透遮罩）
Title: 标题
Tag: #AI生成 #标签 #标签       （#AI生成 仅 Pixiv illust_ai_type==2 时首位添加）
From <Pixiv|X|抖音>(作品链接) By 作者名(作者链接)
```

## 部署

### Linux（systemd，VPS）

见 `deploy/hanabi.service`。将仓库 clone 到 VPS，装 rust + gallery-dl，传入 `gallery-dl.conf` / `config.toml`，配置 systemd service（`HANABI_BOT_TOKEN` 经 service 环境变量注入），`systemctl enable --now hanabi`。`WorkingDirectory` 必须位于持久磁盘；默认会在其下创建 `pending/`。

### macOS（launchd）

`deploy/ai.hanabi.plist` → `~/Library/LaunchAgents/`，改占位后 `launchctl load`。

### Docker Compose（推荐用于 VPS）

镜像内含固定版本的 gallery-dl、`gallery_repair` 与 `restore_pending`。发布时用已创建 tag 的干净源码在实际部署主机原生构建，不经 QEMU 构建其他平台：

```bash
./tools/build_on_target.sh <SSH 主机> vX.Y.Z
```

脚本会将该 tag 归档传到目标机的临时目录，用目标 Docker daemon 的原生架构构建 `ghcr.io/furinelle/hanabi:vX.Y.Z`，核对镜像架构与源码 revision，然后清理临时源码。GitHub Actions 只运行格式、测试和 Clippy，不再构建或发布容器。

生产必须固定不可变版本，不能使用 `latest`。把 DB、pending、outbox 和配置作为一个状态目录挂到容器内固定的 `/opt/hanabi`，这样 SQLite 中已有的绝对图片路径在换 VPS 或重建容器后仍然有效：

```bash
install -d -m 0700 /var/lib/hanabi /opt/hanabi-container
cp deploy/compose.yaml /opt/hanabi-container/compose.yaml

# /opt/hanabi-container/.env（不入库）
HANABI_IMAGE=ghcr.io/furinelle/hanabi:<固定版本>
HANABI_UID=<宿主机状态目录属主 UID>
HANABI_GID=<宿主机状态目录属组 GID>
HANABI_STATE_DIR=/var/lib/hanabi

# /opt/hanabi-container/hanabi.env（chmod 600，不入库）
HANABI_BOT_TOKEN=<bot token>
HANABI_GALLERY_TOKEN=<gallery token>

cd /opt/hanabi-container
docker compose config --quiet
docker compose up -d --pull never
```

`/var/lib/hanabi` 内至少包含 `config.toml`、`gallery-dl.conf`、`hanabi.db`、`pending/` 与 `gallery-outbox/`，目录及文件应只允许运行 UID 读取。迁移时先停旧实例并执行 SQLite WAL checkpoint，再整体复制该目录；目标验证完成前不能启动第二个 Telegram polling 实例。Compose 使用只读根文件系统、空 capabilities、`no-new-privileges` 和有界日志轮转。

## 架构

单 Rust 二进制 + 三个并发任务：

- **抓取循环**（`main` loop）：`Source`（Pixiv/X 使用 gallery-dl；抖音作者使用 douyin-downloader JSON 桥）→ `FilterChain` → `TelegramSink`（发审批消息），sqlite 去重（`mark_pushed` 在发审批后执行 = 审过即去重）
- **审批回调任务**（`run_review_loop`）：短轮询 `get_updates`（避代理长连接超时），处理按钮回调（批准→发频道+删私聊）和 `/` 命令；`pending.state` 通过条件更新原子抢占，保证同一审批只会启动一次上传；`/run` 经 mpsc 通道触发抓取循环立即跑一轮
- **图库补偿任务**：只消费持久 `gallery_outbox` 并重试 Vitrine；不调用 Telegram，也不读写 `pushed`

两阶段抓取：`probe`（`gallery-dl -j` 拉元数据过滤）→ `download`（只下通过的作品）。设计/计划见 `docs/superpowers/`。
