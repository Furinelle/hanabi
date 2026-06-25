# Hanabi 改进升级 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修一批稳定性/质量缺陷，并增强交付能力（原画质图进频道帖评论区、手动链接收紧、Docker 镜像），不破坏现有「人工审批制」架构。

**Architecture:** 单 Rust 二进制，`Source → FilterChain → TelegramSink` 三层 trait 解耦，sqlite 去重，抓取循环与 `run_review_loop` 经 `tokio::spawn` 并发、共享 `Arc<ReviewState>`。本计划在此之上做**点状**修改与三个新增能力，不重构主架构。

**Tech Stack:** Rust 2021、tokio、teloxide 0.13、rusqlite(bundled)、serde/serde_json、toml、anyhow、tracing、image 0.25；外部 gallery-dl(Python 子进程)。

设计来源：`docs/superpowers/specs/2026-06-25-hanabi-improvements-design.md`。

## Global Constraints

- Rust 2021，rustc 1.95，teloxide **0.13.0**，reqwest **0.11.27**（不升级——`trust_dns` 是 musl DNS 命脉）。
- 中文注释，匹配现有代码风格与注释密度。
- 每个 PR 区段结束时 `cargo test` 全绿、`cargo clippy` 零告警。
- 不新增运行时外部依赖（ugoira/ffmpeg 不做）。
- 所有 sqlite 兼容性改动用 `ALTER TABLE ... ADD COLUMN`（已存在则忽略错误），不破坏旧库。

## 经验性未知（实施中头部验证，不要假设）

1. **teloxide 0.13 auto-forward 字段形状**：频道帖自动转发到讨论组后，那条 `Message` 用什么暴露「源频道 + 源消息 id」（`forward_origin` / `forward_from_chat` / `is_automatic_forward`）——**PR4 Task 1 先用真机抓一条真实 `Update` 存成 fixture 校准**，再写 `match_auto_forward`。
2. **teloxide 0.13 send_media_group 的 reply 方法名**：`reply_to_message_id` 还是 `reply_parameters`——PR4 Task 1 一并确认，写进 fixture 旁注。

---

# PR 1 — 阶段一：稳定性与质量

纯修复+清理，除告警外不改用户可见行为。本 PR 结束应 `cargo test` 全绿、`cargo clippy -D warnings` 零告警、`cargo fmt --check` 通过。

### Task 1: Store 连接加 busy_timeout + WAL

**Files:**
- Modify: `src/store.rs`（`Store::init`）
- Test: `src/store.rs`（`#[cfg(test)]`）

**Interfaces:**
- Produces: `Store::open` / `Store::open_in_memory` 行为不变，但连接启用 WAL + busy_timeout。

- [ ] **Step 1: 写失败测试** — 在 `src/store.rs` 的 `mod tests` 加：

```rust
#[test]
fn open_sets_wal_and_busy_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let store = Store::open(path.to_str().unwrap()).unwrap();
    let mode: String = store
        .conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
    let busy: i64 = store
        .conn
        .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
        .unwrap();
    assert_eq!(busy, 5000);
}
```

> 注：`tempfile` 已是 dev-dependency。该测试需要访问 `store.conn`；`conn` 当前是私有字段，同模块测试可访问，无需改可见性。WAL 只在文件库生效（内存库会回落），故用临时文件。

- [ ] **Step 2: 跑测试确认失败** — `cargo test --lib store::tests::open_sets_wal_and_busy_timeout`，预期 FAIL（journal_mode 为 "memory"/"delete"，busy_timeout 为 0）。

- [ ] **Step 3: 实现** — 在 `src/store.rs` 的 `fn init` 开头、`CREATE TABLE` 之前加 PRAGMA：

```rust
    fn init(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;",
        )?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS pushed (
                 source_kind TEXT NOT NULL,
                 source_id   TEXT NOT NULL,
                 pushed_at   INTEGER NOT NULL,
                 PRIMARY KEY (source_kind, source_id)
             );",
        )?;
        Ok(())
    }
```

- [ ] **Step 4: 跑测试确认通过** — `cargo test --lib store::tests`，预期 PASS（含原有两测）。

- [ ] **Step 5: 提交**

```bash
git add src/store.rs
git commit -m "fix: Store 连接启用 WAL + busy_timeout 消除并发写锁竞争

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

### Task 2: 时间槽时区配置化

**Files:**
- Modify: `src/config.rs`（`Config` 加字段）、`src/main.rs`（`secs_until_next_slot` 签名 + 调用）
- Test: `src/main.rs`（`#[cfg(test)]`）

**Interfaces:**
- Produces: `fn secs_until_next_slot(interval_secs: u64, tz_offset_hours: i64, now_unix: u64) -> u64`（纯函数，可注入时间）；`Config.tz_offset_hours: i64`（默认 8）。

- [ ] **Step 1: 写失败测试** — 在 `src/main.rs` 末尾加 `#[cfg(test)] mod tests`：

```rust
#[cfg(test)]
mod tests {
    use super::secs_until_next_slot;

    #[test]
    fn slot_aligns_to_interval_in_local_tz() {
        // UTC 16:00 = CST(+8) 次日 00:00 整点槽 → 距下一个 8h 槽应为 8h。
        // 取一个 local 时间正好落在槽起点的 now：local secs_into_day = 0。
        // UTC unix 使 (utc + 8*3600) % 86400 == 0：utc % 86400 == 86400-28800 == 57600。
        let now = 57600; // 当天 UTC 16:00:00
        assert_eq!(secs_until_next_slot(28800, 8, now), 28800);
        // 槽中点：local 04:00（now=86400-28800+14400=72000），距下一个 08:00 槽 4h。
        assert_eq!(secs_until_next_slot(28800, 8, 72000), 14400);
        // tz_offset=0 时同一 now=57600 → local 16:00，距 24:00 槽 8h。
        assert_eq!(secs_until_next_slot(28800, 0, 57600), 28800);
    }
}
```

