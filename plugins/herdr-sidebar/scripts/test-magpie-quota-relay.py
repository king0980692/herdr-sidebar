#!/usr/bin/env python3
"""Hermetic stdlib tests; never contact Magpie or read real credentials."""
import concurrent.futures
import contextlib
import http.client
import http.server
import importlib.util
import io
import json
from pathlib import Path
import socket
import subprocess
import sys
import threading
import unittest
from unittest import mock
import urllib.request

SCRIPT = Path(__file__).with_name('magpie-quota-relay.py')
spec = importlib.util.spec_from_file_location('magpie_quota_relay', SCRIPT)
relay = importlib.util.module_from_spec(spec)
spec.loader.exec_module(relay)


def payload():
    return {'data': [{'provider': 'codex', 'name': 'Codex',
                      'user': 'same@example.test · Team', 'alias': 'Work', 'plan': 'team',
                      'readAt': '2026-11-16T01:00:00Z', 'error': None,
                      'windows': [{'name': 'Weekly', 'used': 42, 'remaining': 58,
                                   'resetsAt': '2026-11-20T00:00:00Z'},
                                  {'name': 'Images', 'text': True, 'display': 'Unknown'}]}]}


def encoded(value):
    return json.dumps(value).encode()


@contextlib.contextmanager
def running(server):
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()
        thread.join(3)


class SanitizerTests(unittest.TestCase):
    def test_allowlist_preserves_identity_and_windows(self):
        raw = payload()
        raw['access_token'] = 'ROOT-SECRET'
        card = raw['data'][0]
        for key in ('id', 'cardId', 'resetCardId', 'balance', 'balances', 'token',
                    'access_token', 'refresh_token', 'unknown'):
            card[key] = 'CARD-SECRET'
            card['windows'][0][key] = 'WINDOW-SECRET'
        result = relay.sanitize(encoded(raw))
        self.assertNotIn(b'SECRET', result)
        safe = json.loads(result)
        self.assertEqual(set(safe), {'data'})
        self.assertEqual(safe, {'data': [{**payload()['data'][0], 'error': False}]})

    def test_source_error_is_boolean_and_keeps_original_freshness(self):
        raw = payload()
        raw['data'][0]['error'] = {'message': 'Bearer PRIVATE', 'token': 'SECRET'}
        safe = json.loads(relay.sanitize(encoded(raw)))['data'][0]
        self.assertIs(safe['error'], True)
        self.assertEqual(safe['readAt'], '2026-11-16T01:00:00Z')
        self.assertNotIn('PRIVATE', json.dumps(safe))
        del raw['data'][0]['readAt']
        raw['data'][0]['error'] = False
        raw['data'][0]['failed'] = True
        safe = json.loads(relay.sanitize(encoded(raw)))['data'][0]
        self.assertIs(safe['error'], True)
        self.assertNotIn('readAt', safe)
        self.assertNotIn('failed', safe)

    def test_empty_data_is_valid(self):
        self.assertEqual(relay.sanitize(b'{"data":[]}'), b'{"data":[]}')

    def test_strings_remove_controls_and_redact_credentials(self):
        raw = payload()
        raw['data'][0]['alias'] = 'A\x1b\n\u202eB'
        raw['data'][0]['plan'] = 'Bearer SECRET'
        raw['data'][0]['user'] = 'x' * 1000
        raw['data'][0]['windows'][0]['display'] = 'access_token=SECRET'
        safe = json.loads(relay.sanitize(encoded(raw)))['data'][0]
        self.assertEqual(safe['alias'], 'AB')
        self.assertEqual(safe['plan'], '[redacted]')
        self.assertEqual(len(safe['user']), 160)
        self.assertNotIn('SECRET', json.dumps(safe))

    def test_malformed_and_oversize_rejected(self):
        bad = [b'not JSON SECRET', b'[]', b'{}', b'{"data":null}',
               b'{"data":[null]}', b'{"data":[{"windows":{}}]}',
               b'{"data":[{"windows":[null]}]}',
               b'{"data":[{"windows":[{"used":NaN}]}]}',
               b'{"data":[{"windows":[{"used":true}]}]}',
               b'{"data":[{"windows":[{"used":101}]}]}',
               b'{"data":[{"windows":[{"display":{"token":"SECRET"}}]}]}',
               b'{"data":[{"alias":{"token":"SECRET"}}]}',
               b' ' * (relay.MAX_BYTES + 1)]
        for body in bad:
            with self.subTest(body=body[:80]):
                with self.assertRaises(relay.InvalidQuota):
                    relay.sanitize(body)

    def test_card_and_window_bounds(self):
        for value in ({'data': [{}] * 257}, {'data': [{'windows': [{}] * 65}]}):
            with self.assertRaises(relay.InvalidQuota):
                relay.sanitize(encoded(value))


