use std::path::{Path, PathBuf};

/// 待审图片必须跨重启保留。默认放在工作目录下的 `pending/`，部署时工作目录为
/// `/opt/hanabi`；可用 `HANABI_PENDING_ROOT` 覆盖。
pub fn pending_root() -> PathBuf {
    std::env::var_os("HANABI_PENDING_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("pending")
        })
}

/// 单作品的持久工作目录。source/source_id 均来自内部枚举和已校验的作品 ID。
pub fn pending_dir(source: &str, source_id: &str) -> PathBuf {
    pending_root().join(format!("hanabi_{source}_{source_id}"))
}

/// 同一作品的"查重→下载→交付→标记"互斥:定时抓取与手动链接并发时,避免两边写同一个
/// pending 目录或重复交付。按作品哈希分片,不同作品互不等待(偶尔同片只多等一个作品)。
pub async fn work_lock(source: &str, source_id: &str) -> tokio::sync::MutexGuard<'static, ()> {
    use std::hash::{Hash, Hasher};
    static SHARDS: [tokio::sync::Mutex<()>; 32] = [const { tokio::sync::Mutex::const_new(()) }; 32];
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (source, source_id).hash(&mut hasher);
    SHARDS[hasher.finish() as usize % SHARDS.len()].lock().await
}

/// 待审目录权限收紧为 0700:目录名可预测(hanabi_<源>_<id>),多用户机器上
/// 不给其他本地用户预读待审图片(可能含 R18)。非 unix 平台为空操作。
pub fn restrict_dir(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn work_lock_serializes_the_same_work() {
        let held = work_lock("test", "work_lock_same").await;
        let second = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            work_lock("test", "work_lock_same"),
        );
        assert!(second.await.is_err(), "同一作品必须等前一个处理完");
        drop(held);
        drop(work_lock("test", "work_lock_same").await);
    }
}
