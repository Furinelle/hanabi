use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use hanabi::config::Config;
use hanabi::filter::FilterChain;
use hanabi::gallerydl::GalleryDl;
use hanabi::model::{MediaItem, SourceKind};
use hanabi::pipeline::run_once;
use hanabi::sink::telegram::TelegramSink;
use hanabi::sink::Sink;
use hanabi::source::douyin::DouyinUserSource;
use hanabi::source::pixiv::PixivSource;
use hanabi::source::x::{download_extra, XSource};
use hanabi::source::Source;
use hanabi::store::Store;

/// 计算距下一个整点时间槽的秒数。`tz_offset_hours` 为本地时区相对 UTC 的偏移
/// (CST=+8)。`now_unix` 为当前 UTC 秒(注入便于测试)。
/// interval_secs 须能整除 86400，例如 28800 → 00:00 / 08:00 / 16:00。
fn secs_until_next_slot(interval_secs: u64, tz_offset_hours: i64, now_unix: u64) -> u64 {
    let local = (now_unix as i64 + tz_offset_hours * 3600).rem_euclid(86400) as u64;
    let next_slot = ((local / interval_secs) + 1) * interval_secs;
    next_slot - local
}

/// in-flight 链接集合的 RAII 守卫:任务结束(含 panic 展开)时移除 URL,
/// 防止 panic 后该链接被"已在处理中"永久吞掉。
struct InflightGuard {
    set: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    url: String,
}
impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Ok(mut s) = self.set.lock() {
            s.remove(&self.url);
        }
    }
}

