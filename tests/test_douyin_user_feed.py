import asyncio
import importlib.util
import json
import sys
import types
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, call, patch


SPEC = importlib.util.spec_from_file_location(
    "douyin_user_feed", Path(__file__).parents[1] / "tools/douyin_user_feed.py"
)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class BrowserAwemeTest(unittest.TestCase):
    def test_selects_only_the_requested_aweme(self):
        wanted = {"aweme_id": "2", "images": [{}]}
        self.assertIs(
            MODULE._browser_aweme(
                {"aweme_list": [{"aweme_id": "1"}, wanted]}, "2"
            ),
            wanted,
        )
        self.assertIsNone(MODULE._browser_aweme({"aweme_detail": wanted}, "1"))


class FakeClock:
    def __init__(self):
        self.now = 0.0
        self.scheduled = []

    def monotonic(self):
        return self.now

    def advance(self, seconds):
        self.now += seconds
        ready = [entry for entry in self.scheduled if entry[0] <= self.now]
        self.scheduled = [entry for entry in self.scheduled if entry[0] > self.now]
        for _, callback in ready:
            callback()


class FakeCDP:
    def __init__(self, navigate, reload=None, scroll=None, failures=None, challenge=False):
        self.clock = FakeClock()
        self.messages = asyncio.Queue()
        self.commands = []
        self.bodies = {}
        self.body_reads = []
        self.timeout_pending = False
        self.failures = failures or {}
        self.challenge = challenge
        self.actions = {
            "Page.navigate": navigate,
            "Page.reload": reload,
            "scroll": scroll,
        }

    async def __aenter__(self):
        return self

    async def __aexit__(self, *_):
        pass

    def ws_connect(self, *_args, **_kwargs):
        return self

    def __aiter__(self):
        return self

    async def __anext__(self):
        return types.SimpleNamespace(type=1, data=json.dumps(await self.messages.get()))

    def event(self, method, params, session="session"):
        self.messages.put_nowait({"sessionId": session, "method": method, "params": params})

    def post(self, request_id, items, has_more=1, session="session", failure=False, status_code=0):
        self.bodies[request_id] = failure or {
            "aweme_list": items, "has_more": has_more, "status_code": status_code,
        }
        self.event("Network.responseReceived", {
            "requestId": request_id,
            "response": {"url": "https://www.douyin.com/aweme/v1/web/aweme/post/", "status": 200},
        }, session)
        self.event("Network.loadingFinished", {"requestId": request_id}, session)

    async def send_json(self, message):
        method = message["method"]
        self.commands.append((self.clock.now, method, message["params"]))
        result = {
            "Target.createBrowserContext": {"browserContextId": "context"},
            "Target.createTarget": {"targetId": "target"},
            "Target.attachToTarget": {"sessionId": "session"},
            "Runtime.evaluate": {"result": {"value": self.challenge}},
            "Network.getAllCookies": {"cookies": [{"name": "ttwid", "value": "fake_ttwid"}]},
        }.get(method, {})
        response = {"id": message["id"], "result": result}
        if method == "Network.getResponseBody":
            request_id = message["params"]["requestId"]
            self.body_reads.append(request_id)
            body = self.bodies[request_id]
            if body == "timeout":
                self.timeout_pending = True
                return
            if body == "error":
                response = {"id": message["id"], "error": {"message": "fake body unavailable"}}
            else:
                response["result"] = {"body": json.dumps(body)}
        action_key = method
        if method == "Runtime.evaluate" and "scrollTo" in message["params"].get("expression", ""):
            action_key = "scroll"
        failure = self.failures.get(action_key)
        if failure == "timeout":
            self.timeout_pending = True
            return
        if failure == "error":
            response = {"id": message["id"], "error": {"message": "fake command unavailable"}}
        self.messages.put_nowait(response)
        action = self.actions.get(action_key)
        if action:
            action(self)

    async def wait_for(self, awaitable, timeout):
        if asyncio.isfuture(awaitable):
            if self.timeout_pending:
                self.timeout_pending = False
                awaitable.cancel()
                self.clock.advance(timeout)
                raise TimeoutError
            return await asyncio.wait_for(awaitable, 1)
        try:
            return await asyncio.wait_for(awaitable, 0.001)
        except TimeoutError:
            self.clock.advance(timeout)
            raise


