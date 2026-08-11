#!/usr/bin/env python3
"""Hanabi <-> jiji262/douyin-downloader bridge.

The upstream project owns the volatile Douyin signing/session/browser logic.
This bridge keeps Hanabi's contract deliberately small: JSON request on stdin,
JSON response on stdout, diagnostics on stderr.  Cookies are read only from
HANABI_DOUYIN_COOKIE or HANABI_DOUYIN_COOKIE_FILE and never echoed.
"""

from __future__ import annotations

import asyncio
import importlib.util
import json
import logging
import os
import re
import sys
from pathlib import Path
from typing import Any


def _load_upstream():
    try:
        # Importing ``core.api_client`` normally executes upstream core/__init__.py,
        # which eagerly imports the complete downloader stack (ffmpeg/aiofiles/etc.).
        # Load this self-contained module by file path so the discovery bridge only
        # needs aiohttp + pyyaml + gmssl.
        core_spec = importlib.util.find_spec("core")
        if not core_spec or not core_spec.submodule_search_locations:
            raise ModuleNotFoundError("douyin-downloader core package not found")
        api_path = Path(next(iter(core_spec.submodule_search_locations))) / "api_client.py"
        api_spec = importlib.util.spec_from_file_location(
            "_hanabi_douyin_api_client", api_path
        )
        if not api_spec or not api_spec.loader:
            raise ModuleNotFoundError("douyin-downloader api_client.py not found")
        api_module = importlib.util.module_from_spec(api_spec)
        api_spec.loader.exec_module(api_module)
        # 上游默认每个进程随机挑 Windows/macOS UA；Cookie/风控指纹跨轮询切换会
        # 导致刚由浏览器刷新的会话下一轮又返回空 200。固定使用其 Windows UA。
        if getattr(api_module, "_USER_AGENT_POOL", None):
            api_module._USER_AGENT_POOL[:] = [api_module._USER_AGENT_POOL[0]]
        DouyinAPIClient = api_module.DouyinAPIClient
        from utils.cookie_utils import parse_cookie_header
        from utils.logger import set_console_log_level
        from utils.validators import is_short_url, normalize_short_url
    except Exception as exc:  # pragma: no cover - exercised by deployment self-check
        raise RuntimeError(
            "缺少 jiji262/douyin-downloader 2.x；请按 README 安装固定提交"
        ) from exc
    set_console_log_level(logging.WARNING)
    return (
        DouyinAPIClient,
        parse_cookie_header,
        is_short_url,
        normalize_short_url,
    )


def _read_request() -> dict[str, Any]:
    try:
        value = json.load(sys.stdin)
    except Exception as exc:
        raise RuntimeError("stdin 不是有效 JSON") from exc
    if not isinstance(value, dict):
        raise RuntimeError("stdin JSON 顶层必须是对象")
    return value


def _extract_target(value: Any) -> str:
    text = str(value or "").strip()
    match = re.search(r"https?://[^\s]+", text)
    if match:
        # 兼容复制分享文本末尾的中英文标点。
        return match.group(0).rstrip(".,;:!?，。；：！？)]}>'\"")
    if re.fullmatch(r"[A-Za-z0-9_-]{20,}", text):
        return f"https://www.douyin.com/user/{text}"
    raise RuntimeError("target 必须是抖音作者主页、作者短链或 sec_user_id")


def _cookie_header() -> str:
    direct = os.environ.get("HANABI_DOUYIN_COOKIE", "").strip()
    if direct:
        return direct
    cookie_file = os.environ.get("HANABI_DOUYIN_COOKIE_FILE", "").strip()
    if not cookie_file:
        return ""
    path = Path(cookie_file).expanduser()
    if not path.exists():
        return ""
    raw = path.read_text(encoding="utf-8").strip()
    if not raw:
        return ""
    try:
        value = json.loads(raw)
    except json.JSONDecodeError:
        return raw
    if not isinstance(value, dict):
        raise RuntimeError("Cookie 文件 JSON 必须是 name -> value 对象")
    return "; ".join(f"{key}={val}" for key, val in value.items())