- [ ] **Step 2: 跑测试确认失败** — `cargo test --bin hanabi slot_aligns`，预期 FAIL（签名不匹配，编译错误即视为失败）。

- [ ] **Step 3: 改 `secs_until_next_slot`**（`src/main.rs`）为纯函数：

```rust
/// 计算距下一个整点时间槽的秒数。`tz_offset_hours` 为本地时区相对 UTC 的偏移
/// (CST=+8)。`now_unix` 为当前 UTC 秒(注入便于测试)。
/// interval_secs 须能整除 86400，例如 28800 → 00:00 / 08:00 / 16:00。
fn secs_until_next_slot(interval_secs: u64, tz_offset_hours: i64, now_unix: u64) -> u64 {
    let local = (now_unix as i64 + tz_offset_hours * 3600).rem_euclid(86400) as u64;
    let next_slot = ((local / interval_secs) + 1) * interval_secs;
    next_slot - local
}
```

- [ ] **Step 4: 改调用点**（`src/main.rs` loop 内）：

```rust
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let wait = secs_until_next_slot(cfg.poll_interval_secs, cfg.tz_offset_hours, now_unix);
```

并删除文件顶部 `secs_until_next_slot` 旧实现里的 `use std::time::{SystemTime, UNIX_EPOCH}` 内联（已移到调用点）。

- [ ] **Step 5: 加配置字段**（`src/config.rs` 的 `Config`）：

```rust
#[derive(Debug, Deserialize)]
pub struct Config {
    pub poll_interval_secs: u64,
    #[serde(default = "default_tz_offset")]
    pub tz_offset_hours: i64,
    pub telegram: TelegramCfg,
    pub gallery_dl: GalleryDlCfg,
    #[serde(default)]
    pub x_image: XImageCfg,
    #[serde(rename = "source", default)]
    pub sources: Vec<SourceCfg>,
}

fn default_tz_offset() -> i64 {
    8
}
```

- [ ] **Step 6: 跑测试** — `cargo test`，预期全绿（config 既有测试不传 tz_offset_hours，走默认 8）。

- [ ] **Step 7: 提交**

```bash
git add src/config.rs src/main.rs
git commit -m "feat: 时间槽时区可配置(tz_offset_hours, 默认8), secs_until_next_slot 纯函数化

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

### Task 3: owner 解析失败显式告警

**Files:**
- Modify: `src/sink/telegram.rs`（`TelegramSink::new`）
- Test: `src/sink/telegram.rs`（`#[cfg(test)]`）

**Interfaces:**
- Produces: `fn parse_owner(review_chat_id: &str) -> Option<i64>`（纯函数）。

- [ ] **Step 1: 写失败测试**（`src/sink/telegram.rs` 的 `mod tests`）：

```rust
#[test]
fn parse_owner_numeric_only() {
    assert_eq!(parse_owner("7794592020"), Some(7794592020));
    assert_eq!(parse_owner("@my_channel"), None);
    assert_eq!(parse_owner(""), None);
}
```

- [ ] **Step 2: 跑测试确认失败** — `cargo test --lib telegram::tests::parse_owner_numeric_only`，预期 FAIL（函数未定义）。

- [ ] **Step 3: 实现** — 在 `src/sink/telegram.rs` 加纯函数，并改 `TelegramSink::new`：

```rust
/// 解析审批私聊数字 id。非数字(如 @username)返回 None —— 命令/链接功能要求数字 id。
fn parse_owner(review_chat_id: &str) -> Option<i64> {
    review_chat_id.parse::<i64>().ok()
}
```

把 `TelegramSink::new` 里 `let owner: i64 = review_chat_id.parse().unwrap_or(0);` 改为：

```rust
        let owner: i64 = match parse_owner(&review_chat_id) {
            Some(n) => n,
            None => {
                tracing::error!(
                    channel_id = %review_chat_id,
                    "channel_id 非数字 id, 命令/链接功能将无法响应(owner 校验恒不匹配); 请改用数字私聊 id"
                );
                0
            }
        };
```

- [ ] **Step 4: 跑测试** — `cargo test --lib telegram::tests`，预期 PASS。

- [ ] **Step 5: 提交**

```bash
git add src/sink/telegram.rs
git commit -m "fix: channel_id 非数字时显式 error 告警, 不再静默吞掉所有命令

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

### Task 4: 缩放保留 alpha（不强转 JPG）

**Files:**
- Modify: `src/sink/telegram.rs`（`prepare`）
- Test: `src/sink/telegram.rs`（`#[cfg(test)]`）

**Interfaces:**
- Produces: `prepare(path) -> Result<PathBuf>` 行为不变，但缩放副本保留原格式（PNG 保 alpha）。

- [ ] **Step 1: 写失败测试**（`src/sink/telegram.rs` 的 `mod tests`，需 `use image::GenericImageView;`）：

