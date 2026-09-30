import asyncio
import importlib.util
import json
import sys
import types
import unittest
from pathlib import Path
from unittest.mock import patch


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
    def __init__(self, navigate, reload=None, scroll=None):
        self.clock = FakeClock()
        self.messages = asyncio.Queue()
        self.commands = []
        self.bodies = {}
        self.body_reads = []
        self.timeout_pending = False
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

    def post(self, request_id, items, has_more=1, session="session", failure=False):
        self.bodies[request_id] = failure or {"aweme_list": items, "has_more": has_more}
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
            "Runtime.evaluate": {"result": {"value": False}},
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
        self.messages.put_nowait(response)
        action = self.actions.get(method)
        if method == "Runtime.evaluate" and "scrollTo" in message["params"].get("expression", ""):
            action = self.actions["scroll"]
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
        if awaitable.cr_code.co_name == "command":
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
        ), patch.object(MODULE, "asyncio", async_api), patch.object(MODULE, "_persist_browser_cookies"):
            return await MODULE._browser_feed("ws://fake", "fake_author", {}, "fake_UA", max_pages=max_pages)

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


if __name__ == "__main__":
    unittest.main()