def _persist_browser_cookies(cookies: dict[str, Any]) -> None:
    """Persist refreshed anonymous/login cookies only when an explicit file is configured."""
    cookie_file = os.environ.get("HANABI_DOUYIN_COOKIE_FILE", "").strip()
    if not cookie_file or not cookies:
        return
    path = Path(cookie_file).expanduser()
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(f".{path.name}.tmp")
    tmp.write_text(
        json.dumps(cookies, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    try:
        os.chmod(tmp, 0o600)
    except OSError:
        pass
    tmp.replace(path)


def _aweme_id(item: Any) -> str:
    if not isinstance(item, dict):
        return ""
    value = item.get("aweme_id")
    text = str(value or "")
    return text if text.isdigit() else ""


def _append_unique(items: list[dict[str, Any]], seen: set[str], values: Any) -> None:
    if not isinstance(values, list):
        return
    for item in values:
        aweme_id = _aweme_id(item)
        if aweme_id and aweme_id not in seen and isinstance(item, dict):
            seen.add(aweme_id)
            items.append(item)


async def _run(request: dict[str, Any]) -> dict[str, Any]:
    (
        DouyinAPIClient,
        parse_cookie_header,
        is_short_url,
        normalize_short_url,
    ) = _load_upstream()

    target = _extract_target(request.get("target"))
    operation = str(request.get("operation") or "feed").strip().lower()
    if operation not in {"feed", "detail"}:
        raise RuntimeError(f"不支持的 operation: {operation}")
    max_pages = max(1, min(int(request.get("max_pages") or 3), 100))
    known_ids = {
        str(value)
        for value in (request.get("known_ids") or [])
        if str(value).isdigit()
    }
    browser_enabled = bool(request.get("browser_fallback"))
    browser_headless = bool(request.get("browser_headless"))
    cookies = parse_cookie_header(_cookie_header())

    all_items: list[dict[str, Any]] = []
    seen: set[str] = set()
    pages_fetched = 0
    restricted = False
    browser_used = False
    resolved_url = target
    expected_count = 0

    async with DouyinAPIClient(cookies) as client:
        if is_short_url(target):
            resolved = await client.resolve_short_url(normalize_short_url(target))
            if not resolved:
                raise RuntimeError("抖音短链解析失败")
            resolved_url = resolved

        if operation == "detail":
            match = re.search(r"/(?:note|video|slides)/(\d+)", resolved_url)
            if not match:
                raise RuntimeError(
                    f"链接没有解析为作品页: {resolved_url.split('?', 1)[0]}"
                )
            aweme_id = match.group(1)
            item = await client.get_video_detail(aweme_id)
            if not isinstance(item, dict) or _aweme_id(item) != aweme_id:
                raise RuntimeError("作品详情接口返回空数据，可能是 Cookie/签名失效或触发验证")
            return {
                "resolved_url": resolved_url.split("?", 1)[0],
                "item": item,
            }

        match = re.search(r"/user/([A-Za-z0-9_-]+)", resolved_url)
        if not match:
            raise RuntimeError(f"链接没有解析为作者主页: {resolved_url.split('?', 1)[0]}")
        sec_user_id = match.group(1)

        try:
            profile = await client.get_user_info(sec_user_id)
        except Exception:
            profile = None
        if isinstance(profile, dict):
            try:
                expected_count = max(0, int(profile.get("aweme_count") or 0))
            except (TypeError, ValueError):
                expected_count = 0

        cursor = 0
        for _ in range(max_pages):
            page = await client.get_user_post(sec_user_id, max_cursor=cursor, count=20)
            pages_fetched += 1
            page_items = page.get("items") or page.get("aweme_list") or []
            if not isinstance(page_items, list) or not page_items:
                restricted = bool(page.get("has_more")) or pages_fetched == 1
                break
            _append_unique(all_items, seen, page_items)

            if not bool(page.get("has_more")):
                break
            try:
                next_cursor = int(page.get("max_cursor") or 0)
            except (TypeError, ValueError):
                next_cursor = 0
            if next_cursor == cursor:
                restricted = True
                break
            cursor = next_cursor

        if restricted and browser_enabled:
            browser_used = True
            ids = await client.collect_user_post_ids_via_browser(
                sec_user_id,
                expected_count=expected_count,
                headless=browser_headless,
            )
            captured = client.pop_browser_post_aweme_items()
            for aweme_id in ids:
                if aweme_id in seen or aweme_id in known_ids:
                    continue
                item = captured.get(aweme_id)
                # post 接口在 aid=6383 下会返回图文项；DOM 额外发现但没有接口
                # payload 的 id 多为纯视频，Hanabi 本来就跳过，不逐条请求 detail。
                if isinstance(item, dict):
                    _append_unique(all_items, seen, [item])
            if ids:
                restricted = False
            else:
                # 即使页面 DOM/接口拦截没有拿到 id，浏览器访问也可能已经刷新 ttwid、
                # s_v_web_id 等匿名会话 Cookie；douyin-downloader 会把这些 Cookie
                # 同步回 API client。立刻用新会话再跑一次签名接口。
                cursor = 0
                for _ in range(max_pages):
                    page = await client.get_user_post(
                        sec_user_id, max_cursor=cursor, count=20
                    )
                    pages_fetched += 1
                    page_items = page.get("items") or page.get("aweme_list") or []
                    if not isinstance(page_items, list) or not page_items:
                        break
                    _append_unique(all_items, seen, page_items)
                    if not bool(page.get("has_more")):
                        restricted = False
                        break
                    try:
                        next_cursor = int(page.get("max_cursor") or 0)
                    except (TypeError, ValueError):
                        next_cursor = 0
                    if next_cursor == cursor:
                        break
                    cursor = next_cursor
                if all_items:
                    restricted = False
            _persist_browser_cookies(client.cookies)

        if not all_items and (restricted or expected_count > 0):
            suffix = "；Playwright 兜底没有取得作品" if browser_used else ""
            raise RuntimeError(
                "作者作品接口返回空列表，可能是 Cookie/签名失效或触发验证" + suffix
            )

    # Rust/SQLite 再做一次最终幂等；这里先过滤旧 id，减少跨进程 JSON 与解析开销。
    new_items = [item for item in all_items if _aweme_id(item) not in known_ids]
    return {
        "sec_user_id": sec_user_id,
        "resolved_url": resolved_url.split("?", 1)[0],
        "pages_fetched": pages_fetched,
        "restricted": restricted,
        "browser_fallback_used": browser_used,
        "items": new_items,
    }


def main() -> int:
    try:
        result = asyncio.run(_run(_read_request()))
        json.dump(result, sys.stdout, ensure_ascii=False, separators=(",", ":"))
        sys.stdout.write("\n")
        return 0
    except Exception as exc:
        print(f"douyin user feed bridge: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
