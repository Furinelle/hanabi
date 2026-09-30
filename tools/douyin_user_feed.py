#!/usr/bin/env python3
"""Hanabi <-> jiji262/douyin-downloader bridge.

The upstream project owns the volatile Douyin signing/session/browser logic.
This bridge keeps Hanabi's contract deliberately small: JSON request on stdin,
JSON response on stdout, diagnostics on stderr.  Cookies are read only from
HANABI_DOUYIN_COOKIE or HANABI_DOUYIN_COOKIE_FILE and never echoed.
"""

from __future__ import annotations

import asyncio
import base64
import contextlib
import importlib.util
import json
import logging
import os
import re
import sys
import time
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit


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
    if isinstance(value, dict):
        # Playwright storage_state 格式: {"cookies": [...], "origins": [...]}
        if "cookies" in value and isinstance(value["cookies"], list):
            items = {
                str(item["name"]): str(item["value"])
                for item in value["cookies"]
                if isinstance(item, dict) and "name" in item and "value" in item
            }
            return "; ".join(f"{k}={v}" for k, v in items.items())
        return "; ".join(f"{key}={val}" for key, val in value.items())
    if isinstance(value, list):
        # Cookie-Editor / EditThisCookie 格式: [{"name": "...", "value": "..."}, ...]
        items = {
            str(item["name"]): str(item["value"])
            for item in value
            if isinstance(item, dict) and "name" in item and "value" in item
        }
        if items:
            return "; ".join(f"{k}={v}" for k, v in items.items())
    raise RuntimeError(
        "Cookie 文件 JSON 必须是 name -> value 对象、cookie 列表或原始 Cookie 字符串"
    )


