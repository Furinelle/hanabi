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