/// 下载单个作品到独立临时目录(X 用 size=orig)。供定时抓取与手动链接共用。
/// 同步阻塞(子进程等待可达数分钟),调用方必须放 spawn_blocking。
fn download_work(gdl: &GalleryDl, item: &MediaItem, x_size: Option<&str>) -> Vec<PathBuf> {
    let dir = hanabi::util::pending_dir(item.source.as_str(), &item.source_id);
    let _ = std::fs::create_dir_all(&dir);
    hanabi::util::restrict_dir(&dir);
    let extra = match item.source {
        SourceKind::X => download_extra(x_size),
        SourceKind::Pixiv => vec![],
        SourceKind::Douyin => vec![], // 抖音不走 gallery-dl 下载,见 handle_douyin
    };
    gdl.download(&item.url, &dir, &extra).unwrap_or_else(|e| {
        tracing::warn!(id = %item.source_id, error = %e, "gallery-dl 下载失败");
        Vec::new()
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cfg_path = std::env::var("HANABI_CONFIG").unwrap_or_else(|_| "config.toml".into());
    let cfg = Config::load(&cfg_path).context("加载 config.toml 失败")?;
    // 校验:整点时间槽要求 poll_interval_secs 能整除 86400。
    // 0 会让 secs_until_next_slot 整数除零 panic(且是跑完首轮才崩),启动即拦截。
    if cfg.poll_interval_secs == 0 {
        anyhow::bail!("poll_interval_secs 不能为 0(建议 21600/28800/43200)");
    }
    if 86400 % cfg.poll_interval_secs != 0 {
        tracing::warn!(
            poll = cfg.poll_interval_secs,
            "poll_interval_secs 不能整除 86400,整点时间槽将不均匀(建议 21600/28800/43200)"
        );
    }
    let token = std::env::var("HANABI_BOT_TOKEN").context("缺少环境变量 HANABI_BOT_TOKEN")?;

    let store = Store::open("hanabi.db")?;
    let chain = FilterChain::standard();
    let gallery = if cfg.gallery.enabled() {
        match hanabi::gallery::GalleryClient::new(
            cfg.gallery.endpoint.clone(),
            cfg.gallery.resolved_token(),
        ) {
            Ok(c) => {
                tracing::info!(endpoint = %cfg.gallery.endpoint, "图库入库已启用(Vitrine)");
                Some(c)
            }
            Err(e) => {
                tracing::error!(error = %e, "图库客户端初始化失败,审批将不显示入库按钮");
                None
            }
        }
    } else {
        tracing::info!("未配置 [gallery],跳过图库入库");
        None
    };
    if cfg.gallery.enabled() && cfg.gallery.fingerprint_sync_interval_secs > 0 {
        let endpoint = cfg.gallery.endpoint.clone();
        let token = cfg.gallery.resolved_token();
        let interval = cfg.gallery.fingerprint_sync_interval_secs.max(300);
        tokio::spawn(async move {
            loop {
                match hanabi::gallery_sync::sync_gallery_fingerprints(
                    std::path::Path::new("hanabi.db"),
                    &endpoint,
                    &token,
                )
                .await
                {
                    Ok(summary) => tracing::info!(
                        listed = summary.listed,
                        imported = summary.imported,
                        unchanged = summary.unchanged,
                        failed = summary.failed,
                        "Vitrine 图片指纹同步完成"
                    ),
                    Err(error) => tracing::warn!(error = %error, "Vitrine 图片指纹同步失败"),
                }
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
        });
    }
    // Arc 包裹:手动链接处理移入独立 task 时需克隆共享。
    let sink = Arc::new(TelegramSink::new(
        token,
        cfg.telegram.channel_id.clone(),
        cfg.telegram.publish_channel.clone(),
        "hanabi.db",
        gallery,
    )?);
    // 手动触发通道:/run 命令经此通知抓取循环立即跑一轮。
    let (trigger_tx, mut trigger_rx) = tokio::sync::mpsc::channel::<()>(8);
    // 手动链接通道:发来的 Pixiv/X 作品链接(含消息 id)经此交抓取循环直发频道。
    let (link_tx, mut link_rx) = tokio::sync::mpsc::channel::<hanabi::sink::telegram::LinkJob>(16);
    // 启动审批回调 + 命令/链接轮询任务(与抓取循环并发运行)。
    tokio::spawn(hanabi::sink::telegram::run_review_loop(
        sink.state(),
        trigger_tx,
        link_tx,
    ));
    if let Ok(raw) = std::env::var("HANABI_PUBLISH_PENDING_TOKENS") {
        let tokens: Vec<i64> = raw
            .split(',')
            .filter_map(|part| part.trim().parse().ok())
            .collect();
        if !tokens.is_empty() {
            let summary = sink.publish_pending_tokens(&tokens, true).await;
            tracing::info!(
                requested = summary.requested,
                succeeded = summary.succeeded,
                failed = summary.failed,
                missing = summary.missing,
                busy = summary.busy,
                archive_failed = summary.archive_failed,
                "维护任务:指定审批发布并入库完成"
            );
        }
    }
    let gdl = Arc::new(GalleryDl {
        config_path: cfg.gallery_dl.config_path.clone(),
        probe_range: cfg.gallery_dl.probe_range.clone(),
    });

    let x_size = cfg.x_image.size.clone();
    let douyin_runtime = cfg.douyin.clone();
    // kind 白名单:拼错时旧逻辑静默按 X 源解析 pixiv 输出,恒 0 命中且无提示;
    // 启动即报错,尽早暴露配置问题。
    let mut sources: Vec<Box<dyn Source>> = Vec::new();
    for s in &cfg.sources {
        match s.kind.as_str() {
            "pixiv_user" | "pixiv_bookmarks" | "pixiv_ranking" => {
                sources.push(Box::new(PixivSource::new(s.clone(), gdl.clone())));
            }
            "x_list" | "x_foryou" => {
                sources.push(Box::new(XSource::new(s.clone(), gdl.clone())));
            }
            "douyin_user" => {
                sources.push(Box::new(DouyinUserSource::new(
                    s.clone(),
                    cfg.douyin.clone(),
                )?));
            }
            other => anyhow::bail!(
                "源 {} 的 kind 无效: {other}(可选: pixiv_user | pixiv_bookmarks | pixiv_ranking | x_list | x_foryou | douyin_user)",
                s.name
            ),
        }
    }

    // 下载闭包:复用 download_work。x_size 克隆给闭包,原值留给手动链接(handle_link)。
    // gallery-dl 是同步子进程等待,包 spawn_blocking 不占 tokio worker
    // (单核 VPS 上 worker 只有 1 个,同步阻塞会让审批按钮/命令全部冻结)。
    let gdl_dl = gdl.clone();
    let x_size_dl = x_size.clone();
    let douyin_client_dl = hanabi::source::douyin::build_client()?;
    let download = move |item: MediaItem| {
        let gdl = gdl_dl.clone();
        let x_size = x_size_dl.clone();
        let douyin_client = douyin_client_dl.clone();
        async move {
            if item.source == SourceKind::Douyin {
                let dir = hanabi::util::pending_dir("douyin", &item.source_id);
                let (files, failed) =
                    hanabi::source::douyin::download_images(&douyin_client, &item, &dir).await;
                if failed == 0 {
                    files
                } else {
                    tracing::warn!(
                        id = %item.source_id,
                        downloaded = files.len(),
                        failed,
                        "抖音作者作品下载不完整,不交付审批,下轮重试"
                    );
                    discard_download_dir(dir).await;
                    Vec::new()
                }
            } else {
                tokio::task::spawn_blocking(move || download_work(&gdl, &item, x_size.as_deref()))
                    .await
                    .unwrap_or_default()
            }
        }
    };

    // 手动链接 in-flight 去重:同一链接连发两次时,两个任务会在"查重→下载→发布"的
    // 窗口内都读到未推送,各自发布 → 频道重复;以 URL 粒度串行化。
    let inflight: Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

    // 启动立即跑首轮。
    if let Err(e) = run_once(
        &store,
        &sources,
        &chain,
        sink.as_ref() as &dyn Sink,
        &download,
    )
    .await
    {
        tracing::error!(error = %e, "本轮异常");
    }
    loop {
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let wait = secs_until_next_slot(cfg.poll_interval_secs, cfg.tz_offset_hours, now_unix);
        tracing::info!(
            wait_secs = wait,
            "下次抓取在 {:.1} 小时后",
            wait as f64 / 3600.0
        );
        // 整点时间槽到点 / /run 手动触发 → 跑一轮;手动链接 → 直发频道(不跑全量)。
        let do_fetch = tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(wait)) => true,
            _ = trigger_rx.recv() => {
                tracing::info!("收到 /run 手动触发,立即抓取");
                true
            }
            Some(job) = link_rx.recv() => {
                tracing::info!(url = %job.url, "收到手动链接,直发频道");
                // 移入独立 task:慢链接(probe+下载+上传)不阻塞定时槽与 /run。
                let gdl = gdl.clone();
                let sink = sink.clone();
                let x_size = x_size.clone();
                let douyin_runtime = douyin_runtime.clone();
                let inflight = inflight.clone();
                tokio::spawn(async move {
                    let url = job.url.clone();
                    let notice_id = job.notice_msg_id;
                    let user_msg_id = job.user_msg_id;
                    if !inflight.lock().unwrap().insert(url.clone()) {
                        tracing::info!(url = %url, "同一链接已在处理中,跳过重复触发");
                        sink.delete_review_messages(&[user_msg_id, notice_id]).await;
                        return;
                    }
                    // RAII 守卫:handle_link panic 时也移除 in-flight 标记,否则该
                    // URL 直到重启都会被"已在处理中"静默吞掉。
                    let _guard = InflightGuard {
                        set: inflight.clone(),
                        url: url.clone(),
                    };
                    if let Err(e) = handle_link(
                        job,
                        &gdl,
                        x_size.as_deref(),
                        &douyin_runtime,
                        &sink,
                    )
                    .await
                    {
                        tracing::warn!(error = %e, "手动链接处理失败");
                        // 失败反馈:把"抓取中"改成失败提示,不再永久残留
                        // (成功路径由 handle_link 自行删除这两条消息)。
                        // 截断:gallery-dl 失败可携带数 KB stderr,超 editMessageText
                        // 4096 上限会被拒,反馈静默失效。
                        let mut err_text = format!("{e}");
                        if err_text.chars().count() > 300 {
                            err_text = err_text.chars().take(300).collect::<String>() + "…";
                        }
                        sink.edit_review_text(notice_id, &format!("⚠️ 处理失败: {err_text}"))
                            .await;
                    }
                });
                false
            }
        };
        if do_fetch {
            if let Err(e) = run_once(
                &store,
                &sources,
                &chain,
                sink.as_ref() as &dyn Sink,
                &download,
            )
            .await
            {
                tracing::error!(error = %e, "本轮异常");
            }
        }
    }
}

/// 处理手动发来的作品链接:probe + 解析 + 下载,直接发布到频道(跳过审批)。
/// 发布前查去重,已发过的跳过,避免重复进频道。
async fn handle_link(
    job: hanabi::sink::telegram::LinkJob,
    gdl: &Arc<GalleryDl>,
    x_size: Option<&str>,
    douyin_runtime: &hanabi::config::DouyinCfg,
    sink: &TelegramSink,
) -> Result<()> {
    // 抖音:gallery-dl 不支持,走独立的 reqwest 解析路径(直发频道,同单作品)。
    if hanabi::source::douyin::is_douyin_url(&job.url) {
        return handle_douyin(job, douyin_runtime, sink).await;
    }
    // 自开 Store 连接:Store 持 rusqlite::Connection(!Sync)不能跨 spawn 共享;
    // Task1 已加 busy_timeout, 多连接并发安全。
    let store = Store::open("hanabi.db").context("handle_link 打开 Store 失败")?;
    let is_pixiv = job.url.contains("pixiv");
    // 裸画师主页规整为作品子页(pixiv→/artworks, X→/media):否则 gallery-dl 只回
    // type-6 Queue,probe 解析出 0 张图。
    let probe_url = if is_pixiv {
        hanabi::source::pixiv::normalize_profile_url(&job.url)
    } else {
        hanabi::source::x::normalize_profile_url(&job.url)
    };
    let g = gdl.clone();
    let u = probe_url;
    let val = tokio::task::spawn_blocking(move || g.probe(&u)).await??;
    let items = if is_pixiv {
        hanabi::gallerydl::parse_pixiv(&val, "manual")
    } else {
        hanabi::source::x::parse_twitter(&val, "manual")
    };

    use hanabi::sink::telegram::{classify_link, LinkKind, PublishOutcome};

    // 未解析出作品:不静默删提示,告知用户(旧行为是悄悄删光,用户以为已发布)。
    if items.is_empty() {
        sink.delete_review_messages(&[job.user_msg_id]).await;
        sink.edit_review_text(
            job.notice_msg_id,
            "ℹ️ 未从链接解析出可用作品(可能不支持、已删除或全部无图)",
        )
        .await;
        return Ok(());
    }

    // 多作品链接(画师主页/榜单/list): 逐个下载后进审批私聊, 不直发频道。
    if classify_link(&job.url) == Some(LinkKind::Multi) {
        let mut delivered = 0usize;
        let mut failed = 0usize; // 下载 0 张 + 交付失败
        for item in &items {
            if store.already_pushed(item)? {
                continue;
            }
            let files = download_in_blocking(gdl, item, x_size).await;
            if files.is_empty() {
                // gallery-dl 失败被吞成空 Vec(cookie 过期/限流时必现),计入失败反馈。
                failed += 1;
                continue;
            }
            match sink.deliver(item, &files).await {
                Ok(()) => {
                    delivered += 1;
                    if let Err(e) = store.mark_pushed(item) {
                        tracing::warn!(id = %item.source_id, error = %e, "标记已推送失败,可能重复投递");
                    }
                }
                // 失败不静默:该作品这次不入库,重发同一链接可重试。
                Err(e) => {
                    failed += 1;
                    tracing::warn!(id = %item.source_id, error = %e, "手动链接作品交付审批失败");
                }
            }
        }
        // 处理完:删链接消息;全部顺利才删"抓取中"提示,否则改成失败摘要。
        if failed > 0 {
            sink.delete_review_messages(&[job.user_msg_id]).await;
            sink.edit_review_text(
                job.notice_msg_id,
                &format!(
                    "⚠️ {failed} 个作品下载/投递失败(本次进审批 {delivered} 个);失败项未入库,可重发链接重试"
                ),
            )
            .await;
        } else {
            sink.delete_review_messages(&[job.user_msg_id, job.notice_msg_id])
                .await;
        }
        return Ok(());
    }

    // 单作品链接: 直发频道(跳过审批,手动=已选定)。
    let mut partial = 0usize;
    let mut failed = 0usize;
    for item in &items {
        if store.already_pushed(item)? {
            tracing::info!(id = %item.source_id, "手动链接作品已发过,跳过");
            continue;
        }
        let files = download_in_blocking(gdl, item, x_size).await;
        if files.is_empty() {
            failed += 1;
            continue;
        }
        let outcome = sink.publish_direct(item, &files).await?;
        // Partial 也入库:频道帖已存在,重发整条会重复,与 finish_claimed 收尾一致。
        if let Err(e) = store.mark_pushed(item) {
            tracing::warn!(id = %item.source_id, error = %e, "标记已推送失败,重发链接会重复发布");
        }
        if outcome == PublishOutcome::Partial {
            partial += 1;
        }
    }
    // 处理完:删用户链接消息;有异常时"抓取中"改成结果摘要,否则一并删除。
    if failed > 0 || partial > 0 {
        sink.delete_review_messages(&[job.user_msg_id]).await;
        let mut parts: Vec<String> = Vec::new();
        if failed > 0 {
            parts.push(format!("{failed} 个作品下载失败(未发布,可重发重试)"));
        }
        if partial > 0 {
            parts.push(format!(
                "{partial} 个作品部分图片发布失败(已按已发布记录,请人工检查频道帖补图)"
            ));
        }
        sink.edit_review_text(job.notice_msg_id, &format!("⚠️ {}", parts.join(";")))
            .await;
    } else {
        sink.delete_review_messages(&[job.user_msg_id, job.notice_msg_id])
            .await;
    }
    Ok(())
}

/// download_work 的 async 包装:gallery-dl 同步子进程放 spawn_blocking 执行。
async fn download_in_blocking(
    gdl: &Arc<GalleryDl>,
    item: &MediaItem,
    x_size: Option<&str>,
) -> Vec<PathBuf> {
    let gdl = gdl.clone();
    let item = item.clone();
    let x_size = x_size.map(str::to_string);
    tokio::task::spawn_blocking(move || download_work(&gdl, &item, x_size.as_deref()))
        .await
        .unwrap_or_default()
}

async fn discard_download_dir(dir: std::path::PathBuf) {
    let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(dir)).await;
}