```rust
#[test]
fn prepare_preserves_png_alpha_when_downscaling() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("big.png");
    // 9000x2000 RGBA(宽+高>10000 触发缩放), 半透明像素。
    let mut img = image::RgbaImage::new(9000, 2000);
    for p in img.pixels_mut() {
        *p = image::Rgba([10, 20, 30, 128]);
    }
    img.save(&src).unwrap();

    let out = prepare(&src).unwrap();
    assert_ne!(out, src, "应产出缩放副本");
    assert_eq!(out.extension().unwrap(), "png", "应保留 png 而非转 jpg");
    let reloaded = image::open(&out).unwrap();
    assert_eq!(reloaded.color().has_alpha(), true, "应保留 alpha 通道");
}
```

- [ ] **Step 2: 跑测试确认失败** — `cargo test --lib telegram::tests::prepare_preserves_png_alpha_when_downscaling`，预期 FAIL（现实现输出 `.scaled.jpg`、无 alpha）。

- [ ] **Step 3: 实现** — 把 `prepare` 末尾改为按原格式保存：

```rust
    let dyn_img = image::open(path).context("打开图片失败")?;
    let scaled = dyn_img.resize(
        MAX_DIMENSION,
        MAX_DIMENSION,
        image::imageops::FilterType::Lanczos3,
    );
    // 保留原格式: PNG 缩放后仍是 PNG(保 alpha), JPG 仍是 JPG。
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("jpg");
    let out = path.with_extension(format!("scaled.{ext}"));
    scaled.save(&out).context("保存缩放图失败")?;
    Ok(out)
```

> 删除原来的 `scaled.to_rgb8().save(...)` 与 `with_extension("scaled.jpg")`。`DynamicImage::save` 按扩展名推断编码并保留 RGBA(png)。

- [ ] **Step 4: 跑测试** — `cargo test --lib telegram::tests`，预期 PASS。

- [ ] **Step 5: 提交**

```bash
git add src/sink/telegram.rs
git commit -m "fix: 超限图缩放保留原格式与 alpha 通道, 不再强转 RGB JPG

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

### Task 5: 周期性 cleanup_stale

**Files:**
- Modify: `src/sink/telegram.rs`（`run_review_loop` + 新增纯函数 `cleanup_due`）
- Test: `src/sink/telegram.rs`（`#[cfg(test)]`）

**Interfaces:**
- Consumes: 已有 `cleanup_stale(&state)`、`now_secs()`。
- Produces: `fn cleanup_due(last_secs: i64, now_secs: i64, interval_secs: i64) -> bool`。

- [ ] **Step 1: 写失败测试**（`mod tests`）：

```rust
#[test]
fn cleanup_due_after_interval() {
    assert!(!cleanup_due(1000, 1000 + 6 * 3600 - 1, 6 * 3600));
    assert!(cleanup_due(1000, 1000 + 6 * 3600, 6 * 3600));
}
```

- [ ] **Step 2: 跑测试确认失败** — `cargo test --lib telegram::tests::cleanup_due_after_interval`，预期 FAIL（未定义）。

- [ ] **Step 3: 实现纯函数 + 接入循环** — 加：

```rust
/// 距上次清理是否已超 interval。
fn cleanup_due(last_secs: i64, now_secs: i64, interval_secs: i64) -> bool {
    now_secs - last_secs >= interval_secs
}
```

在 `run_review_loop` 里，启动清理后加计时变量，每轮判断：

```rust
    // 启动先清一次超期/孤儿。
    cleanup_stale(&state).await;
    let mut last_cleanup = now_secs();
    const CLEANUP_INTERVAL_SECS: i64 = 6 * 3600;

    let mut offset: i32 = 0;
    loop {
        // 周期清理(常驻不重启实例也能清过期 pending/孤儿目录)。
        if cleanup_due(last_cleanup, now_secs(), CLEANUP_INTERVAL_SECS) {
            cleanup_stale(&state).await;
            last_cleanup = now_secs();
        }
        let updates = state
            .bot
            .get_updates()
            // ...(其余不变)
```

- [ ] **Step 4: 跑测试** — `cargo test --lib telegram::tests`，预期 PASS。

- [ ] **Step 5: 提交**

```bash
git add src/sink/telegram.rs
git commit -m "fix: cleanup_stale 周期化(每6h), 常驻实例也能清过期 pending

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

### Task 6: handle_link 异步化（不阻塞调度）

**Files:**
- Modify: `src/main.rs`（`main` 的 sink/store 包装 + `link_rx` 分支 + `handle_link` 签名）

**Interfaces:**
- Produces: `sink` 改为 `Arc<TelegramSink>`；`handle_link` 在独立 task 内运行、内部自开 `Store`。

> 背景：`Store` 持 `rusqlite::Connection`(`!Sync`)，不能跨 `spawn` 共享 `&Store`。方案：被 spawn 的 `handle_link` 内部自开一条 `Store::open("hanabi.db")`（Task 1 已加 busy_timeout，并发连接安全）；`sink` 包成 `Arc` 便于克隆进 task。无独立单测，靠 `cargo build` + 既有测试守护。

- [ ] **Step 1: sink 包 Arc** — `main` 里：

```rust
    let sink = Arc::new(TelegramSink::new(
        token,
        cfg.telegram.channel_id.clone(),
        cfg.telegram.publish_channel.clone(),
        "hanabi.db",
    )?);
