import asyncio
import importlib.util
import itertools
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


class BrowserFeedTest(unittest.IsolatedAsyncioTestCase):
    async def test_reloads_an_empty_first_page_once(self):
        item = {"aweme_id": "2", "images": [{}]}
        commands = []

        class Socket:
            def __init__(self):
                self.messages = asyncio.Queue()

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

            async def send_json(self, message):
                method = message["method"]
                commands.append(method)
                result = {
                    "Target.createBrowserContext": {"browserContextId": "context"},
                    "Target.createTarget": {"targetId": "target"},
                    "Target.attachToTarget": {"sessionId": "session"},
                    "Network.getResponseBody": {"body": json.dumps({"aweme_list": [item]})},
                }.get(method, {})
                await self.messages.put({"id": message["id"], "result": result})
                if method == "Page.reload":
                    for event, params in (
                        ("Network.responseReceived", {"requestId": "post", "response": {
                            "url": "https://www.douyin.com/aweme/v1/web/aweme/post/", "status": 200}}),
                        ("Network.loadingFinished", {"requestId": "post"}),
                    ):
                        await self.messages.put({"sessionId": "session", "method": event, "params": params})

        socket = Socket()
        aiohttp = types.SimpleNamespace(ClientSession=lambda: socket, WSMsgType=types.SimpleNamespace(TEXT=1))
        ticks = itertools.chain([0] * 5, [16] * 4, itertools.repeat(31))
        with patch.dict(sys.modules, aiohttp=aiohttp), patch.object(
            MODULE, "time", types.SimpleNamespace(monotonic=lambda: next(ticks))
        ), patch.object(MODULE, "_persist_browser_cookies"):
            items = await MODULE._browser_feed("ws://browser", "author", {}, "UA", max_pages=1)
        self.assertEqual(items, [item])
        self.assertEqual(commands.count("Page.reload"), 1)
        self.assertIn("Target.disposeBrowserContext", commands)


if __name__ == "__main__":
    unittest.main()
