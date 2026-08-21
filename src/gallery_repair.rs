//! 图库定向修复入口的纯参数校验。

use anyhow::Result;

/// 把抖音作品 ID 规范化为 note URL；URL 必须仍属于抖音域名。
pub fn normalize_douyin_target(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(format!("https://www.douyin.com/note/{raw}"));
    }
    if (raw.starts_with("https://") || raw.starts_with("http://"))
        && crate::source::douyin::is_douyin_url(raw)
    {
        return Ok(raw.to_string());
    }
    anyhow::bail!("目标必须是抖音单作品 URL 或纯数字作品 ID")
}