class BrowserFeedTest(unittest.IsolatedAsyncioTestCase):
    async def run_feed(self, socket, max_pages):
        aiohttp = types.SimpleNamespace(ClientSession=lambda: socket, WSMsgType=types.SimpleNamespace(TEXT=1))
        async_api = types.SimpleNamespace(
            Queue=asyncio.Queue, create_task=asyncio.create_task,
            get_running_loop=asyncio.get_running_loop, CancelledError=asyncio.CancelledError,
            wait_for=socket.wait_for,
        )
        with patch.dict(sys.modules, aiohttp=aiohttp), patch.object(
            MODULE, "time", socket.clock
        ), patch.object(MODULE, "asyncio", async_api), patch.object(MODULE, "_persist_browser_cookies") as persist:
            items = await MODULE._browser_feed("ws://fake", "fake_author", {}, "fake_UA", max_pages=max_pages)
            if items:
                persist.assert_called_once_with({"ttwid": "fake_ttwid"})
            else:
                persist.assert_not_called()
            return items

    def assert_cleaned(self, socket):
        methods = [method for _, method, _ in socket.commands]
        self.assertEqual(methods.count("Target.closeTarget"), 1)
        self.assertEqual(methods.count("Target.disposeBrowserContext"), 1)

    async def test_reloads_an_actual_empty_response_once(self):
        item = {"aweme_id": "2", "images": [{}]}
        socket = FakeCDP(
            navigate=lambda s: s.post("empty", []),
            reload=lambda s: s.post("reloaded", [item], has_more=0),
        )
        self.assertEqual(await self.run_feed(socket, max_pages=1), [item])
        self.assertEqual(socket.body_reads, ["empty", "reloaded"])
        self.assertEqual([(t, m) for t, m, _ in socket.commands if m == "Page.reload"], [(15.0, "Page.reload")])
        self.assert_cleaned(socket)

    async def test_scroll_waits_for_the_delayed_second_page(self):
        first, second = {"aweme_id": "1"}, {"aweme_id": "2"}
        for max_pages in (2, 5):
            with self.subTest(max_pages=max_pages):
                socket = FakeCDP(
                    navigate=lambda s: s.post("first", [first]),
                    scroll=lambda s: s.clock.scheduled.append((
                        s.clock.now + 1, lambda: s.post("second", [second], has_more=0)
                    )),
                )
                self.assertEqual(await self.run_feed(socket, max_pages), [first, second])
                self.assertEqual(socket.body_reads, ["first", "second"])
                self.assertTrue(any("scrollTo" in p.get("expression", "") for _, _, p in socket.commands))
                self.assertFalse(any(m == "Page.reload" for _, m, _ in socket.commands))
                self.assertLess(socket.clock.now, 30)
                self.assert_cleaned(socket)

    async def test_body_failures_duplicates_and_other_sessions_allow_later_pages(self):
        first, second = {"aweme_id": "1"}, {"aweme_id": "2"}
        for failure in ("error", "timeout"):
            with self.subTest(failure=failure):
                def navigate(socket):
                    socket.post("broken", [], failure=failure)
                    socket.post("foreign", [{"aweme_id": "99"}], session="unrelated")
                    socket.post("invalid", [{"aweme_id": "invalid"}])
                    socket.post("first", [first])
                    socket.event("Network.loadingFinished", {"requestId": "first"})
                    socket.post("first", [first])
                    socket.post("same-items", [first])
                    socket.post("second", [second], has_more=0)

                socket = FakeCDP(navigate)
                self.assertEqual(await self.run_feed(socket, max_pages=2), [first, second])
                self.assertEqual(socket.body_reads, ["broken", "invalid", "first", "same-items", "second"])
                self.assert_cleaned(socket)

    async def test_always_empty_reloads_once_and_cleans_up_at_deadline(self):
        socket = FakeCDP(
            navigate=lambda s: s.post("empty", []),
            reload=lambda s: s.post("still-empty", []),
        )
        self.assertEqual(await self.run_feed(socket, max_pages=1), [])
        self.assertEqual(socket.body_reads, ["empty", "still-empty"])
        self.assertEqual([t for t, m, _ in socket.commands if m == "Page.reload"], [15.0])
        self.assertEqual(socket.clock.now, 30.0)
        self.assert_cleaned(socket)

    async def test_late_scroll_failures_preserve_items_and_reload_timeout_is_bounded(self):
        first = {"aweme_id": "1"}

        def late_navigation(socket, items):
            socket.post("first", items)
            # Advance after the feed deadline has been established, before a scroll/reload.
            socket.clock.scheduled.append((1, lambda: socket.clock.advance(26)))

        for failure in ("error", "timeout"):
            with self.subTest(command="scroll", failure=failure):
                socket = FakeCDP(
                    navigate=lambda s: late_navigation(s, [first]),
                    failures={"scroll": failure},
                )
                self.assertEqual(await self.run_feed(socket, max_pages=2), [first])
                self.assertEqual([t for t, m, p in socket.commands if "scrollTo" in p.get("expression", "")], [27.0])
                self.assertLessEqual(socket.clock.now, 30)
                self.assert_cleaned(socket)

        with self.subTest(command="reload", failure="timeout"):
            socket = FakeCDP(
                navigate=lambda s: late_navigation(s, []),
                failures={"Page.reload": "timeout"},
            )
            self.assertEqual(await self.run_feed(socket, max_pages=1), [])
            self.assertEqual([t for t, m, _ in socket.commands if m == "Page.reload"], [27.0])
            self.assertLessEqual(socket.clock.now, 30)
            self.assert_cleaned(socket)

    async def test_malformed_terminal_responses_do_not_hide_the_next_valid_page(self):
        first, second = {"aweme_id": "1"}, {"aweme_id": "2"}

        def navigate(socket):
            socket.post("first", [first])
            socket.post("string-list", "invalid", has_more=0)
            socket.post("failed-status", [{"aweme_id": "99"}], has_more=0, status_code=1)
            socket.post("invalid-ids", [{"aweme_id": "invalid"}], has_more=0)
            socket.post("second", [second], has_more=0)

        socket = FakeCDP(navigate)
        self.assertEqual(await self.run_feed(socket, max_pages=2), [first, second])
        self.assertEqual(socket.body_reads, ["first", "string-list", "failed-status", "invalid-ids", "second"])
        self.assert_cleaned(socket)

    async def test_challenge_raises_and_cleans_up(self):
        socket = FakeCDP(navigate=lambda s: s.post("empty", []), challenge=True)
        with self.assertRaisesRegex(RuntimeError, "需要人工验证"):
            await self.run_feed(socket, max_pages=1)
        self.assertEqual([t for t, m, _ in socket.commands if m == "Page.reload"], [15.0])
        self.assertLessEqual(socket.clock.now, 30)
        self.assert_cleaned(socket)