```

`run_once(... &sink as &dyn Sink ...)` 两处改为 `sink.as_ref() as &dyn Sink`。

- [ ] **Step 2: 改 `link_rx` 分支**为 spawn：

```rust
            Some(job) = link_rx.recv() => {
                tracing::info!(url = %job.url, "收到手动链接,直发频道");
                let gdl = gdl.clone();
                let sink = sink.clone();
                let x_size = x_size.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_link(job, &gdl, x_size.as_deref(), &sink).await {
                        tracing::warn!(error = %e, "手动链接处理失败");
                    }
                });
                false
            }
```

- [ ] **Step 3: 改 `handle_link` 签名**——去掉 `store: &Store` 形参，内部自开：

```rust
async fn handle_link(
    job: hanabi::sink::telegram::LinkJob,
    gdl: &Arc<GalleryDl>,
    x_size: Option<&str>,
    sink: &TelegramSink,
) -> Result<()> {
    let store = Store::open("hanabi.db").context("handle_link 打开 Store 失败")?;
    // ...函数体其余不变(原先用 store 处照常)...
```

- [ ] **Step 4: 验证** — `cargo build && cargo test`，预期编译通过、测试全绿。

- [ ] **Step 5: 提交**

```bash
git add src/main.rs
git commit -m "perf: handle_link 移入独立 task, 慢链接不再阻塞定时槽与 /run

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

### Task 7: 删死依赖 thiserror + clippy 清零

**Files:**
- Modify: `Cargo.toml`、`src/sink/telegram.rs`（2 处 clippy）

- [ ] **Step 1: 删 thiserror** — 从 `Cargo.toml` `[dependencies]` 删除 `thiserror = "1"` 行。

- [ ] **Step 2: clippy 修复**（`src/sink/telegram.rs`）：
  - `cleanup_stale` 里 `.map_or(false, |n| n.starts_with("hanabi_"))` → `.is_some_and(|n| n.starts_with("hanabi_"))`。
  - `TelegramSink::new` 的 client 构造，`.trust_dns(true)` 上方加 `#[allow(deprecated)]`（保留行为、消警告）：

```rust
        #[allow(deprecated)]
        let client = teloxide::net::default_reqwest_settings()
            .timeout(std::time::Duration::from_secs(300))
            .connect_timeout(std::time::Duration::from_secs(15))
            .trust_dns(true)
            .build()
            .context("构造 reqwest client 失败")?;
```

- [ ] **Step 3: 验证** — `cargo clippy --all-targets -- -D warnings`，预期零告警；`cargo test` 全绿。

- [ ] **Step 4: 提交**

```bash
git add Cargo.toml Cargo.lock src/sink/telegram.rs
git commit -m "chore: 删除死依赖 thiserror, clippy 清零(is_some_and / allow deprecated trust_dns)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

### Task 8: CI 加 lint job + aarch64 产物

**Files:**
- Modify: `.github/workflows/build.yml`
- 先跑一次 `cargo fmt` 归一化（现存代码未格式化，否则 fmt-check 会红）。

- [ ] **Step 1: 格式化归一化** — `cargo fmt`，单独提交：

```bash
cargo fmt
git add -u
git commit -m "style: cargo fmt 全库归一化(为 CI fmt-check 铺路)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

- [ ] **Step 2: 加 lint job** — 在 `.github/workflows/build.yml` 的 `jobs:` 下、`build-musl` 之前插入：

```yaml
  lint:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: rustup component add clippy rustfmt
      - name: Format check
        run: cargo fmt --check
      - name: Clippy
        run: cargo clippy --all-targets -- -D warnings
```

并把触发条件从仅 tag 扩展到 push/PR（顶部 `on:`）：

```yaml
on:
  push:
    branches: [master]
    tags:
      - 'v*'
  pull_request:
    branches: [master]
  workflow_dispatch:
```

- [ ] **Step 3: 加 aarch64 产物** — 把 `build-musl` job 改为矩阵：

```yaml
  build-musl:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        target: [x86_64-unknown-linux-musl, aarch64-unknown-linux-musl]
    steps:
      - uses: actions/checkout@v4
      - name: Install musl + cross tools
        run: |
          sudo apt-get update
          sudo apt-get install -y musl-tools
          if [ "${{ matrix.target }}" = "aarch64-unknown-linux-musl" ]; then
            sudo apt-get install -y gcc-aarch64-linux-gnu
          fi
      - name: Add target
        run: rustup target add ${{ matrix.target }}
      - name: Test (x86_64 only)
        if: matrix.target == 'x86_64-unknown-linux-musl'
        run: cargo test --release
      - name: Build
        env:
          CC_x86_64_unknown_linux_musl: musl-gcc
          CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER: musl-gcc
          CC_aarch64_unknown_linux_musl: aarch64-linux-gnu-gcc
          CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER: aarch64-linux-gnu-gcc
        run: cargo build --release --target ${{ matrix.target }}
      - name: Package
        run: |
          cd target/${{ matrix.target }}/release
          tar czf hanabi-${{ matrix.target }}.tar.gz hanabi
          sha256sum hanabi-${{ matrix.target }}.tar.gz > hanabi-${{ matrix.target }}.tar.gz.sha256
      - name: Release
        if: startsWith(github.ref, 'refs/tags/')
        uses: softprops/action-gh-release@v2
        with:
          files: |
            target/${{ matrix.target }}/release/hanabi-${{ matrix.target }}.tar.gz
            target/${{ matrix.target }}/release/hanabi-${{ matrix.target }}.tar.gz.sha256
```

- [ ] **Step 4: 本地验证可验证部分** — `cargo fmt --check`（应通过）、`cargo clippy --all-targets -- -D warnings`（零告警）。CI 矩阵/aarch64 编译只能 push 后在 Actions 验证。

- [ ] **Step 5: 提交**

```bash
git add .github/workflows/build.yml
git commit -m "ci: 加 clippy/fmt lint job + push/PR 触发, Release 增 aarch64-musl 产物

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

**PR1 收尾验收：** `cargo test` 全绿、`cargo clippy --all-targets -- -D warnings` 零告警、`cargo fmt --check` 通过。配 `channel_id="@x"` 启动见 error 日志；改 `tz_offset_hours` 日志「下次抓取」时间随之变化。

---

# PR 2 — 手动链接收紧（单作品直发 / 多作品进审批）

### Task 1: 链接分类纯函数

**Files:**
- Modify: `src/sink/telegram.rs`（改 `extract_supported_url` 为 host 判定 + 新增 `classify_link`）
- Test: `src/sink/telegram.rs`（`#[cfg(test)]`）

**Interfaces:**
- Produces:
  - `fn supported_host(url: &str) -> bool`（host 属于 pixiv.net/x.com/twitter.com）
  - `enum LinkKind { Single, Multi }`
  - `fn classify_link(url: &str) -> Option<LinkKind>`（受支持域名才 Some）

- [ ] **Step 1: 写失败测试**（替换原 `extract_url_*` 两个测试，新增）：

```rust
#[test]
fn classify_single_vs_multi() {
    use super::{classify_link, LinkKind};
    assert_eq!(classify_link("https://www.pixiv.net/artworks/123"), Some(LinkKind::Single));
    assert_eq!(classify_link("https://www.pixiv.net/i/123"), Some(LinkKind::Single));
    assert_eq!(classify_link("https://x.com/user/status/9"), Some(LinkKind::Single));
    assert_eq!(classify_link("https://twitter.com/u/status/7"), Some(LinkKind::Single));
    assert_eq!(classify_link("https://www.pixiv.net/users/555"), Some(LinkKind::Multi));
    assert_eq!(classify_link("https://www.pixiv.net/ranking.php?mode=weekly"), Some(LinkKind::Multi));
    assert_eq!(classify_link("https://x.com/i/lists/42"), Some(LinkKind::Multi));
    // 子串伪装域名被 host 判定挡掉。
    assert_eq!(classify_link("https://evil.com/pixiv.net/artworks/1"), None);
    assert_eq!(classify_link("https://example.com/a"), None);
}

#[test]
fn extract_url_finds_supported_in_text() {
    assert_eq!(
        extract_supported_url("看这张 https://x.com/u/status/9 不错").as_deref(),
        Some("https://x.com/u/status/9")
    );
    assert!(extract_supported_url("/run").is_none());
    assert!(extract_supported_url("https://evil.com/pixiv.net/x").is_none());
}
```

- [ ] **Step 2: 跑测试确认失败** — `cargo test --lib telegram::tests`，预期 FAIL（`classify_link`/`LinkKind` 未定义、`extract` 仍走子串）。

- [ ] **Step 3: 实现** — 在 `src/sink/telegram.rs` 替换/新增：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    Single,
    Multi,
}

