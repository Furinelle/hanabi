# Hanabi — 二次元图片 Telegram 推送 Bot · 设计文档

**日期**:2026-06-07
**状态**:设计已确认,待 review → 进入实现计划
**语言/技术栈**:Rust(主体)+ gallery-dl(抓取后端)

---

## 1. 概述与目标

Hanabi 是一个常驻运行的 Telegram 机器人,定时从 **Pixiv** 和 **X(Twitter)** 抓取二次元插画,经过去重与过滤后,把符合条件的作品以相册形式推送到指定的 Telegram 频道。

**核心目标**:让用户关注的画师新作、自己的收藏/时间线里的好图,自动、去重、过滤后出现在自己的频道里,无需手动刷。

**成功标准**:
- 关注画师的新插画能在一个轮询周期内(默认 30 分钟)推送到频道。
- 不重复推送同一作品。
- 过滤规则按来源生效,频道里看到的都是符合调性(全年龄、达标、命中 tag)的图。
- 进程崩溃/重启后不漏推、不重推。

---

## 2. 需求

### 2.1 数据源(均使用用户自己的登录态)
- **Pixiv**:① 关注画师的新作;② 用户自己的收藏夹;③ 排行榜(日榜/周榜等)。认证 = OAuth `refresh_token`。榜单嘈杂 → 严格过滤,**必启用 tag 白名单**。gallery-dl 支持 pixiv ranking 抓取。
- **X(Twitter)**:① 用户自建 List 里画师的作品(List timeline);② "为你推荐"(For You)推荐流插画。认证 = `auth_token` + `ct0` cookie(**建议使用小号**隔离封号风险)。
  - gallery-dl twitter 提取器支持 `https://x.com/i/lists/<id>`(List);"For You"算法流支持不确定,实现阶段验证,必要时退回 home/following 流。
  - **For-You 流嘈杂**:X 无 illust 类型字段,插画筛选只能 best-effort(靠 hashtag 如 `#イラスト` + 必须带图 + 高点赞阈值)。
  - **画质**:X 图按最高画质下载(gallery-dl `size = "orig"`,至 4096),送达仍发 photo(见 §2.3),让 TG 压缩基于 4K 原图而非缩略图。

### 2.2 过滤管线(按来源可配置强度)
执行顺序:
1. **去重**(必做,不可关):按 `(source_kind, source_id)` 判重。
2. **R18 过滤**:只保留全年龄(Pixiv `x_restrict == 0`)。
3. **Pixiv 类型过滤**:只保留 `illust`(排除 `manga`、`ugoira`)。
4. **页数过滤**:`page_count < 5`。
5. **收藏/点赞阈值**:低于设定值不推。
6. **Tag 白名单**:作品 tag 与白名单有交集才推。

**按来源差异化**(解决"信任画师"与"严格过滤"的矛盾):
- **画师源**:宽松(默认仅去重 + R18 过滤,收下画师全部全年龄新作)。
- **收藏 / 榜单 / List 源**:严格(启用阈值 + tag 白名单等全部规则;榜单**必带 tag 白名单**)。

AI 生成图:**不过滤**(照常推送)。

### 2.3 推送
- 一个作品的全部图打包成一个 Telegram 相册(因页数 < 5,至多 4 张,单条相册装得下,不分批、不刷屏)。
- 图片**直接发送**(超出 Telegram 限制时压缩为 photo;不保留原图 document)。
- caption(置于相册第一张):`标题 · 画师名(超链到主页) · 原作品链接 · 主要 tag`,HTML 格式。
- X 推文图无 illust/manga 分类,按 1–4 张直接推;按 4K/orig 下载后仍发 photo(TG 压缩,不发 document)。

### 2.4 运行
- tokio 定时轮询,默认间隔 30 分钟(可配置)。
- 部署路径:**先本地 Mac(launchd)跑通 → 后迁 VPS**。
- 单 Rust 二进制 + 外部 gallery-dl 依赖。

---

## 3. 非目标(v1 范围外)

- **booru 家族图源**(Danbooru / yande.re / Konachan 等):架构预留 `Source` 接口,留作 **v2**。
- 原画质 document 存档、网盘分发、Web 管理界面、交互式命令(收藏/搜图)、ugoira 动图合成、Pixiv 漫画/多页(>4 页)作品。

---

## 4. 架构(方案 B:trait 模块化单体)

