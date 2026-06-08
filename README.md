# Hanabi

二次元图片 Telegram 推送 bot。Pixiv(画师/收藏/榜单)+ X(List/为你推荐)→ 去重过滤 → 频道相册。

## 前置
- Rust(`cargo build --release`)
- gallery-dl:`pipx install gallery-dl`
- Pixiv refresh_token + X cookies 填入 `gallery-dl.conf`(参考 `gallery-dl.conf.example`)

## 运行
1. `cp config.example.toml config.toml` 并改 channel_id / sources
2. `export HANABI_BOT_TOKEN=<bot token>`
3. `cargo run --release`

## 常驻(macOS)
`deploy/ai.hanabi.plist` → `~/Library/LaunchAgents/`,改占位后 `launchctl load`。

## 架构
单 Rust 二进制:`Source`(gallery-dl 抓取后端)→ `FilterChain` → `TelegramSink`,sqlite 去重,tokio 定时轮询。设计/计划见 `docs/superpowers/`。