/// 取 http(s) URL 的 host(小写)。无 scheme 或畸形返回 None。
fn url_host(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.split('@').last().unwrap_or(host); // 去掉 userinfo
    let host = host.split(':').next().unwrap_or(host); // 去掉端口
    if host.is_empty() {
        None
    } else {
        Some(host.to_lowercase())
    }
}

/// host 是否属于受支持站点(精确后缀匹配, 防子串伪装)。
fn supported_host(url: &str) -> bool {
    matches!(url_host(url), Some(h)
        if h == "pixiv.net" || h.ends_with(".pixiv.net")
        || h == "x.com" || h.ends_with(".x.com")
        || h == "twitter.com" || h.ends_with(".twitter.com"))
}

/// 受支持站点的单作品/多作品分类。非受支持站点 → None。
pub fn classify_link(url: &str) -> Option<LinkKind> {
    if !supported_host(url) {
        return None;
    }
    let single = url.contains("/artworks/")
        || url.contains("/status/")
        || url.contains("/i/") && url_host(url).as_deref() == Some("www.pixiv.net");
    // 更稳妥: pixiv /i/<id> 与 /artworks/<id> 为单作品; x /status/<id> 单作品。
    let is_single = url.contains("/artworks/")
        || url.contains("/status/")
        || (url.contains("pixiv.net/i/"));
    let _ = single;
    Some(if is_single { LinkKind::Single } else { LinkKind::Multi })
}
```

并把 `extract_supported_url` 改为基于 `classify_link`：

```rust
/// 从消息文本提取首个受支持作品链接(host 精确判定)。
fn extract_supported_url(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|w| w.starts_with("http") && classify_link(w).is_some())
        .map(|s| s.to_string())
}
```

> 实现注：上面 `single` 中间变量是冗余草稿，落地时只保留 `is_single` 一段；此处保留是为说明判定边界，实现时删掉 `let single...` 与 `let _ = single;` 两行。

- [ ] **Step 4: 跑测试** — `cargo test --lib telegram::tests`，预期 PASS。

- [ ] **Step 5: 提交**

```bash
git add src/sink/telegram.rs
git commit -m "fix: 手动链接改 host 精确判定 + 单/多作品分类, 防子串伪装域名

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

