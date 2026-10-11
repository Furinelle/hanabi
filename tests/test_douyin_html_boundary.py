"""Real aiohttp cookies and local TLS; no upstream imports or public requests."""
import contextlib
import io
import json
import socket
import ssl
import subprocess
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, patch

import aiohttp
from aiohttp import web

from tests.test_douyin_user_feed import MODULE


class LocalResolver(aiohttp.abc.AbstractResolver):
    def __init__(self, tls_port, http_port):
        self.ports = {('www.douyin.com', 443): tls_port,
                      ('recipient.example.test', 80): http_port,
                      ('www.douyin.com', 80): http_port}

    async def resolve(self, host, port=0, family=socket.AF_INET):
        # Unknown destinations fail before DNS or any public connection.
        local_port = self.ports[(host, port)]
        return [{'hostname': host, 'host': '127.0.0.1', 'port': local_port,
                 'family': socket.AF_INET, 'proto': 0, 'flags': 0}]

    async def close(self):
        pass


class HtmlCookieBoundaryTest(unittest.IsolatedAsyncioTestCase):
    @classmethod
    def setUpClass(cls):
        cls.cert_dir = tempfile.TemporaryDirectory(prefix='hanabi-html-tls-')
        cls.addClassCleanup(cls.cert_dir.cleanup)
        root = Path(cls.cert_dir.name)
        cls.cert, cls.key = root / 'cert.pem', root / 'key.pem'
        subprocess.run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes',
                        '-keyout', str(cls.key), '-out', str(cls.cert), '-days', '1',
                        '-subj', '/CN=www.douyin.com',
                        '-addext', 'subjectAltName=DNS:www.douyin.com'], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    async def asyncSetUp(self):
        self.canary = 'SYNTHETIC_COOKIE_NOT_FOR_LOGS'
        self.cookies = {'sessionid': self.canary}
        self.mode = 'success'
        self.trusted_requests, self.recipient_requests = [], []
        self.item = {'aweme_id': '123', 'author': {'sec_uid': 'MS4wSYNTHETIC'},
                     'images': [{'url_list': ['https://images.example.test/123.png']}]}
        self.html = '<script>window._ROUTER_DATA = ' + json.dumps({'aweme_detail': self.item}) + ';</script>'

        async def trusted(request):
            self.trusted_requests.append(request.cookies.get('sessionid'))
            if self.mode == 'cross-host':
                raise web.HTTPFound('http://recipient.example.test/landing')
            if self.mode == 'downgrade':
                raise web.HTTPFound('http://www.douyin.com/landing')
            if self.mode == 'missing':
                return web.Response(status=404)
            return web.Response(text=self.html, content_type='text/html')

        async def recipient(request):
            self.recipient_requests.append(request.cookies.get('sessionid'))
            return web.Response(text=self.html, content_type='text/html')

        server_ssl = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        server_ssl.load_cert_chain(self.cert, self.key)
        runners = []
        for handler, tls in ((trusted, server_ssl), (recipient, None)):
            app = web.Application()
            app.router.add_get('/{path:.*}', handler)
            runner = web.AppRunner(app)
            await runner.setup()
            self.addAsyncCleanup(runner.cleanup)
            site = web.TCPSite(runner, '127.0.0.1', 0, ssl_context=tls)
            await site.start()
            runners.append(site._server.sockets[0].getsockname()[1])
        client_ssl = ssl.create_default_context(cafile=str(self.cert))
        connector = aiohttp.TCPConnector(resolver=LocalResolver(*runners), ssl=client_ssl)
        # Match upstream's domainless shared-cookie session, without importing it.
        self.session = aiohttp.ClientSession(cookies=self.cookies, connector=connector, trust_env=False)
        self.addAsyncCleanup(self.session.close)
        self.client = types.SimpleNamespace(_session=self.session, headers={'User-Agent': 'LocalTlsTest'})

    async def fetch(self, resolved_url='https://www.douyin.com/note/123'):
        return await MODULE._fetch_and_extract_html_detail(self.client, '123', resolved_url)

    async def test_trusted_https_keeps_native_cookie_and_session(self):
        self.assertEqual(await self.fetch(), self.item)
        self.assertEqual(self.trusted_requests, [self.canary])
        self.assertEqual(self.recipient_requests, [])
        self.assertFalse(self.session.closed)

    async def test_cross_host_redirect_never_sends_native_cookie(self):
        self.mode = 'cross-host'
        self.assertIsNone(await self.fetch())
        self.assertEqual(self.trusted_requests, [self.canary])
        self.assertEqual(self.recipient_requests, [])

    async def test_https_downgrade_never_sends_native_cookie(self):
        self.mode = 'downgrade'
        self.assertIsNone(await self.fetch())
        self.assertEqual(self.trusted_requests, [self.canary])
        self.assertEqual(self.recipient_requests, [])

    async def test_untrusted_resolved_targets_are_not_requested(self):
        self.mode = 'missing'
        for url in ('https://recipient.example.test/note/123',
                    'http://www.douyin.com/note/123',
                    'https://www.douyin.com:444/note/123',
                    'https://www.douyin.com:invalid/note/123',
                    f'https://{self.canary}@www.douyin.com/note/123',
                    'https://www.douyin.com.attacker.test/note/123'):
            with self.subTest(url=url):
                self.assertIsNone(await self.fetch(url))
        self.assertEqual(self.trusted_requests, [self.canary] * 6)
        self.assertEqual(self.recipient_requests, [])

    async def test_redirect_continues_to_signed_api_and_short_resolver_is_anonymous(self):
        self.mode = 'cross-host'
        client = types.SimpleNamespace(_session=self.session, headers=self.client.headers,
                                      get_video_detail=AsyncMock(return_value=self.item))
        resolver = types.SimpleNamespace(resolve_short_url=AsyncMock(return_value='https://www.douyin.com/note/123'))
        cookie_arguments = []

        class Context:
            def __init__(self, cookies):
                cookie_arguments.append(cookies)
                self.value = client if cookies else resolver

            async def __aenter__(self):
                return self.value

            async def __aexit__(self, *_):
                pass

        upstream = (Context, lambda _: self.cookies, lambda _: True, lambda value: value)
        with patch.object(MODULE, '_load_upstream', return_value=upstream), patch.object(
            MODULE, '_cookie_header', return_value=''
        ), patch.object(MODULE, 'os', types.SimpleNamespace(environ={})):
            result = await MODULE._run({'target': 'https://v.douyin.com/fake/', 'operation': 'detail'})
        self.assertEqual(result['item'], self.item)
        client.get_video_detail.assert_awaited_once_with('123')
        resolver.resolve_short_url.assert_awaited_once_with('https://v.douyin.com/fake/')
        self.assertEqual(cookie_arguments, [self.cookies, {}])
        self.assertEqual(self.recipient_requests, [])
        self.assertFalse(self.session.closed)

    async def test_untrusted_response_and_exception_diagnostics_are_safe(self):
        response = AsyncMock()
        response.__aenter__.return_value = response
        response.status = 200
        response.url = 'https://recipient.example.test/note/123'
        response.text = AsyncMock(return_value=self.html)
        unsafe = types.SimpleNamespace(_session=types.SimpleNamespace(get=lambda *a, **k: response), headers={})
        self.assertIsNone(await MODULE._fetch_and_extract_html_detail(unsafe, '123', ''))
        response.text.assert_not_awaited()

        def fail(*args, **kwargs):
            raise RuntimeError(f'Cookie: sessionid={self.canary}')

        broken = types.SimpleNamespace(_session=types.SimpleNamespace(get=fail), headers={})
        output = io.StringIO()
        with contextlib.redirect_stderr(output):
            self.assertIsNone(await MODULE._fetch_and_extract_html_detail(broken, '123', ''))
        self.assertIn('RuntimeError', output.getvalue())
        self.assertNotIn(self.canary, output.getvalue())


class TrustedHtmlUrlTest(unittest.TestCase):
    def test_existing_domains_require_https_default_port_and_no_userinfo(self):
        for url in ('https://www.douyin.com/note/123', 'https://douyin.com:443/note/123',
                    'https://www.iesdouyin.com/share/slides/123/'):
            self.assertTrue(MODULE._is_trusted_html_url(url), url)
        for url in ('http://douyin.com/', 'https://douyin.com:80/', 'https://user:pass@douyin.com/',
                    'https://@douyin.com/', 'https://douyin.com.evil.test/', 'https://[invalid/',
                    'file:///douyin.com'):
            self.assertFalse(MODULE._is_trusted_html_url(url), url)


if __name__ == '__main__':
    unittest.main()
