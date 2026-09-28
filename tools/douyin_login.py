#!/usr/bin/env python3
"""Douyin login and session cookie management utility for Hanabi.

Inspired by MediaCrawler & douyin-downloader:
- QR code scan login via Playwright (interactive browser)
- Manual cookie import (supports raw cookie header, JSON dict, or JSON list)
- Cookie status verification (/aweme/v1/web/user/profile/self/)
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import re
import sys
import time
from pathlib import Path
from typing import Any

# Ensure Hanabi tools and upstream packages are loadable
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from tools.douyin_user_feed import _load_upstream


def parse_cookie_input(raw_text: str) -> dict[str, str]:
    """Parse cookie text from various formats into a key-value dictionary."""
    text = raw_text.strip()
    if not text:
        return {}

    # Try JSON
    try:
        data = json.loads(text)
        if isinstance(data, dict):
            if "cookies" in data and isinstance(data["cookies"], list):
                # Playwright storageState
                return {
                    str(c["name"]): str(c["value"])
                    for c in data["cookies"]
                    if isinstance(c, dict) and "name" in c and "value" in c
                }
            return {str(k): str(v) for k, v in data.items()}
        elif isinstance(data, list):
            # Cookie-Editor / EditThisCookie export
            return {
                str(c["name"]): str(c["value"])
                for c in data
                if isinstance(c, dict) and "name" in c and "value" in c
            }
    except json.JSONDecodeError:
        pass

    # Fallback to key=value string (e.g. from DevTools Network header)
    if text.lower().startswith("cookie:"):
        text = text[7:].strip()

    result = {}
    for item in text.split(";"):
        item = item.strip()
        if not item or "=" not in item:
            continue
        k, v = item.split("=", 1)
        result[k.strip()] = v.strip()
    return result


def save_cookies(cookies: dict[str, str], output_path: Path) -> None:
    """Safely write cookies with restricted 0o600 permissions."""
    output_path.parent.mkdir(parents=True, exist_ok=True)
    tmp_path = output_path.with_name(f".{output_path.name}.tmp")
    tmp_path.write_text(
        json.dumps(cookies, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    try:
        os.chmod(tmp_path, 0o600)
    except OSError:
        pass
    tmp_path.replace(output_path)
    print(f"✅ 已保存 {len(cookies)} 个 Cookie 到: {output_path.resolve()}")


async def verify_login_state(cookies: dict[str, str], target_url: str | None = None) -> bool:
    """Verify cookies via Douyin self profile API and optional target author feed."""
    DouyinAPIClient = _load_upstream()[0]
    has_session = bool(cookies.get("sessionid") or cookies.get("sessionid_ss"))

    print("\n🔍 正在向抖音验证 Cookie 有效性...")
    async with DouyinAPIClient(cookies) as client:
        try:
            self_info = await client.get_self_info()
        except Exception as exc:
            print(f"⚠️ 无法获取当前登录用户信息: {exc}")
            self_info = None

        if self_info and isinstance(self_info, dict):
            nickname = self_info.get("nickname", "未知")
            uid = self_info.get("uid", "未知")
            unique_id = self_info.get("unique_id") or self_info.get("short_id") or "-"
            print(f"🎉 登录状态有效！")
            print(f"   👤 登录用户: {nickname}")
            print(f"   🆔 UID: {uid} (抖音号: {unique_id})")
        else:
            if has_session:
                print("⚠️ 包含 sessionid，但 /aweme/v1/web/user/profile/self/ 未返回用户信息，Cookie 可能已过期或需要重新验证。")
            else:
                print("⚠️ 当前 Cookie 为【未登录游客状态】（缺少 sessionid）。")
                print("   提示：部分公开作者可正常采集，但受限账号、小号或风控作者的主页作品将被平台隐藏。")

        if target_url:
            print(f"\n🎯 正在测试目标作者: {target_url}")
            resolved = await client.resolve_short_url(target_url)
            print(f"   落点 URL: {resolved}")
            match = re.search(r"/user/([A-Za-z0-9_-]+)", resolved or "")
            if match:
                sec_uid = match.group(1)
                info = await client.get_user_info(sec_uid)
                nickname = info.get("nickname") if isinstance(info, dict) else "未知"
                aweme_count = info.get("aweme_count") if isinstance(info, dict) else 0
                print(f"   作者: {nickname} (显示作品数: {aweme_count})")
                post = await client.get_user_post(sec_uid, max_cursor=0, count=20)
                items = post.get("items") or post.get("aweme_list") or []
                print(f"   作品列表接口返回数量: {len(items)} 条")
                if items:
                    print("   ✅ 测试成功！已能够正常获取该作者作品。")
                else:
                    print("   ⚠️ 作品接口仍返回 0 条（若为小号可能仍需有效登录态）。")

        return bool(self_info)


def get_chrome_executable() -> str | None:
    """Find native Google Chrome on the system, avoiding Chrome for Testing."""
    candidates = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ]
    for p in candidates:
        if os.path.exists(p):
            return p
    return None


async def run_qrcode_login(output_path: Path, headless: bool = False, timeout: int = 300) -> int:
    """Run interactive Playwright QR code login."""
    try:
        from playwright.async_api import async_playwright
    except ImportError:
        print("❌ 未安装 Playwright，请先在环境运行: pip install playwright && playwright install chromium")
        return 1

    print("=" * 60)
    print("🚀 启动本机 Google Chrome 进行抖音扫码登录")
    print("=" * 60)
    print("1. 浏览器窗口将自动打开并加载抖音首页")
    print("2. 请使用【抖音手机 App】扫描屏幕上的登录二维码")
    print("3. 手机确认登录后，程序将自动捕获登录 Cookie 并保存")
    print(f"4. 等待超时时间: {timeout} 秒")
    print("=" * 60)

    chrome_path = get_chrome_executable()
    launch_kwargs: dict[str, Any] = {
        "headless": headless,
        "args": [
            "--disable-blink-features=AutomationControlled",
            "--no-sandbox",
        ],
        "ignore_default_args": ["--enable-automation"],
    }
    if chrome_path:
        print(f"🌐 使用本机原生 Google Chrome: {chrome_path}")
        launch_kwargs["executable_path"] = chrome_path
    else:
        print("🌐 未检测到本机原生 Google Chrome，使用 Playwright 默认引擎")

    async with async_playwright() as p:
        browser = await p.chromium.launch(**launch_kwargs)
        context = await browser.new_context(
            user_agent="Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36",
            locale="zh-CN",
            viewport={"width": 1400, "height": 900},
        )
        page = await context.new_page()

        try:
            print("\n⏳ 正在打开抖音首页...")
            await page.goto("https://www.douyin.com/", wait_until="domcontentloaded", timeout=60000)
            await page.wait_for_timeout(2000)

            # Check if login modal should be triggered
            try:
                login_btn = page.locator("text='登录'").first
                if await login_btn.is_visible():
                    print("💡 点击登录按钮唤起登录弹窗...")
                    await login_btn.click()
            except Exception:
                pass

            print("\n👀 正在等待您扫码登录中...")
            start_time = time.monotonic()
            captured_cookies: dict[str, str] = {}

            while time.monotonic() - start_time < timeout:
                all_cookies = await context.cookies(["https://www.douyin.com", "https://douyin.com"])
                c_dict = {c["name"]: c["value"] for c in all_cookies}
                # Check for sessionid indicating successful login
                if c_dict.get("sessionid") or c_dict.get("sessionid_ss"):
                    print("\n🎉 检测到登录凭证 sessionid！正在同步完整会话...")
                    await page.wait_for_timeout(3000)  # wait for secondary auth tokens
                    final_cookies = await context.cookies(["https://www.douyin.com", "https://douyin.com"])
                    captured_cookies = {c["name"]: c["value"] for c in final_cookies}
                    break
                await asyncio.sleep(1.5)

            if not captured_cookies.get("sessionid") and not captured_cookies.get("sessionid_ss"):
                print("❌ 超时未检测到登录完成（缺少 sessionid）。")
                await browser.close()
                return 1

            save_cookies(captured_cookies, output_path)
            await browser.close()

            # Verify
            await verify_login_state(captured_cookies)
            return 0

        except Exception as exc:
            print(f"❌ 扫码登录发生异常: {exc}")
            await browser.close()
            return 1


def run_manual_import(output_path: Path) -> int:
    """Prompt user to paste cookies manually and save them."""
    print("=" * 60)
    print("📝 手动导入抖音 Cookie (MediaCrawler / 浏览器 F12 模式)")
    print("=" * 60)
    print("你可以直接粘贴以下任意一种格式：")
    print("1. 浏览器开发者工具 (F12 -> Network) 复制的 `Cookie: ...` 原始请求头")
    print("2. 浏览器插件 (Cookie-Editor / EditThisCookie) 导出的 JSON 列表")
    print("3. 键值对 JSON 对象: {\"sessionid\": \"...\", \"ttwid\": \"...\"}")
    print("=" * 60)
    print("请粘贴 Cookie 内容后按回车（如果在多行模式，按两次回车提交）：\n")

    lines = []
    while True:
        try:
            line = input()
            if not line and lines:
                break
            lines.append(line)
        except EOFError:
            break

    raw_text = "\n".join(lines).strip()
    if not raw_text:
        print("❌ 未收到任何 Cookie 输入。")
        return 1

    parsed = parse_cookie_input(raw_text)
    if not parsed:
        print("❌ 无法解析输入的 Cookie 内容。")
        return 1

    print(f"\n🔍 成功解析出 {len(parsed)} 个 Cookie 字段:")
    for k in sorted(parsed.keys()):
        masked = parsed[k][:6] + "..." + parsed[k][-4:] if len(parsed[k]) > 12 else parsed[k]
        print(f"   - {k}: {masked}")

    if not parsed.get("sessionid") and not parsed.get("sessionid_ss"):
        print("\n⚠️ 警告: 输入的 Cookie 中未找到 sessionid 或 sessionid_ss！")
        print("   若要访问受限作者主页，通常必须包含有效 sessionid。")
        choice = input("   是否仍要保存？(y/N): ").strip().lower()
        if choice != "y":
            print("已取消保存。")
            return 1

    save_cookies(parsed, output_path)
    asyncio.run(verify_login_state(parsed))
    return 0


def run_check(output_path: Path, target_url: str | None = None) -> int:
    """Check current cookie file validity."""
    if not output_path.exists():
        print(f"❌ Cookie 文件不存在: {output_path.resolve()}")
        return 1

    try:
        content = output_path.read_text(encoding="utf-8")
        parsed = parse_cookie_input(content)
    except Exception as exc:
        print(f"❌ 读取 Cookie 文件失败: {exc}")
        return 1

    print(f"📄 当前 Cookie 文件: {output_path.resolve()} (包含 {len(parsed)} 个字段)")
    has_session = bool(parsed.get("sessionid") or parsed.get("sessionid_ss"))
    print(f"🔑 是否包含登录凭据 sessionid: {'是' if has_session else '否 (游客态)'}")

    asyncio.run(verify_login_state(parsed, target_url))
    return 0


def main() -> int:
    default_output = Path(os.environ.get("HANABI_DOUYIN_COOKIE_FILE", ".douyin-cookies.json"))

    parser = argparse.ArgumentParser(
        description="抖音登录态与 Cookie 管理工具 (支持扫码登录与手动导入)",
    )
    parser.add_argument(
        "--output",
        "-o",
        type=Path,
        default=default_output,
        help=f"Cookie 保存文件路径 (默认: {default_output})",
    )
    parser.add_argument(
        "--manual",
        "-m",
        action="store_true",
        help="手动粘贴导入 Cookie (支持文本或 JSON 格式)",
    )
    parser.add_argument(
        "--check",
        "-c",
        action="store_true",
        help="检查当前保存的 Cookie 登录态与有效性",
    )
    parser.add_argument(
        "--target",
        "-t",
        type=str,
        help="可选: 验证时测试指定的目标作者主页/短链",
    )
    parser.add_argument(
        "--headless",
        action="store_true",
        help="以无头模式运行扫码 (默认 False 打开图形窗口)",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=180,
        help="扫码登录等待超时时间（秒，默认 180）",
    )

    args = parser.parse_args()

    if args.check:
        return run_check(args.output, args.target)
    elif args.manual:
        return run_manual_import(args.output)
    else:
        return asyncio.run(run_qrcode_login(args.output, args.headless, args.timeout))


if __name__ == "__main__":
    sys.exit(main())