### Task 2: 多作品链接走审批流

**Files:**
- Modify: `src/main.rs`（`handle_link`）
- 依赖 PR2 Task 1 的 `classify_link` / `LinkKind`（`pub`，从 `hanabi::sink::telegram` 引）。

**Interfaces:**
- Consumes: `hanabi::sink::telegram::{classify_link, LinkKind}`、已有 `sink.deliver`（`Sink` trait）、`sink.publish_direct`。

- [ ] **Step 1: 改 `handle_link`** — 在 probe 解析出 `items` 后，按 `classify_link(&job.url)` 分流：

```rust
    use hanabi::sink::telegram::{classify_link, LinkKind};
    let kind = classify_link(&job.url).unwrap_or(LinkKind::Single);

    if kind == LinkKind::Multi {
        // 多作品(主页/榜单/list): 逐个下载后进审批私聊, 不直发频道。
        let mut queued = 0;
        for item in &items {
            if store.already_pushed(item)? {
                continue;
            }
            let files = download_work(gdl, item, x_size);
            if files.is_empty() {
                continue;
            }
            if sink.deliver(item, &files).await.is_ok() {
                let _ = store.mark_pushed(item);
                queued += 1;
            }
        }
        sink.edit_review_text(
            job.notice_msg_id,
            &format!("📥 已转 {queued} 个作品进审批,请在审批消息上点按钮"),
        )
        .await;
        return Ok(());
    }

    // 单作品: 直发频道(原逻辑)。
    let mut published = 0;
    for item in &items {
        // ...保持原直发循环不变...
    }
```

> `sink: &TelegramSink` 已实现 `Sink`(`deliver`)与 `publish_direct`/`edit_review_text`，无需新增方法。`download_work` 已是 `main.rs` 内自由函数。

- [ ] **Step 2: 验证** — `cargo build && cargo test`，预期编译通过、测试全绿。

- [ ] **Step 3: 真机验证清单**（记入 PR 描述，人工执行）：
  - 私聊发单作品链接 `pixiv.net/artworks/<id>` → 直发频道。
  - 私聊发画师主页 `pixiv.net/users/<id>` → 多个作品进审批私聊、提示「已转 N 个进审批」。

- [ ] **Step 4: 提交**

```bash
git add src/main.rs
git commit -m "feat: 手动多作品链接(主页/榜单)改走审批流, 单作品仍直发频道

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

# PR 3 — Docker / GHCR 镜像

### Task 1: Dockerfile

**Files:**
- Create: `Dockerfile`、`.dockerignore`

- [ ] **Step 1: 写 `.dockerignore`**

```
target
.git
*.db
*.sqlite
config.toml
gallery-dl.conf
.env
docs
```

- [ ] **Step 2: 写 `Dockerfile`**（multi-stage：musl 静态编译 + alpine runtime 装 gallery-dl）

```dockerfile
# ---- builder: 编 musl 静态二进制 ----
FROM rust:1.95-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
RUN cargo build --release && \
    cp target/release/hanabi /hanabi

# ---- runtime: alpine + gallery-dl ----
FROM alpine:3.20
RUN apk add --no-cache python3 py3-pip ca-certificates && \
    pip install --no-cache-dir --break-system-packages gallery-dl
COPY --from=builder /hanabi /usr/local/bin/hanabi
WORKDIR /data
ENV HANABI_CONFIG=/data/config.toml
ENTRYPOINT ["hanabi"]
```

> runtime 用 glibc-free 的 alpine + 直接编译(非 musl target 交叉)，因 builder 已是 alpine(musl) 基础镜像，`cargo build --release` 默认即 musl 静态。`/data` 挂载 `config.toml` 与 `gallery-dl.conf`。

- [ ] **Step 3: 本地验证构建** —

```bash
docker build -t hanabi:dev .
```

预期：构建成功，最后输出镜像 id。（若本机无 docker，跳过，留 CI 验证。）

- [ ] **Step 4: 提交**

```bash
git add Dockerfile .dockerignore
git commit -m "feat: 多阶段 Dockerfile(alpine musl 静态 + gallery-dl runtime)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

### Task 2: CI 推送 GHCR + README

**Files:**
- Modify: `.github/workflows/build.yml`（加 docker job）、`README.md`（加 Docker 部署小节）

- [ ] **Step 1: 加 docker job**（`.github/workflows/build.yml` 的 `jobs:` 下）：

```yaml
  docker:
    runs-on: ubuntu-latest
    if: startsWith(github.ref, 'refs/tags/')
    permissions:
      contents: read
      packages: write
    steps:
      - uses: actions/checkout@v4
      - uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}
      - uses: docker/metadata-action@v5
        id: meta
        with:
          images: ghcr.io/furinelle/hanabi
          tags: |
            type=ref,event=tag
            type=raw,value=latest
      - uses: docker/build-push-action@v6
        with:
          context: .
          push: true
          tags: ${{ steps.meta.outputs.tags }}
```

- [ ] **Step 2: README 加 Docker 部署小节**（`## 部署` 下新增）：

```markdown
### Docker（GHCR 镜像）

```bash
docker run -d --name hanabi \
  -e HANABI_BOT_TOKEN="<bot token>" \
  -v $PWD/config.toml:/data/config.toml:ro \
  -v $PWD/gallery-dl.conf:/data/gallery-dl.conf:ro \
  -v $PWD/hanabi.db:/data/hanabi.db \
  ghcr.io/furinelle/hanabi:latest