class CacheTests(unittest.TestCase):
    def test_concurrent_cache_fetches_once_and_expires(self):
        now = [100.0]
        fetch = mock.Mock(return_value=b'{"data":[]}')
        cache = relay.QuotaCache(fetch, lambda: now[0])
        barrier = threading.Barrier(8)

        def get(_):
            barrier.wait(timeout=3)
            return cache.get()

        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            self.assertEqual(list(pool.map(get, range(8))), [b'{"data":[]}'] * 8)
        self.assertEqual(fetch.call_count, 1)
        now[0] += relay.CACHE_TTL - 1
        cache.get()
        self.assertEqual(fetch.call_count, 1)
        now[0] += 1
        cache.get()
        self.assertEqual(fetch.call_count, 2)

    def test_error_never_serves_stale(self):
        now = [0]
        fetch = mock.Mock(side_effect=[b'{"data":[]}', ValueError('SECRET'),
                                      ValueError('SECRET')])
        cache = relay.QuotaCache(fetch, lambda: now[0])
        cache.get()
        now[0] += relay.CACHE_TTL
        for _ in range(2):
            with self.assertRaises(ValueError):
                cache.get()
        self.assertIsNone(cache.body)


class RequestTests(unittest.TestCase):
    def setUp(self):
        self.fetch = mock.Mock(return_value=relay.sanitize(encoded(payload())))
        self.server = relay.RelayServer(0, relay.QuotaCache(self.fetch))
        self.context = running(self.server)
        self.context.__enter__()

    def tearDown(self):
        self.context.__exit__(None, None, None)

    def request(self, method='GET', path=relay.PATH, headers=None):
        conn = http.client.HTTPConnection('127.0.0.1', self.server.server_port, timeout=3)
        try:
            conn.request(method, path, headers=headers or {})
            response = conn.getresponse()
            return response.status, dict(response.getheaders()), response.read()
        finally:
            conn.close()

    def test_exact_get_only_and_cache(self):
        for path in ('/', '/v1/models', relay.PATH + '?x=1', relay.PATH + '/',
                     '/v1/magpie/%71uotas', 'http://127.0.0.1:3425' + relay.PATH):
            # http.client derives Host from absolute request targets unless set.
            self.assertEqual(self.request(path=path, headers={
                'Host': '127.0.0.1:%d' % self.server.server_port})[0], 404)
        self.fetch.assert_not_called()
        self.assertEqual(self.request()[0], 200)
        status, headers, body = self.request()
        self.assertEqual(status, 200)
        self.assertEqual(json.loads(body)['data'][0]['alias'], 'Work')
        self.assertEqual(headers['Cache-Control'], 'no-store')
        self.assertNotIn('Access-Control-Allow-Origin', headers)
        self.fetch.assert_called_once_with()

    def test_all_non_get_methods_are_405(self):
        for method in ('POST', 'PUT', 'PATCH', 'DELETE', 'HEAD', 'OPTIONS', 'CONNECT',
                       'TRACE', 'CUSTOM'):
            with self.subTest(method=method):
                status, headers, _ = self.request(method)
                self.assertEqual(status, 405)
                self.assertEqual(headers['Allow'], 'GET')
        self.fetch.assert_not_called()

    def test_origin_rejected_even_null_or_empty(self):
        for origin in ('https://evil.test', 'null', ''):
            self.assertEqual(self.request(headers={'Origin': origin})[0], 403)
        self.fetch.assert_not_called()

    def test_host_must_be_loopback_with_actual_port(self):
        for host in ('evil.test', '127.0.0.1', '127.0.0.1:3425',
                     '127.0.0.1.evil.test:%d' % self.server.server_port):
            self.assertEqual(self.request(headers={'Host': host})[0], 403)
        self.fetch.assert_not_called()
        for host in ('127.0.0.1', 'localhost', '[::1]'):
            self.assertEqual(self.request(headers={
                'Host': '%s:%d' % (host, self.server.server_port)})[0], 200)

    def test_missing_duplicate_hosts_and_get_body_rejected(self):
        for hostlines in ('', 'Host: localhost:%d\r\nHost: localhost:%d\r\n' %
                          (self.server.server_port, self.server.server_port)):
            with socket.create_connection(('127.0.0.1', self.server.server_port), 3) as sock:
                sock.sendall(('GET %s HTTP/1.0\r\n%s\r\n' %
                              (relay.PATH, hostlines)).encode())
                self.assertIn(b' 403 ', sock.recv(1024).split(b'\r\n')[0])
        self.assertEqual(self.request(headers={'Content-Length': '1'})[0], 400)
        self.assertEqual(self.request(headers={'Transfer-Encoding': 'chunked'})[0], 400)
        self.fetch.assert_not_called()

    def test_generic_failure_without_logs(self):
        self.fetch.side_effect = ValueError('Bearer UPSTREAM-SECRET')
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            status, _, body = self.request()
        self.assertEqual(status, 502)
        self.assertEqual(body, relay.UNAVAILABLE)
        self.assertEqual(stderr.getvalue(), '')

    def test_bounded_worker_slots(self):
        # Occupy the same gate used before thread creation, deterministically.
        for _ in range(relay.MAX_REQUESTS):
            self.assertTrue(self.server.slots.acquire(blocking=False))
        try:
            with socket.create_connection(('127.0.0.1', self.server.server_port), 3) as sock:
                sock.sendall(b'GET / HTTP/1.0\r\n\r\n')
                try:
                    self.assertEqual(sock.recv(1024), b'')
                except ConnectionResetError:
                    pass
            self.fetch.assert_not_called()
        finally:
            for _ in range(relay.MAX_REQUESTS):
                self.server.slots.release()