fn douyin_download_complete(downloaded: usize, failed: usize) -> bool {
    downloaded > 0 && failed == 0
}

/// 抖音作者主页属于多作品链接：逐条下载后进入审批，与 Pixiv/X 主页一致。
async fn handle_douyin_user(
    job: hanabi::sink::telegram::LinkJob,
    items: Vec<MediaItem>,
    store: Store,
    client: &reqwest::Client,
    sink: &TelegramSink,
) -> Result<()> {
    let mut delivered = 0usize;
    let mut failed = 0usize;
    for item in &items {
        if store.already_pushed(item)? {
            continue;
        }
        let dir = hanabi::util::pending_dir("douyin", &item.source_id);
        let (files, image_failures) =
            hanabi::source::douyin::download_images(client, item, &dir).await;
        if image_failures > 0 || files.is_empty() {
            failed += 1;
            tracing::warn!(
                id = %item.source_id,
                downloaded = files.len(),
                failed = image_failures,
                "抖音作者作品下载不完整,未交付审批"
            );
            discard_download_dir(dir).await;
            continue;
        }
        match sink.deliver(item, &files).await {
            Ok(()) => {
                delivered += 1;
                if let Err(e) = store.mark_pushed(item) {
                    tracing::warn!(id = %item.source_id, error = %e, "标记已推送失败,可能重复投递");
                }
            }
            Err(e) => {
                failed += 1;
                tracing::warn!(id = %item.source_id, error = %e, "抖音作者作品交付审批失败");
            }
        }
    }

    sink.delete_review_messages(&[job.user_msg_id]).await;
    if failed > 0 {
        sink.edit_review_text(
            job.notice_msg_id,
            &format!(
                "⚠️ {failed} 个作品下载/投递失败(本次进审批 {delivered} 个);失败项未入库,可重发链接重试"
            ),
        )
        .await;
    } else {
        sink.delete_review_messages(&[job.notice_msg_id]).await;
    }
    Ok(())
}