```

镜像内含 gallery-dl，无需另装。`config.toml` 里 `gallery_dl.config_path` 设为 `/data/gallery-dl.conf`。
```

- [ ] **Step 3: 提交**

```bash
git add .github/workflows/build.yml README.md
git commit -m "ci: tag 时构建并推送 GHCR 镜像 + README Docker 部署说明

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

# PR 4 — 原图进评论区（最高风险，先 spike 校准）

> 前置（已确认）：`publish_channel` 已绑讨论组、bot 在讨论组有管理员/发言权限。

### Task 1: Spike — 真机校准 auto-forward 字段 + reply API

**Files:**
- Create: `tests/fixtures/auto_forward_update.json`（真机抓的一条 `Update`）
- Create: `docs/superpowers/notes/2026-06-25-autoforward-calibration.md`（记录字段路径与 teloxide 0.13 方法名）

- [ ] **Step 1: 真机抓样本** — 临时在 `run_review_loop` 的 `UpdateKind::Message(msg)` 分支加 `tracing::info!(?msg, "DEBUG raw msg")`（或 `serde_json` 打印原始 update），发一张图到频道触发 auto-forward，从讨论组那条消息日志中抓取并存成 `tests/fixtures/auto_forward_update.json`。完成后**移除该调试日志**。

- [ ] **Step 2: 记录校准结论** — 在 notes 文件写明：
  - `is_automatic_forward` 的取值方式（字段或 `msg.is_automatic_forward()`）。
  - 「源频道」「源消息 id」在 teloxide 0.13 的访问路径（`msg.forward_origin()` / `forward_from_chat()` / `forward_from_message_id()` 三选一，以真机为准）。
  - `send_media_group` 设置 reply 的方法名（`.reply_to_message_id(id)` 或 `.reply_parameters(...)`）。
  - `InputMediaDocument` 构造方式。

- [ ] **Step 3: 提交**

```bash
git add tests/fixtures/auto_forward_update.json docs/superpowers/notes/2026-06-25-autoforward-calibration.md
git commit -m "spike: 真机校准 auto-forward 字段与 teloxide reply API

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

### Task 2: pending 加 originals 列 + deliver 存原图

**Files:**
- Modify: `src/sink/telegram.rs`（建表/兼容、`deliver` 写 originals、`publish_direct`/`handle_callback` 取 originals）
- Test: `src/sink/telegram.rs`

**Interfaces:**
- Produces: `pending` 表新增 `originals TEXT`；`deliver` 同时持久化原始文件路径（缩放前）。
- Consumes: `deliver` 收到的 `files`（原始下载文件）与 `prepared`（缩放后）。

- [ ] **Step 1: 写失败测试**（验证建表含 originals 列）：

```rust
#[test]
fn pending_table_has_originals_column() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.db");
    let _sink = TelegramSink::new(
        "123:abc".into(),
        "7794592020".into(),
        "@chan".into(),
        path.to_str().unwrap(),
    )
    .unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    let cols: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('pending')")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .flatten()
        .collect();
    assert!(cols.contains(&"originals".to_string()));
}
```

> 注：`TelegramSink::new` 不发网络请求，仅构造 Bot 与建表，可在测试中创建。

- [ ] **Step 2: 跑测试确认失败** — `cargo test --lib telegram::tests::pending_table_has_originals_column`，预期 FAIL（无该列）。

- [ ] **Step 3: 实现** — `TelegramSink::new` 建表语句加 `originals` 列 + 兼容 ALTER：

```rust
                msg_ids    TEXT NOT NULL,
                originals  TEXT NOT NULL DEFAULT '[]',
                created_at INTEGER NOT NULL DEFAULT 0
```

在 `created_at` 兼容 ALTER 旁加：

```rust
        let _ = conn.execute(
            "ALTER TABLE pending ADD COLUMN originals TEXT NOT NULL DEFAULT '[]'",
            [],
        );
```

`deliver` 里把**原始 files**(未缩放)也序列化写入：

```rust
        let originals_str: Vec<String> = files_owned_for_originals
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let originals_json = serde_json::to_string(&originals_str)?;
```

并把 INSERT 改为 7 列(加 originals)。注意 `deliver` 现把 `files_owned` move 进 spawn_blocking 做缩放——在 move 前先克隆一份原始路径用于 originals：

```rust
        let files_owned: Vec<PathBuf> = files.to_vec();
        let originals = files.to_vec(); // 原始(缩放前)路径, 发 document 用
        let prepared = tokio::task::spawn_blocking(move || prepare_all(&files_owned)).await??;
```

- [ ] **Step 4: 跑测试** — `cargo test --lib telegram::tests`，预期 PASS。

- [ ] **Step 5: 提交**