class BridgeDetailTest(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.skeleton = {
            "aweme_id": "2", "awemeId": "2", "desc": "",
            "author": {"sec_uid": "", "secUid": ""},
            "authorInfo": {"sec_uid": "", "secUid": ""}, "images": [],
        }
        self.client = MagicMock()
        self.client.__aenter__.return_value = self.client
        self.client.__aexit__.return_value = None
        self.client.headers = {"User-Agent": "fake_UA"}
        self.client.get_video_detail = AsyncMock(return_value=self.skeleton)
        upstream = (lambda _: self.client, lambda _: {}, lambda _: False, lambda url: url)
        self.enterContext(patch.object(MODULE, "_load_upstream", return_value=upstream))
        self.enterContext(patch.object(MODULE, "_cookie_header", return_value=""))
        self.environment = {}
        self.enterContext(patch.object(MODULE, "os", types.SimpleNamespace(environ=self.environment)))
        self.html = self.enterContext(patch.object(
            MODULE, "_fetch_and_extract_html_detail", new_callable=AsyncMock, return_value=self.skeleton,
        ))
        self.browser = self.enterContext(patch.object(
            MODULE, "_browser_detail", new_callable=AsyncMock, return_value=self.skeleton,
        ))

    async def detail(self):
        return await MODULE._run({"target": "https://www.douyin.com/note/2", "operation": "detail"})

    async def test_skeleton_html_continues_to_api(self):
        valid = {"aweme_id": "2", "images": [{"url_list": ["https://images.example/2.jpg"]}]}
        self.client.get_video_detail.return_value = valid
        result = await self.detail()
        self.assertIs(result["item"], valid)
        self.assertFalse(result["browser_fallback_used"])
        self.client.get_video_detail.assert_awaited_once_with("2")
        self.browser.assert_not_awaited()

    async def test_skeleton_html_and_api_continue_to_browser(self):
        self.environment["HANABI_DOUYIN_CDP_URL"] = "ws://fake"
        valid = {"aweme_id": "2", "author": {"sec_uid": "MS4wVIDEO"}, "video": {"cover": {}}}
        self.browser.return_value = valid
        result = await self.detail()
        self.assertIs(result["item"], valid)
        self.assertTrue(result["browser_fallback_used"])
        self.client.get_video_detail.assert_awaited_once_with("2")
        self.browser.assert_awaited_once_with("ws://fake", "2", {}, "fake_UA")

    async def test_real_video_author_identity_returns_without_gallery_images(self):
        for author_key, id_key in (("author", "sec_uid"), ("authorInfo", "secUid")):
            with self.subTest(author_key=author_key):
                valid = {"aweme_id": "2", author_key: {id_key: "MS4wVIDEO"}, "video": {"cover": {}}}
                self.html.return_value = valid
                result = await self.detail()
                self.assertIs(result["item"], valid)
                self.client.get_video_detail.assert_not_awaited()
                self.browser.assert_not_awaited()

    async def test_real_gallery_images_keep_legacy_details_without_author(self):
        images = [{"origin_image": {"url_list": ["https://images.example/2.jpg"]}}]
        for gallery in ({"images": images}, {"image_list": images},
                        {"image_post_info": {"images": images}}):
            with self.subTest(gallery=gallery):
                valid = {"aweme_id": "2", **gallery}
                self.html.return_value = valid
                self.assertIs((await self.detail())["item"], valid)
                self.client.get_video_detail.assert_not_awaited()
                self.browser.assert_not_awaited()

    async def test_terminal_skeleton_is_not_success_with_or_without_browser(self):
        for cdp_url in ("", "ws://fake"):
            with self.subTest(cdp_url=cdp_url):
                self.environment["HANABI_DOUYIN_CDP_URL"] = cdp_url
                with self.assertRaisesRegex(RuntimeError, "作品详情接口返回空数据"):
                    await self.detail()
        self.assertEqual(self.client.get_video_detail.await_count, 2)
        self.browser.assert_awaited_once()
        for invalid in (None, {"aweme_id": "1", "author": {"sec_uid": "MS4wVIDEO"}},
                        {"aweme_id": "2", "images": [{}]},
                        {"aweme_id": "2", "author": {"sec_uid": "invalid/id"}}):
            self.assertFalse(MODULE._usable_detail(invalid, "2"))


class BrowserDetailTest(unittest.IsolatedAsyncioTestCase):
    async def test_placeholder_html_and_network_payload_do_not_hide_valid_video(self):
        skeleton = {"aweme_id": "2", "author": {"sec_uid": ""}, "images": []}
        valid = {"aweme_id": "2", "author": {"sec_uid": "MS4wVIDEO"}, "video": {"cover": {}}}

        def navigate(socket):
            socket.post("skeleton", [skeleton])
            socket.post("valid", [valid])

        socket = FakeCDP(navigate)
        socket.clock.now = 2  # Make the first outerHTML poll precede network inspection.
        aiohttp = types.SimpleNamespace(ClientSession=lambda: socket, WSMsgType=types.SimpleNamespace(TEXT=1))
        async_api = types.SimpleNamespace(
            Queue=asyncio.Queue, create_task=asyncio.create_task,
            get_running_loop=asyncio.get_running_loop, CancelledError=asyncio.CancelledError,
            wait_for=socket.wait_for,
        )
        with patch.dict(sys.modules, aiohttp=aiohttp), patch.object(
            MODULE, "time", socket.clock
        ), patch.object(MODULE, "asyncio", async_api), patch.object(
            MODULE, "extract_aweme_from_html", return_value=skeleton
        ) as html, patch.object(MODULE, "_persist_browser_cookies") as persist:
            result = await MODULE._browser_detail("ws://fake", "2", {}, "fake_UA")
        self.assertIsNotNone(html.call_args)
        self.assertEqual(result, valid)
        self.assertEqual(socket.body_reads, ["skeleton", "valid"])
        persist.assert_called_once_with({"ttwid": "fake_ttwid"})
        methods = [method for _, method, _ in socket.commands]
        self.assertEqual(methods.count("Target.closeTarget"), 1)
        self.assertEqual(methods.count("Target.disposeBrowserContext"), 1)


class BridgeFeedTest(unittest.IsolatedAsyncioTestCase):
    async def test_short_link_resolution_is_anonymous_and_cdp_keeps_author_cookies(self):
        cookies = {"fake_session": "fake_value"}
        item = {"aweme_id": "1"}
        short_url = "https://v.douyin.com/fake/"
        author_url = "https://www.douyin.com/user/fake_author"
        main, resolver = MagicMock(), MagicMock()
        for client in (main, resolver):
            client.__aenter__.return_value = client
            client.__aexit__.return_value = None
        main.headers = {"User-Agent": "fake_UA"}
        main.resolve_short_url = AsyncMock(side_effect=AssertionError("authenticated client must not resolve"))
        resolver.resolve_short_url = AsyncMock(return_value=author_url)
        factory = MagicMock(side_effect=[main, resolver])
        upstream = (factory, lambda _: cookies, lambda url: url == short_url, lambda url: url)
        with patch.object(MODULE, "_load_upstream", return_value=upstream), patch.object(
            MODULE, "_cookie_header", return_value="fake_header"
        ), patch.object(
            MODULE, "os", types.SimpleNamespace(environ={"HANABI_DOUYIN_CDP_URL": "ws://fake"})
        ), patch.object(MODULE, "_browser_feed", new_callable=AsyncMock, return_value=[item]) as browser:
            result = await MODULE._run({"target": short_url})
        self.assertEqual(result["items"], [item])
        self.assertEqual(factory.call_args_list, [call(cookies), call({})])
        main.resolve_short_url.assert_not_awaited()
        resolver.resolve_short_url.assert_awaited_once_with(short_url)
        browser.assert_awaited_once_with("ws://fake", "fake_author", cookies, "fake_UA", max_pages=3)
        self.assertIs(browser.await_args.args[2], cookies)
        for client in (main, resolver):
            client.__aexit__.assert_awaited_once()

    async def test_configured_cdp_skips_http_filters_known_ids_and_reports_exhaustion(self):
        first, second = {"aweme_id": "1"}, {"aweme_id": "2"}
        http_calls = []

        class Client:
            headers = {"User-Agent": "fake_UA"}

            def __init__(self, cookies):
                pass

            async def __aenter__(self):
                return self

            async def __aexit__(self, *_):
                pass

            async def get_user_info(self, _):
                http_calls.append("profile")
                raise AssertionError("configured CDP must skip profile HTTP")

            async def get_user_post(self, *_args, **_kwargs):
                http_calls.append("post")
                raise AssertionError("configured CDP must skip post HTTP")

        upstream = (Client, lambda _: {}, lambda _: False, lambda value: value)
        for responses in (([first, second],), ([], [first, second]), ([], []), (RuntimeError("Oracle 抖音浏览器需要人工验证"),)):
            succeeds = isinstance(responses[-1], list) and bool(responses[-1])
            with self.subTest(responses=responses), patch.object(
                MODULE, "_load_upstream", return_value=upstream
            ), patch.object(MODULE, "_cookie_header", return_value=""), patch.object(
                MODULE, "os", types.SimpleNamespace(environ={"HANABI_DOUYIN_CDP_URL": "ws://fake"})
            ), patch.object(MODULE, "_browser_feed", new_callable=AsyncMock, side_effect=responses) as browser:
                request = {"target": "https://www.douyin.com/user/fake_author", "known_ids": ["1"]}
                if succeeds:
                    result = await MODULE._run(request)
                    self.assertEqual(result["items"], [second])
                    self.assertTrue(result["browser_fallback_used"])
                    self.assertFalse(result["restricted"])
                else:
                    with self.assertRaisesRegex(RuntimeError, "浏览器兜底没有取得作品"):
                        await MODULE._run(request)
                self.assertEqual(browser.await_count, len(responses))
                browser.assert_has_awaits([call("ws://fake", "fake_author", {}, "fake_UA", max_pages=3)] * len(responses))
                self.assertEqual(http_calls, [])

    async def test_without_cdp_api_paginates_and_403_is_not_success(self):
        first, second = {"aweme_id": "1"}, {"aweme_id": "2"}
        cursors = []

        class Client:
            def __init__(self, cookies):
                pass

            async def __aenter__(self):
                return self

            async def __aexit__(self, *_):
                pass

            async def get_user_info(self, _):
                return {"aweme_count": 2}

            async def get_user_post(self, _, max_cursor, count):
                cursors.append(max_cursor)
                if blocked:
                    raise RuntimeError("HTTP 403")
                return {
                    "items": [first if max_cursor == 0 else second],
                    "has_more": max_cursor == 0, "max_cursor": 1,
                }

        upstream = (Client, lambda _: {}, lambda _: False, lambda value: value)
        for blocked in (False, True):
            cursors.clear()
            with self.subTest(blocked=blocked), patch.object(
                MODULE, "_load_upstream", return_value=upstream
            ), patch.object(MODULE, "_cookie_header", return_value=""), patch.object(
                MODULE, "os", types.SimpleNamespace(environ={})
            ), patch.object(MODULE, "_browser_feed", new_callable=AsyncMock) as browser:
                request = {"target": "https://www.douyin.com/user/fake_author"}
                if blocked:
                    with self.assertRaisesRegex(RuntimeError, "作者主页未返回作品"):
                        await MODULE._run(request)
                    self.assertEqual(cursors, [0])
                else:
                    result = await MODULE._run(request)
                    self.assertEqual(result["items"], [first, second])
                    self.assertEqual(result["pages_fetched"], 2)
                    self.assertFalse(result["browser_fallback_used"])
                    self.assertFalse(result["restricted"])
                    self.assertEqual(cursors, [0, 1])
                browser.assert_not_awaited()


if __name__ == "__main__":
    unittest.main()