单进程、单二进制。三层经 trait 解耦,X 的抓取脏活完全封装在 `XSource` 内,与 Pixiv 互不污染。该模块化方向已由同类成熟项目 [nazurin](https://github.com/y-young/nazurin)(driver-based,16+ 图源)验证。

```
src/
  main.rs          // 启动、加载配置、起 tokio 定时调度循环
  config.rs        // TOML 配置 + 密钥加载
  model.rs         // 核心数据类型:MediaItem / ImageRef / SourceKind ...
  store.rs         // sqlite:去重表、每源 cursor
  gallerydl.rs     // gallery-dl 子进程封装 + JSON 解析(pixiv/x 复用)
  source/
    mod.rs         //   trait Source
    pixiv.rs       //   PixivSource
    x.rs           //   XSource(cookie + 内部 GraphQL,脏活封这里)
  filter/
    mod.rs         //   trait Filter + FilterChain
    rules.rs       //   R18 / 类型 / 页数 / 收藏数 / tag 各规则
  sink/
    mod.rs         //   trait Sink
    telegram.rs    //   TelegramSink(相册推送 + 图片处理)
```

**Rust 依赖**:`tokio`、`teloxide`、`reqwest`、`rusqlite`、`serde` / `serde_json`、`toml`、`anyhow` / `thiserror`、`tracing`。
**外部依赖**:`gallery-dl`(Python)。

### 核心 trait

```rust
#[async_trait] trait Source { async fn fetch(&self, store: &Store) -> Result<Vec<MediaItem>>; }
trait Filter   { fn keep(&self, item: &MediaItem, cfg: &SourceFilterCfg) -> bool; }
#[async_trait] trait Sink   { async fn deliver(&self, item: &MediaItem) -> Result<()>; }
```

---

## 5. 数据模型

统一中间表示,把"源"与"下游"彻底解耦:

```rust
struct MediaItem {
    source: SourceKind,            // Pixiv | X
    source_id: String,             // 作品/推文 ID(去重键)
    author: Author,                // 名字 + 主页链接
    title: Option<String>,
    url: String,                   // 原作品/推文链接
    tags: Vec<String>,
    bookmark_count: Option<u32>,   // Pixiv 收藏数 / X 点赞数
    is_r18: bool,
    pixiv_type: Option<PixivType>, // illust | manga | ugoira
    page_count: u32,
    images: Vec<ImageRef>,         // 每张图:下载 URL + 需要的 Referer 等
    origin: OriginCtx,             // 来自哪个源实例 → 决定过滤强度
}
```

---

## 6. 模块详解

### 6.1 Source 层(gallery-dl 集成)

**两阶段抓取**(先探测、再下载,省下被过滤掉的图的流量):

```
① 探测:gallery-dl --config our.conf -j --range 1-20 <target_url>
        → 输出 JSON(不下载)→ 解析成 Vec<MediaItem>
② 去重 + 过滤(纯元数据,不碰网络)
③ 下载:对通过的作品 gallery-dl <work_url> -D <tmpdir>
        → gallery-dl 自动处理 Pixiv 防盗链 Referer → 本地原图路径
```

- **认证集中**:Pixiv `refresh_token`、X cookies 全部放在 `gallery-dl.conf`,Rust 侧不直接接触。
- `--range 1-20`:每个 target 每轮只看最近 20 个,避免拉全历史。
- **Pixiv 可用元数据字段**:`id`、`title`、`tags`、`total_bookmarks`、`type`、`page_count`、`x_restrict`、`user.name`/`user.id`。
- `gallerydl.rs` 封装子进程调用与 JSON → `MediaItem` 映射;`pixiv.rs`/`x.rs` 各自构造 target URL 并复用它。

### 6.2 Filter 层

每条规则一个 `Filter` 实现,组成 `FilterChain`:

```
R18Filter         → x_restrict == 0
PixivTypeFilter   → type == "illust"
PageCountFilter   → page_count < 5
BookmarkThreshold → bookmark_count >= N
TagWhitelist      → tags ∩ whitelist ≠ ∅
```

每个源携带 `SourceFilterCfg`,决定启用哪些规则及阈值。

### 6.3 Sink 层(Telegram)

- `sendMediaGroup` 发相册(2–4 张);单张用 `sendPhoto`。
- 图片超 Telegram 上传限制(photo 约 10MB + 尺寸约束)时,用 `image` crate 压缩/缩放到限制内再发。
- `teloxide` 的 throttle 适配器自动处理 Telegram 频控。

### 6.4 Store(sqlite)

```sql
CREATE TABLE pushed (
  source_kind TEXT, source_id TEXT, pushed_at INTEGER,
  PRIMARY KEY (source_kind, source_id)
);
```

- `already_pushed(item)`:主键存在性查询。
- `mark_pushed`:**仅在 `sink.deliver` 成功后**写入 → 幂等,失败下轮自动重试,不漏不重。

### 6.5 调度(main.rs)

tokio 定时器,每 `poll_interval_secs` 触发一轮主循环。

---

## 7. 数据流(主循环,每 30 分钟)

```
for source in sources:
    items = source.fetch(store)                       // 带 since/cursor,只取新内容
    for item in items:
        if store.already_pushed(item): continue       // ① 去重
        if !filter_chain.keep(item, item.origin.cfg): continue  // ② 按来源过滤
        downloaded = download_images(item)             // ③ gallery-dl 下载,处理 Referer
        sink.deliver(downloaded)                       // ④ TG 相册
        store.mark_pushed(item)                        // ⑤ 落库(成功才落)
```

---

## 8. 配置(`config.toml`,密钥分离)

```toml
poll_interval_secs = 1800

[telegram]
channel_id = "@my_channel"
# bot_token 走环境变量,不进此文件

[gallery_dl]
config_path = "gallery-dl.conf"      # pixiv refresh_token + x cookies 全在这
probe_range = "1-20"

[[source]]                           # 画师源:宽松
name = "fav_artists"; kind = "pixiv_user"
targets = ["https://www.pixiv.net/users/123", "..."]
filters = { r18 = false }

[[source]]                           # 收藏源:严格
name = "my_bookmarks"; kind = "pixiv_bookmarks"
targets = ["https://www.pixiv.net/users/<me>/bookmarks/artworks"]
filters = { r18 = false, min_bookmarks = 500, tags = ["原神"], illust_only = true, max_pages = 5 }

[[source]]                           # Pixiv 榜单:嘈杂→严格,必过滤 tag
name = "pixiv_ranking"; kind = "pixiv_ranking"
targets = ["https://www.pixiv.net/ranking.php?mode=daily&content=illust"]
filters = { r18 = false, min_bookmarks = 1000, tags = ["原神"], illust_only = true, max_pages = 5 }

[[source]]                           # X 自建 List(画师)
name = "x_artists_list"; kind = "x_list"
targets = ["https://x.com/i/lists/<list_id>"]
filters = { r18 = false, min_likes = 1000 }

[[source]]                           # X 为你推荐(For You,嘈杂→严格)
name = "x_foryou"; kind = "x_foryou"
targets = ["https://x.com/home"]     # For-You 流;gallery-dl 支持待验证
filters = { r18 = false, min_likes = 2000, require_media = true, tags = ["イラスト"] }

[x_image]
size = "orig"                        # X 图下载最高画质(至 4096),送达仍 photo
```

**密钥三件套**(bot_token / pixiv refresh_token / x cookies)全部 git 忽略,放环境变量或 `gallery-dl.conf`(权限 600)。

---

## 9. 错误处理与韧性(分级隔离)

- 一个 source 挂 → log + skip,不连累其他源;一张图下载挂 → skip 该作品;一次推送挂 → 不落库,下轮重试。
- **X 出 401/429**(cookie 失效 / 限流)→ 指数退避 + **给用户发 Telegram 私信告警**(X 最脆,需第一时间感知)。
- **Telegram 自身限流**:`teloxide` throttle 自动节流。
- **崩溃恢复**:launchd / systemd 自动拉起;sqlite 状态持久,重启不重推。

---

## 10. 部署

- **本地(起步)**:`cargo build --release` → launchd 常驻。
- **VPS(后续)**:Docker 镜像打包 `Rust 二进制 + gallery-dl + Python` → systemd,部署到 `lisahost`。
- 密钥:本地放 `~/.config`;VPS 放环境变量或 secrets 文件(权限 600)。

---

## 11. 测试策略

- **单元**:每条 Filter 规则(给定 `MediaItem` 断言 `keep`)、gallery-dl JSON 解析(真实样例 → `MediaItem`)、caption 渲染。
- **集成**:用预录 JSON fixture mock 掉 gallery-dl 子进程,跑 `fetch → filter → MockSink`,断言去重/过滤正确——不碰真实网络。
- **冒烟**:真拉一个画师 → 推到测试频道。
- 重点覆盖过滤逻辑与 JSON 解析(最易出 bug 处);不追求覆盖率数字。

---

## 12. 参考项目

- **[nazurin](https://github.com/y-young/nazurin)**(323⭐,Python,活跃)— 最佳参考,需求几乎一致,driver-based 多源架构验证了本设计方向。重点参考其 pixiv / twitter driver 的字段处理。
- `666wcy/ARPT-Bot`(689⭐,已归档)— 综合下载,含 Pixiv 榜单。
- `my-telegram-bots/Pixiv_bot`(JS)— 专做 pixiv → TG,处理 ugoira / telegraph。
- `jckling/tg-bot` — GitHub Action 每周推 Pixiv 周榜(轻量思路)。
- `soruly/awesome-acg`(1459⭐)— ACG 技术清单。

---

## 13. 未来扩展(v2)

- **booru 家族图源**(Danbooru / yande.re / Konachan,有正规 API、按 tag 拉高质量图、无需对抗反爬),复用 `Source` trait 接入,可在 X/Pixiv 认证失效时给频道兜底。
- 原画质 document 存档、交互式命令、Pixiv 漫画/ugoira 支持。