```bash
git add src/sink/telegram.rs
git commit -m "feat: pending 加 originals 列, deliver 持久化原始(缩放前)文件路径

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

### Task 3: 评论区投递（用 Task 1 校准结论实现）

**Files:**
- Modify: `src/sink/telegram.rs`（`ReviewState` 加 `pending_comments` 映射、发布后登记、`run_review_loop` 捕获 auto-forward 投递、`cleanup_due` 兜底）
- Test: `src/sink/telegram.rs`（`match_auto_forward` 纯函数 + fixture）

**Interfaces:**
- Produces:
  - `ReviewState.pending_comments: Mutex<HashMap<i32, CommentJob>>`，`struct CommentJob { originals: Vec<PathBuf>, temp_dir: PathBuf, created_at: i64 }`
  - `fn match_auto_forward(update_msg: &Message, publish_channel: &Recipient) -> Option<i32>`（返回被转发的频道帖首条 msg_id）—— **签名/字段访问以 Task 1 校准为准**。
  - 登记函数 `register_comment(state, channel_first_msg_id, originals, temp_dir)`。

- [ ] **Step 1: 写 `match_auto_forward` 失败测试** — 用 `tests/fixtures/auto_forward_update.json` 反序列化为 `Update`，断言 `match_auto_forward` 返回 fixture 里那条频道帖 id（具体期望值读 fixture 后填）：

```rust
#[test]
fn match_auto_forward_extracts_channel_msg_id() {
    let raw = include_str!("../../tests/fixtures/auto_forward_update.json");
    let update: teloxide::types::Update = serde_json::from_str(raw).unwrap();
    let msg = match update.kind {
        teloxide::types::UpdateKind::Message(m) => m,
        _ => panic!("fixture 应为 Message update"),
    };
    let chan = to_recipient("-1001234567890".into()); // 用 fixture 里的源频道 id
    // <EXPECTED_ID> 读 fixture 后填入真实值
    assert_eq!(match_auto_forward(&msg, &chan), Some(/* <EXPECTED_ID> */ 0));
}
```

> 该测试的 `-100...` 频道 id 与 `<EXPECTED_ID>` 在 Task 1 拿到 fixture 后填真实值；这是「先 fixture 后实现」流程的必要占位，落地时必须替换为真值。

- [ ] **Step 2: 跑测试确认失败** — `cargo test --lib telegram::tests::match_auto_forward_extracts_channel_msg_id`，预期 FAIL（未定义）。

- [ ] **Step 3: 实现 `match_auto_forward`** — 依 Task 1 校准的字段路径写（示意，以校准为准）：

```rust
/// 若 msg 是 publish_channel 帖子的 auto-forward, 返回被转发的频道帖 msg_id。
fn match_auto_forward(msg: &Message, publish_channel: &Recipient) -> Option<i32> {
    if !msg.is_automatic_forward() {
        return None;
    }
    // 字段访问以 Task 1 校准为准: 取源频道与源消息 id, 源频道需匹配 publish_channel。
    let from_chat = msg.forward_from_chat()?;
    let matches_channel = match publish_channel {
        Recipient::Id(id) => from_chat.id == *id,
        Recipient::ChannelUsername(name) => {
            from_chat.username().map(|u| format!("@{u}")) == Some(name.clone())
        }
    };
    if !matches_channel {
        return None;
    }
    msg.forward_from_message_id()
}
```

- [ ] **Step 4: 加 `pending_comments` 与登记** — `ReviewState` 加字段；`CommentJob` 结构；发布成功处(`handle_callback` ok 分支、`publish_direct`)拿到频道帖首条 msg_id 后调用登记，并**不立即 cleanup**（改由评论区投递或兜底超时清理）。

- [ ] **Step 5: run_review_loop 捕获并投递** — 在 `UpdateKind::Message` 分支前置判断：若 `match_auto_forward` 命中，取出 `CommentJob`，`send_media_group(InputMediaDocument)` 以 reply 投递原图到讨论组那条消息，成功后 `cleanup(temp_dir)` 并移除映射。reply 方法名以 Task 1 校准为准。

- [ ] **Step 6: 兜底超时** — 在周期 `cleanup_due` 块里扫描 `pending_comments`，对 `now - created_at > 120` 的条目直接 `cleanup` 并移除（频道没绑讨论组/auto-forward 丢失时不泄漏临时文件）。

- [ ] **Step 7: 跑测试 + 验证** — `cargo test`（含 `match_auto_forward` 测试）；`cargo clippy -- -D warnings`。

- [ ] **Step 8: 真机验证清单**（PR 描述）：
  - 审批通过 → 频道出压缩大图，评论区出现整组原画质 document。
  - 兜底：人为停掉讨论组监听场景下 120s 后临时文件被清，无残留。

- [ ] **Step 9: 提交**

```bash
git add src/sink/telegram.rs
git commit -m "feat: 发布后原画质图投递到频道帖评论区(auto-forward 配对 + 兜底超时)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## 自审记录（Self-Review）

- **Spec 覆盖**：阶段一 9 项 ↔ PR1 Task1-8（缩放保 alpha=T4、周期清理=T5、handle_link=T6、thiserror/clippy=T7、CI=T8、tz=T2、owner=T3、锁=T1）。阶段二：手动链接=PR2、Docker=PR3、原图进评论区=PR4。R18 路由按用户决策**不做**，booru/AI/pHash/重试队列归阶段三**不在本计划**——与 spec 一致。
- **占位说明**：PR4 Task1 为真机 spike，Task3 的 fixture 期望值（频道 id、`<EXPECTED_ID>`）与 `match_auto_forward` 字段路径**故意延迟到拿到真机 fixture 再填**——这是 spec 标注的经验性未知，非计划缺陷。其余任务均为完整可执行代码。
- **类型一致性**：`classify_link`/`LinkKind`（PR2 Task1 定义，pub）被 PR2 Task2 跨 crate 引用；`CommentJob`/`match_auto_forward`/`pending_comments` 在 PR4 Task3 内自洽。
- **风险排序**：PR4 > PR1(锁/handle_link 重构) > PR3 > PR2。执行顺序 PR1 → PR2 → PR3 → PR4。