class UpstreamTests(unittest.TestCase):
    def setUp(self):
        self.paths = []
        self.mode = 'ok'
        owner = self

        class FakeGateway(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                owner.paths.append(self.path)
                if owner.mode.startswith('redirect'):
                    self.send_response(int(owner.mode[-3:]))
                    self.send_header('Location', '/must-not-follow-SECRET')
                    self.end_headers()
                    return
                body = encoded(payload())
                status = 200
                if owner.mode == 'malformed':
                    body = b'UPSTREAM-SECRET malformed'
                elif owner.mode in ('oversize', 'oversize-no-length'):
                    body = b'x' * (relay.MAX_BYTES + 1)
                elif owner.mode == 'error':
                    status, body = 503, b'Bearer UPSTREAM-SECRET'
                self.send_response(status)
                if owner.mode != 'oversize-no-length':
                    self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                try:
                    self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError):
                    pass

        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), FakeGateway)
        self.context = running(self.server)
        self.context.__enter__()
        # Test-only injection: production has no upstream CLI or env override.
        self.patch = mock.patch.object(relay, 'UPSTREAM',
                                       'http://127.0.0.1:%d%s' %
                                       (self.server.server_port, relay.PATH))
        self.patch.start()

    def tearDown(self):
        self.patch.stop()
        self.context.__exit__(None, None, None)

    def test_fixed_get_and_environment_proxies_ignored(self):
        with mock.patch.dict('os.environ', {'http_proxy': 'http://127.0.0.1:1',
                                           'HTTP_PROXY': 'http://127.0.0.1:1',
                                           'ALL_PROXY': 'http://127.0.0.1:1',
                                           'no_proxy': '', 'NO_PROXY': ''}):
            result = relay.fetch_quota()
        self.assertEqual(json.loads(result)['data'][0]['alias'], 'Work')
        self.assertEqual(self.paths, [relay.PATH])

    def test_redirects_never_followed(self):
        for code in (301, 302, 303, 307, 308):
            self.mode = 'redirect%d' % code
            with self.assertRaises(Exception):
                relay.fetch_quota()
        self.assertEqual(self.paths, [relay.PATH] * 5)

    def test_malformed_oversize_and_errors_become_generic_502(self):
        for mode in ('malformed', 'oversize', 'oversize-no-length', 'error'):
            self.mode = mode
            with running(relay.RelayServer(0)) as server:
                conn = http.client.HTTPConnection('127.0.0.1', server.server_port, timeout=3)
                try:
                    conn.request('GET', relay.PATH)
                    response = conn.getresponse()
                    self.assertEqual(response.status, 502, mode)
                    self.assertEqual(response.read(), relay.UNAVAILABLE)
                finally:
                    conn.close()
        self.assertEqual(self.paths, [relay.PATH] * 4)

    def test_read_limit_and_timeout_are_explicit(self):
        response = mock.MagicMock()
        response.__enter__.return_value = response
        response.status = 200
        response.headers = {}
        response.read.return_value = b'{"data":[]}'
        opener = mock.Mock()
        opener.open.return_value = response
        with mock.patch.object(urllib.request, 'build_opener', return_value=opener) as build:
            relay.fetch_quota()
        response.read.assert_called_once_with(relay.MAX_BYTES + 1)
        self.assertEqual(opener.open.call_args.kwargs['timeout'], 6)
        handlers = build.call_args.args
        self.assertEqual(handlers[0].proxies, {})
        self.assertIsInstance(handlers[1], relay.NoRedirect)
        request = opener.open.call_args.args[0]
        self.assertEqual(request.get_method(), 'GET')
        self.assertEqual(request.full_url, relay.UPSTREAM)
        self.assertNotIn('Authorization', request.headers)


class CliTests(unittest.TestCase):
    def test_help_and_invalid_ports_do_not_start_listener(self):
        result = subprocess.run([sys.executable, str(SCRIPT), '--help'],
                                capture_output=True, timeout=3)
        self.assertEqual(result.returncode, 0)
        self.assertIn(b'--port', result.stdout)
        for value in ('no', '0', '-1', '65536'):
            result = subprocess.run([sys.executable, str(SCRIPT), '--port', value],
                                    capture_output=True, timeout=3)
            self.assertEqual(result.returncode, 2)
        self.assertEqual(relay.port_number('3426'), 3426)
        self.assertEqual(relay.REQUEST_TIMEOUT, 10)


if __name__ == '__main__':
    unittest.main()