/// 处理抖音链接：单个图文直发频道；作者主页批量抓取后进入审批。
async fn handle_douyin(
    job: hanabi::sink::telegram::LinkJob,
    runtime: &hanabi::config::DouyinCfg,
    sink: &TelegramSink,
) -> Result<()> {
    use hanabi::source::douyin;
    let store = Store::open("hanabi.db").context("handle_douyin 打开 Store 失败")?;
    let client = douyin::build_client()?;
    let resolved_url = match douyin::resolve_douyin_url(&client, &job.url).await {
        Ok(url) => Some(url),
        Err(error) => {
            tracing::warn!(error = %error, "抖音短链预解析失败,继续走原链接重试");
            None
        }
    };
    let resolved_target = resolved_url
        .as_ref()
        .map(reqwest::Url::as_str)
        .unwrap_or(&job.url);

    // 明确落到作者主页时直接走作者桥；既避免 note 页三次无效解析，也避免 Python
    // 再解析一遍短链时遇到偶发超时。
    if resolved_url
        .as_ref()
        .is_some_and(douyin::is_user_profile_url)
    {
        match douyin::fetch_user_feed(runtime, resolved_target, "manual").await {
            Ok(items) if !items.is_empty() => {
                tracing::info!(items = items.len(), "手动抖音作者主页解析成功,进入批量审批");
                return handle_douyin_user(job, items, store, &client, sink).await;
            }
            Ok(_) => {
                sink.delete_review_messages(&[job.user_msg_id]).await;
                sink.edit_review_text(job.notice_msg_id, "ℹ️ 该作者主页没有可用图文作品")
                    .await;
                return Ok(());
            }
            Err(error) => {
                tracing::warn!(error = %error, "抖音作者主页解析失败");
                sink.delete_review_messages(&[job.user_msg_id]).await;
                sink.edit_review_text(
                    job.notice_msg_id,
                    "ℹ️ 抖音作者主页解析失败(可能需刷新 Cookie)",
                )
                .await;
                return Ok(());
            }
        }
    }

    match douyin::fetch_note(&client, runtime, resolved_target, "manual").await {
        Ok(item) => {
            // 结束时留给用户的提示;None = 全部顺利,静默删掉两条消息即可。
            let mut notice: Option<String> = None;
            if store.already_pushed(&item)? {
                tracing::info!(id = %item.source_id, "抖音作品已发过,跳过");
            } else {
                let dir = hanabi::util::pending_dir("douyin", &item.source_id);
                let (files, failed) = douyin::download_images(&client, &item, &dir).await;
                let download_complete = douyin_download_complete(files.len(), failed);
                if failed > 0 {
                    debug_assert!(!download_complete);
                    // 原子发布:重试/备用 CDN 后仍缺任何一张时,整条不发布也不 mark_pushed。
                    // 否则重发链接会因去重跳过,或发布完整作品时重复已发的部分图片。
                    tracing::warn!(
                        id = %item.source_id,
                        downloaded = files.len(),
                        failed,
                        "抖音图片重试后仍下载不完整,本次未发布"
                    );
                    discard_download_dir(dir).await;
                    notice = Some(if files.is_empty() {
                        "⚠️ 抖音图片重试后仍全部下载失败,本次未发布;请重新发送链接重试".to_string()
                    } else {
                        format!(
                            "⚠️ {failed} 张图片重试后仍下载失败,为避免频道帖不完整本次未发布;请重新发送链接重试"
                        )
                    });
                } else if !download_complete {
                    tracing::warn!(id = %item.source_id, "抖音作品没有可发布图片");
                    notice = Some("⚠️ 抖音作品没有可发布图片,未发布".to_string());
                } else {
                    let outcome = sink.publish_direct(&item, &files).await?;
                    if let Err(e) = store.mark_pushed(&item) {
                        tracing::warn!(id = %item.source_id, error = %e, "标记已推送失败");
                    }
                    if outcome == hanabi::sink::telegram::PublishOutcome::Partial {
                        tracing::warn!(id = %item.source_id, "抖音直发部分成功,频道帖不完整");
                        notice = Some(
                            "⚠️ 部分图片发布失败,频道帖不完整;已按已发布记录,请人工补图"
                                .to_string(),
                        );
                    }
                }
            }
            match notice {
                // 全部顺利:删链接消息 + "抓取中"提示,保持私聊干净。
                None => {
                    sink.delete_review_messages(&[job.user_msg_id, job.notice_msg_id])
                        .await;
                }
                // 有异常:删链接消息,"抓取中"改成结果提示留给用户看。
                Some(text) => {
                    sink.delete_review_messages(&[job.user_msg_id]).await;
                    sink.edit_review_text(job.notice_msg_id, &text).await;
                }
            }
        }
        Err(note_error) => {
            // 作者短链最终会落到 /share/user/...，自然没有 note 页的 _ROUTER_DATA。
            // 用作者桥接器再判一次：成功即按多作品主页处理；两条路径都失败才反馈解析失败。
            match douyin::fetch_user_feed(runtime, &job.url, "manual").await {
                Ok(items) if !items.is_empty() => {
                    tracing::info!(items = items.len(), "手动抖音作者主页解析成功,进入批量审批");
                    handle_douyin_user(job, items, store, &client, sink).await?;
                }
                Ok(_) => {
                    tracing::warn!(error = %note_error, "抖音作者主页没有可用图文作品");
                    sink.delete_review_messages(&[job.user_msg_id]).await;
                    sink.edit_review_text(job.notice_msg_id, "ℹ️ 该作者主页没有可用图文作品")
                        .await;
                }
                Err(user_error) => {
                    tracing::warn!(note_error = %note_error, user_error = %user_error, "抖音解析失败");
                    sink.delete_review_messages(&[job.user_msg_id]).await;
                    sink.edit_review_text(job.notice_msg_id, "ℹ️ 抖音解析失败(可能改版或需验证)")
                        .await;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{douyin_download_complete, secs_until_next_slot};

    #[test]
    fn slot_aligns_to_interval_in_local_tz() {
        // now=57600 即当天 UTC 16:00:00 → CST(+8) 次日 00:00 整点槽起点,距下一个 8h 槽 8h。
        let now = 57600;
        assert_eq!(secs_until_next_slot(28800, 8, now), 28800);
        // local 04:00(now=72000),距下一个 08:00 槽 4h。
        assert_eq!(secs_until_next_slot(28800, 8, 72000), 14400);
        // tz_offset=0 时同一 now=57600 → local 16:00,距 24:00 槽 8h。
        assert_eq!(secs_until_next_slot(28800, 0, 57600), 28800);
    }

    #[test]
    fn douyin_partial_download_is_not_publishable() {
        assert!(douyin_download_complete(4, 0));
        assert!(!douyin_download_complete(3, 1));
        assert!(!douyin_download_complete(0, 4));
        assert!(!douyin_download_complete(0, 0));
    }
}