def _persist_browser_cookies(cookies: dict[str, Any]) -> None:
    """Persist refreshed anonymous/login cookies only when an explicit file is configured."""
    cookie_file = os.environ.get("HANABI_DOUYIN_COOKIE_FILE", "").strip()
    if not cookie_file or not cookies:
        return
    path = Path(cookie_file).expanduser()
    existing: dict[str, Any] = {}
    if path.exists():
        try:
            raw = json.loads(path.read_text(encoding="utf-8"))
            if isinstance(raw, dict):
                existing = raw
        except Exception:
            pass
    merged = {**existing, **cookies}
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(f".{path.name}.tmp")
    tmp.write_text(
        json.dumps(merged, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    try:
        os.chmod(tmp, 0o600)
    except OSError:
        pass
    tmp.replace(path)


def _is_douyin_domain(host: str | None) -> bool:
    if not host:
        return False
    h = host.lower()
    return (
        h == "douyin.com"
        or h.endswith(".douyin.com")
        or h == "iesdouyin.com"
        or h.endswith(".iesdouyin.com")
    )


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


def _browser_aweme(data: Any, aweme_id: str) -> dict[str, Any] | None:
    if not isinstance(data, dict):
        return None
    values: list[Any] = []
    for key in ("aweme_detail", "aweme_list", "aweme_details", "item_list"):
        value = data.get(key)
        values.extend(value if isinstance(value, list) else [value])
    return next(
        (
            item
            for item in values
            if isinstance(item, dict) and _aweme_id(item) == aweme_id
        ),
        None,
    )


def extract_aweme_from_html(html: str, target_id: str) -> dict[str, Any] | None:
    if not html:
        return None

    # 1. window._ROUTER_DATA
    router_m = re.search(r"window\._ROUTER_DATA\s*=\s*(\{.*?\});\s*</script>", html, re.DOTALL)
    if router_m:
        try:
            data = json.loads(router_m.group(1))
            item = _browser_aweme(data, target_id)
            if item:
                return item
        except Exception:
            pass

    # 2. window.SSR_RENDER_DATA
    ssr_m = re.search(r"window\.SSR_RENDER_DATA\s*=\s*(\{.*?\});\s*</script>", html, re.DOTALL)
    if ssr_m:
        try:
            data = json.loads(ssr_m.group(1))
            item = _browser_aweme(data, target_id)
            if item:
                return item
        except Exception:
            pass

    # 3. self.__pace_f.push
    pushes = re.findall(r"self\.__pace_f\.push\((.*?)\)</script>", html, re.DOTALL)
    for p in pushes:
        try:
            val = json.loads(p)
            if isinstance(val, list) and len(val) >= 2 and isinstance(val[1], str) and target_id in val[1]:
                payload = val[1]
                colon = payload.find(":")
                if colon != -1:
                    raw_json = payload[colon + 1 :]
                    parsed = json.loads(raw_json)

                    def find_in_tree(node: Any) -> dict[str, Any] | None:
                        if isinstance(node, dict):
                            if str(node.get("awemeId") or node.get("aweme_id") or "") == target_id:
                                return node
                            for v in node.values():
                                res = find_in_tree(v)
                                if res:
                                    return res
                        elif isinstance(node, list):
                            for v in node:
                                res = find_in_tree(v)
                                if res:
                                    return res
                        return None

                    node = find_in_tree(parsed)
                    if node:
                        aweme = node.get("aweme") or {}
                        detail = aweme.get("detail") or node
                        author_info = (
                            detail.get("authorInfo")
                            or detail.get("author")
                            or node.get("accountInfo")
                            or {}
                        )
                        raw_images = detail.get("images") or detail.get("image_list") or []
                        images = []
                        for img in raw_images:
                            if not isinstance(img, dict):
                                continue
                            url_list = img.get("urlList") or img.get("url_list") or []
                            download_url_list = (
                                img.get("downloadUrlList") or img.get("download_url_list") or []
                            )
                            images.append({
                                "width": img.get("width"),
                                "height": img.get("height"),
                                "url_list": url_list,
                                "urlList": url_list,
                                "download_url_list": download_url_list,
                                "downloadUrlList": download_url_list,
                            })
                        sec_uid = author_info.get("secUid") or author_info.get("sec_uid") or ""
                        nickname = author_info.get("nickname") or ""
                        res_dict: dict[str, Any] = {
                            "aweme_id": target_id,
                            "awemeId": target_id,
                            "desc": detail.get("desc", ""),
                            "author": {
                                "nickname": nickname,
                                "sec_uid": sec_uid,
                                "secUid": sec_uid,
                            },
                            "authorInfo": {
                                "nickname": nickname,
                                "sec_uid": sec_uid,
                                "secUid": sec_uid,
                            },
                            "images": images,
                        }
                        if detail.get("video"):
                            res_dict["video"] = detail["video"]
                        return res_dict
        except Exception:
            continue
    return None


async def _fetch_and_extract_html_detail(
    client: Any,
    aweme_id: str,
    resolved_url: str,
) -> dict[str, Any] | None:
    import aiohttp

    session = getattr(client, "_session", None) or getattr(client, "session", None)
    headers = dict(getattr(client, "headers", {}))
    headers["Accept"] = (
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8"
    )
    targets = [f"https://www.douyin.com/note/{aweme_id}"]
    if resolved_url and resolved_url not in targets:
        targets.append(resolved_url)

    close_session = False
    if session is None:
        session = aiohttp.ClientSession()
        close_session = True

    try:
        for url in targets:
            try:
                timeout = aiohttp.ClientTimeout(total=10)
                async with session.get(url, headers=headers, timeout=timeout) as resp:
                    if resp.status == 200:
                        html = await resp.text()
                        item = extract_aweme_from_html(html, aweme_id)
                        if item:
                            return item
            except Exception as exc:
                print(f"douyin direct html get failed url={url}: {exc}", file=sys.stderr)
    finally:
        if close_session:
            await session.close()
    return None


async def _browser_detail(
    cdp_url: str,
    aweme_id: str,
    cookies: dict[str, Any],
    user_agent: str,
) -> dict[str, Any]:
    import aiohttp

    pending: dict[int, asyncio.Future[dict[str, Any]]] = {}
    events: asyncio.Queue[dict[str, Any]] = asyncio.Queue()
    next_id = 0

    async with aiohttp.ClientSession() as session:
        async with session.ws_connect(cdp_url, max_msg_size=32 * 1024 * 1024) as ws:
            async def read_messages() -> None:
                async for message in ws:
                    if message.type != aiohttp.WSMsgType.TEXT:
                        continue
                    try:
                        value = json.loads(message.data)
                    except Exception:
                        continue
                    if "id" in value:
                        future = pending.pop(value["id"], None)
                        if future and not future.done():
                            future.set_result(value)
                    elif "method" in value:
                        await events.put(value)

            reader = asyncio.create_task(read_messages())

            async def command(
                method: str,
                params: dict[str, Any] | None = None,
                session_id: str | None = None,
            ) -> dict[str, Any]:
                nonlocal next_id
                next_id += 1
                message: dict[str, Any] = {
                    "id": next_id,
                    "method": method,
                    "params": params or {},
                }
                if session_id:
                    message["sessionId"] = session_id
                future = asyncio.get_running_loop().create_future()
                pending[next_id] = future
                await ws.send_json(message)
                response = await asyncio.wait_for(future, 15)
                if "error" in response:
                    raise RuntimeError(
                        f"CDP {method} 失败: {response['error'].get('message', 'unknown')}"
                    )
                return response.get("result", {})

            browser_context_id = ""
            target_id = ""
            try:
                browser_context_id = (
                    await command("Target.createBrowserContext")
                )["browserContextId"]
                target_id = (
                    await command(
                        "Target.createTarget",
                        {"url": "about:blank", "browserContextId": browser_context_id},
                    )
                )["targetId"]
                target_session = (
                    await command(
                        "Target.attachToTarget",
                        {"targetId": target_id, "flatten": True},
                    )
                )["sessionId"]
                await command("Network.enable", session_id=target_session)
                await command("Page.enable", session_id=target_session)
                await command(
                    "Emulation.setUserAgentOverride",
                    {"userAgent": user_agent, "acceptLanguage": "zh-CN,zh;q=0.9"},
                    target_session,
                )
                await command(
                    "Network.setCookies",
                    {
                        "cookies": [
                            {
                                "name": name,
                                "value": value,
                                "domain": ".douyin.com",
                                "path": "/",
                            }
                            for name, value in cookies.items()
                            if isinstance(name, str)
                            and isinstance(value, str)
                            and name
                            and value
                        ]
                    },
                    target_session,
                )
                await command(
                    "Page.navigate",
                    {"url": f"https://www.douyin.com/note/{aweme_id}"},
                    target_session,
                )

                wanted_requests: set[str] = set()
                response_counts: dict[str, int] = {}
                deadline = time.monotonic() + 30
                reload_at = time.monotonic() + 15
                reloaded = False
                last_html_eval = 0.0

                while time.monotonic() < deadline:
                    now = time.monotonic()
                    if not reloaded and now >= reload_at:
                        await command("Page.reload", session_id=target_session)
                        reloaded = True
                    wait_until = deadline if reloaded else min(deadline, reload_at)

                    # Periodically check outerHTML for SSR render data
                    if now - last_html_eval >= 1.5:
                        last_html_eval = now
                        with contextlib.suppress(Exception):
                            eval_res = await command(
                                "Runtime.evaluate",
                                {"expression": "document.documentElement.outerHTML"},
                                target_session,
                            )
                            outer_html = eval_res.get("result", {}).get("value") or ""
                            item = extract_aweme_from_html(outer_html, aweme_id)
                            if item:
                                refreshed = await command(
                                    "Network.getAllCookies", session_id=target_session
                                )
                                try:
                                    _persist_browser_cookies(
                                        {
                                            cookie["name"]: cookie["value"]
                                            for cookie in refreshed.get("cookies", [])
                                            if cookie.get("name") and cookie.get("value")
                                        }
                                    )
                                except OSError as exc:
                                    print(
                                        f"douyin browser cookie persistence skipped: {exc}",
                                        file=sys.stderr,
                                    )
                                return item

                    try:
                        event = await asyncio.wait_for(
                            events.get(),
                            max(0.1, min(1.0, wait_until - time.monotonic())),
                        )
                    except TimeoutError:
                        continue
                    if event.get("sessionId") != target_session:
                        continue
                    method = event["method"]
                    params = event.get("params", {})
                    if method == "Network.responseReceived":
                        response = params.get("response", {})
                        parsed = urlsplit(str(response.get("url", "")))
                        if _is_douyin_domain(parsed.hostname) and parsed.path.startswith(
                            "/aweme/"
                        ):
                            response_counts[parsed.path] = (
                                response_counts.get(parsed.path, 0) + 1
                            )
                        if (
                            _is_douyin_domain(parsed.hostname)
                            and parsed.path
                            in {
                                "/aweme/v1/web/aweme/detail/",
                                "/aweme/v1/web/aweme/post/",
                            }
                            and int(response.get("status", 0)) == 200
                        ):
                            wanted_requests.add(str(params.get("requestId", "")))
                    elif (
                        method == "Network.loadingFinished"
                        and str(params.get("requestId", "")) in wanted_requests
                    ):
                        body = await command(
                            "Network.getResponseBody",
                            {"requestId": params["requestId"]},
                            target_session,
                        )
                        raw = body.get("body", "")
                        if body.get("base64Encoded"):
                            with contextlib.suppress(Exception):
                                raw = base64.b64decode(raw).decode("utf-8")
                        try:
                            parsed_json = json.loads(raw)
                        except Exception:
                            parsed_json = None
                        if parsed_json:
                            item = _browser_aweme(parsed_json, aweme_id)
                            if item:
                                refreshed = await command(
                                    "Network.getAllCookies", session_id=target_session
                                )
                                try:
                                    _persist_browser_cookies(
                                        {
                                            cookie["name"]: cookie["value"]
                                            for cookie in refreshed.get("cookies", [])
                                            if cookie.get("name") and cookie.get("value")
                                        }
                                    )
                                except OSError as exc:
                                    print(
                                        f"douyin browser cookie persistence skipped: {exc}",
                                        file=sys.stderr,
                                    )
                                return item

                blocked = await command(
                    "Runtime.evaluate",
                    {
                        "expression": "Boolean(document.querySelector('#captcha_container, .captcha-verify-container, .verify-bar-close, [class*=\"captcha_verify\"]') || (document.body && /请完成安全验证|拖动滑块完成拼图|点选文字|请在下方按顺序点击|访问太频繁/.test(document.body.innerText)))",
                        "returnByValue": True,
                    },
                    target_session,
                )
                if blocked.get("result", {}).get("value"):
                    raise RuntimeError("Oracle 抖音浏览器需要人工验证")
                paths = ",".join(sorted(response_counts)) or "none"
                raise RuntimeError(f"Oracle 抖音浏览器未返回目标作品 api={paths}")
            finally:
                if target_id:
                    with contextlib.suppress(Exception):
                        await command("Target.closeTarget", {"targetId": target_id})
                if browser_context_id:
                    with contextlib.suppress(Exception):
                        await command(
                            "Target.disposeBrowserContext",
                            {"browserContextId": browser_context_id},
                        )
                reader.cancel()
                with contextlib.suppress(asyncio.CancelledError):
                    await reader


async def _browser_feed(
    cdp_url: str,
    sec_user_id: str,
    cookies: dict[str, Any],
    user_agent: str,
    max_pages: int = 3,
) -> list[dict[str, Any]]:
    import aiohttp

    pending: dict[int, asyncio.Future[dict[str, Any]]] = {}
    events: asyncio.Queue[dict[str, Any]] = asyncio.Queue()
    next_id = 0

    async with aiohttp.ClientSession() as session:
        async with session.ws_connect(cdp_url, max_msg_size=32 * 1024 * 1024) as ws:
            async def read_messages() -> None:
                async for message in ws:
                    if message.type != aiohttp.WSMsgType.TEXT:
                        continue
                    try:
                        value = json.loads(message.data)
                    except Exception:
                        continue
                    if "id" in value:
                        future = pending.pop(value["id"], None)
                        if future and not future.done():
                            future.set_result(value)
                    elif "method" in value:
                        await events.put(value)

            reader = asyncio.create_task(read_messages())

            async def command(
                method: str,
                params: dict[str, Any] | None = None,
                session_id: str | None = None,
                timeout: float = 15,
            ) -> dict[str, Any]:
                nonlocal next_id
                next_id += 1
                message: dict[str, Any] = {
                    "id": next_id,
                    "method": method,
                    "params": params or {},
                }
                if session_id:
                    message["sessionId"] = session_id
                future = asyncio.get_running_loop().create_future()
                pending[next_id] = future
                await ws.send_json(message)
                response = await asyncio.wait_for(future, timeout)
                if "error" in response:
                    raise RuntimeError(
                        f"CDP {method} 失败: {response['error'].get('message', 'unknown')}"
                    )
                return response.get("result", {})

            browser_context_id = ""
            target_id = ""
            try:
                browser_context_id = (
                    await command("Target.createBrowserContext")
                )["browserContextId"]
                target_id = (
                    await command(
                        "Target.createTarget",
                        {"url": "about:blank", "browserContextId": browser_context_id},
                    )
                )["targetId"]
                target_session = (
                    await command(
                        "Target.attachToTarget",
                        {"targetId": target_id, "flatten": True},
                    )
                )["sessionId"]
                await command("Network.enable", session_id=target_session)
                await command("Page.enable", session_id=target_session)
                await command(
                    "Emulation.setUserAgentOverride",
                    {"userAgent": user_agent, "acceptLanguage": "zh-CN,zh;q=0.9"},
                    target_session,
                )
                await command(
                    "Network.setCookies",
                    {
                        "cookies": [
                            {
                                "name": name,
                                "value": value,
                                "domain": ".douyin.com",
                                "path": "/",
                            }
                            for name, value in cookies.items()
                            if isinstance(name, str)
                            and isinstance(value, str)
                            and name
                            and value
                        ]
                    },
                    target_session,
                )
                await command(
                    "Page.navigate",
                    {"url": f"https://www.douyin.com/user/{sec_user_id}"},
                    target_session,
                )

                wanted_requests: set[str] = set()
                completed_requests: set[str] = set()
                response_statuses: set[int] = set()
                empty_responses = 0
                body_failures = 0
                items: list[dict[str, Any]] = []
                seen_ids: set[str] = set()
                deadline = time.monotonic() + 30
                reload_at = time.monotonic() + 15
                reloaded = False
                last_scroll = time.monotonic()
                pages_done = 0

                while time.monotonic() < deadline:
                    if not items and not reloaded and time.monotonic() >= reload_at:
                        reloaded = True
                        with contextlib.suppress(RuntimeError, TimeoutError):
                            await command(
                                "Page.reload",
                                session_id=target_session,
                                timeout=max(0.01, min(15, deadline - time.monotonic())),
                            )
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        break
                    try:
                        event = await asyncio.wait_for(
                            events.get(),
                            min(0.5, remaining),
                        )
                    except TimeoutError:
                        if items and pages_done >= max_pages:
                            break
                        if items and time.monotonic() - last_scroll > 4:
                            if pages_done < max_pages:
                                try:
                                    await command(
                                        "Runtime.evaluate",
                                        {"expression": "window.scrollTo(0, document.body.scrollHeight)"},
                                        target_session,
                                        timeout=max(0.01, min(15, deadline - time.monotonic())),
                                    )
                                except (RuntimeError, TimeoutError):
                                    break
                                last_scroll = time.monotonic()
                        continue

                    if event.get("sessionId") != target_session:
                        continue
                    method = event["method"]
                    params = event.get("params", {})
                    if method == "Network.responseReceived":
                        response = params.get("response", {})
                        parsed = urlsplit(str(response.get("url", "")))
                        if (
                            _is_douyin_domain(parsed.hostname)
                            and "/aweme/v1/web/aweme/post/" in parsed.path
                        ):
                            status = int(response.get("status", 0))
                            response_statuses.add(status)
                            request_id = str(params.get("requestId", ""))
                            if status == 200 and request_id not in completed_requests:
                                wanted_requests.add(request_id)
                    elif (
                        method == "Network.loadingFinished"
                        and str(params.get("requestId", "")) in wanted_requests
                    ):
                        request_id = str(params["requestId"])
                        wanted_requests.discard(request_id)
                        completed_requests.add(request_id)
                        remaining = deadline - time.monotonic()
                        if remaining <= 0:
                            break
                        try:
                            body = await command(
                                "Network.getResponseBody",
                                {"requestId": request_id},
                                target_session,
                                timeout=min(15, remaining),
                            )
                        except (RuntimeError, TimeoutError):
                            body_failures += 1
                            continue
                        raw = body.get("body", "")
                        if body.get("base64Encoded"):
                            with contextlib.suppress(Exception):
                                raw = base64.b64decode(raw).decode("utf-8")
                        try:
                            data = json.loads(raw)
                        except Exception:
                            data = None
                        if isinstance(data, dict):
                            aweme_list = data.get("aweme_list")
                            if aweme_list is None:
                                aweme_list = data.get("cards")
                            if (
                                not isinstance(aweme_list, list)
                                or data.get("status_code", 0) != 0
                                or (aweme_list and not any(_aweme_id(it) for it in aweme_list))
                            ):
                                empty_responses += 1
                                continue
                            before_count = len(items)
                            _append_unique(items, seen_ids, aweme_list)
                            if items and data.get("has_more") in (False, 0):
                                break
                            if len(items) == before_count:
                                empty_responses += 1
                                continue
                            pages_done += 1
                            if pages_done >= max_pages:
                                break
                        else:
                            empty_responses += 1

                # Refresh cookies if any
                with contextlib.suppress(Exception):
                    refreshed = await command(
                        "Network.getAllCookies", session_id=target_session
                    )
                    _persist_browser_cookies(
                        {
                            cookie["name"]: cookie["value"]
                            for cookie in refreshed.get("cookies", [])
                            if cookie.get("name") and cookie.get("value")
                        }
                    )

                if not items:
                    statuses = ",".join(map(str, sorted(response_statuses))) or "none"
                    print(
                        f"douyin browser feed empty: statuses={statuses} "
                        f"empty_responses={empty_responses} body_failures={body_failures} "
                        f"reloads={int(reloaded)}",
                        file=sys.stderr,
                    )
                    blocked = await command(
                        "Runtime.evaluate",
                        {
                            "expression": "Boolean(document.querySelector('#captcha_container, .captcha-verify-container, .verify-bar-close, [class*=\"captcha_verify\"]') || (document.body && /请完成安全验证|拖动滑块完成拼图|点选文字|请在下方按顺序点击|访问太频繁/.test(document.body.innerText)))",
                            "returnByValue": True,
                        },
                        target_session,
                    )
                    if blocked.get("result", {}).get("value"):
                        raise RuntimeError("Oracle 抖音浏览器需要人工验证")

                return items
            finally:
                if target_id:
                    with contextlib.suppress(Exception):
                        await command("Target.closeTarget", {"targetId": target_id})
                if browser_context_id:
                    with contextlib.suppress(Exception):
                        await command(
                            "Target.disposeBrowserContext",
                            {"browserContextId": browser_context_id},
                        )
                reader.cancel()
                with contextlib.suppress(asyncio.CancelledError):
                    await reader


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
            cdp_url = os.environ.get("HANABI_DOUYIN_CDP_URL", "").strip()
            browser_used = False

            # 1. Fast-path: Direct HTTP GET note page and extract Pace SSR / window._ROUTER_DATA
            item = None
            try:
                item = await _fetch_and_extract_html_detail(
                    client, aweme_id, resolved_url
                )
            except Exception as exc:
                print(f"douyin direct html detail extract failed: {exc}", file=sys.stderr)

            # 2. Try upstream client.get_video_detail
            if not isinstance(item, dict) or _aweme_id(item) != aweme_id:
                try:
                    item = await client.get_video_detail(aweme_id)
                except Exception:
                    item = None

            # 3. Fallback to CDP browser
            if (
                (not isinstance(item, dict) or _aweme_id(item) != aweme_id)
                and cdp_url
            ):
                item = await _browser_detail(
                    cdp_url,
                    aweme_id,
                    cookies,
                    str(getattr(client, "headers", {}).get("User-Agent", "")),
                )
                browser_used = True

            if not isinstance(item, dict) or _aweme_id(item) != aweme_id:
                raise RuntimeError("作品详情接口返回空数据，可能是 Cookie/签名失效或触发验证")
            return {
                "resolved_url": resolved_url.split("?", 1)[0],
                "item": item,
                "browser_fallback_used": browser_used,
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
            try:
                page = await client.get_user_post(sec_user_id, max_cursor=cursor, count=20)
            except Exception as exc:
                print(f"douyin direct get_user_post error: {exc}", file=sys.stderr)
                restricted = True
                break
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

        cdp_url = os.environ.get("HANABI_DOUYIN_CDP_URL", "").strip()
        if (restricted or not all_items) and (browser_enabled or cdp_url):
            browser_used = True
            if cdp_url:
                try:
                    captured = await _browser_feed(
                        cdp_url,
                        sec_user_id,
                        cookies,
                        str(getattr(client, "headers", {}).get("User-Agent", "")),
                        max_pages=max_pages,
                    )
                    if captured:
                        _append_unique(all_items, seen, captured)
                        restricted = False
                except Exception as exc:
                    print(f"douyin cdp browser feed failed: {type(exc).__name__}: {exc}", file=sys.stderr)
            elif browser_enabled:
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
                        try:
                            page = await client.get_user_post(
                                sec_user_id, max_cursor=cursor, count=20
                            )
                        except Exception:
                            break
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
            suffix = "；浏览器兜底没有取得作品" if browser_used else ""
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
