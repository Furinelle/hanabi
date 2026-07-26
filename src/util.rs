use std::path::Path;

/// 临时目录权限收紧为 0700:目录名可预测(hanabi_<源>_<id>),多用户机器上
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
