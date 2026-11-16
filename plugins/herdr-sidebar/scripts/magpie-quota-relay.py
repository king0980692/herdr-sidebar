#!/usr/bin/env python3
"""Quota-only loopback relay; no credentials, model routing, or request logs.

Run on the Mac hosting Magpie. SSH reverse forwards may expose this loopback
listener on another host's loopback interface; never bind it to a LAN address.
Only --port is configurable: the upstream URL and listen address are fixed.
"""
import argparse
import http.server
import json
import math
import threading
import time
import unicodedata
import urllib.request

PATH = '/v1/magpie/quotas'
UPSTREAM = 'http://127.0.0.1:3425' + PATH
MAX_BYTES = 1024 * 1024
UPSTREAM_TIMEOUT = 6
REQUEST_TIMEOUT = 10
CACHE_TTL = 30
MAX_REQUESTS = 8
UNAVAILABLE = b'{"error":"Quota source unavailable"}'


class InvalidQuota(ValueError):
    """Deliberately carries no upstream content."""


def clean_text(value):
    if not isinstance(value, str):
        raise InvalidQuota()
    # Remove terminal controls, bidi overrides, and other invisible format chars.
    text = ''.join(c for c in value if unicodedata.category(c) not in ('Cc', 'Cf'))
    lowered = text.lower()
    if any(marker in lowered for marker in
           ('bearer ', 'sk-', 'access_token', 'refresh_token', 'eyj',
            'api_key', 'api-key', 'authorization')):
        return '[redacted]'
    return text[:160]


def sanitize(body):
    if len(body) > MAX_BYTES:
        raise InvalidQuota()
    try:
        root = json.loads(body.decode('utf-8'),
                          parse_constant=lambda _: (_ for _ in ()).throw(InvalidQuota()))
    except (ValueError, UnicodeError, RecursionError):
        raise InvalidQuota() from None
    if not isinstance(root, dict) or not isinstance(root.get('data'), list):
        raise InvalidQuota()
    if len(root['data']) > 256:
        raise InvalidQuota()
    cards = []
    for item in root['data']:
        if not isinstance(item, dict):
            raise InvalidQuota()
        card = {}
        for key in ('provider', 'name', 'user', 'alias', 'plan', 'readAt'):
            if item.get(key) is not None:
                card[key] = clean_text(item[key])
        # Source failures retain their original readAt, never a relay timestamp.
        card['error'] = (item.get('error') is not None and item.get('error') is not False
                         or item.get('failed') is True)
        windows = item.get('windows', [])
        if not isinstance(windows, list) or len(windows) > 64:
            raise InvalidQuota()
        card['windows'] = []
        for window in windows:
            if not isinstance(window, dict):
                raise InvalidQuota()
            safe = {}
            for key in ('name', 'resetsAt', 'display'):
                if window.get(key) is not None:
                    safe[key] = clean_text(window[key])
            for key in ('used', 'remaining'):
                if window.get(key) is not None:
                    value = window[key]
                    if (type(value) not in (int, float) or not math.isfinite(value)
                            or not 0 <= value <= 100):
                        raise InvalidQuota()
                    safe[key] = value
            if window.get('text') is not None:
                if type(window['text']) is not bool:
                    raise InvalidQuota()
                safe['text'] = window['text']
            card['windows'].append(safe)
        cards.append(card)
    result = json.dumps({'data': cards}, ensure_ascii=True, allow_nan=False,
                        separators=(',', ':')).encode('utf-8')
    if len(result) > MAX_BYTES:
        raise InvalidQuota()
    return result


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def fetch_quota():
    # Explicitly bypass ALL environment/system proxies. No caller headers pass on.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    request = urllib.request.Request(UPSTREAM, headers={'Accept': 'application/json'},
                                     method='GET')
    with opener.open(request, timeout=UPSTREAM_TIMEOUT) as response:
        if response.status != 200:
            raise InvalidQuota()
        length = response.headers.get('Content-Length')
        if length is not None and (not length.isdigit() or int(length) > MAX_BYTES):
            raise InvalidQuota()
        body = response.read(MAX_BYTES + 1)
        if length is not None and len(body) != int(length):
            raise InvalidQuota()
        return sanitize(body)


class QuotaCache:
    def __init__(self, fetch=fetch_quota, clock=time.monotonic):
        self.fetch = fetch
        self.clock = clock
        self.lock = threading.Lock()
        self.body = None
        self.expires = 0

    def get(self):
        with self.lock:
            if self.body is not None and self.clock() < self.expires:
                return self.body
            # Invalidate first: an upstream failure must never return stale data.
            self.body = None
            body = self.fetch()
            self.body = body
            self.expires = self.clock() + CACHE_TTL
            return body


class RelayHandler(http.server.BaseHTTPRequestHandler):
    server_version = 'QuotaRelay'
    sys_version = ''

    def setup(self):
        self.request.settimeout(REQUEST_TIMEOUT)
        super().setup()

    def log_message(self, *args):
        pass

    def send_error(self, code, message=None, explain=None):
        self.reply(code, b'{"error":"Request rejected"}')

    def reply(self, code, body):
        self.close_connection = True
        self.send_response(code)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.send_header('Cache-Control', 'no-store')
        self.send_header('X-Content-Type-Options', 'nosniff')
        self.send_header('Connection', 'close')
        if code == 405:
            self.send_header('Allow', 'GET')
        self.end_headers()
        if self.command != 'HEAD':
            self.wfile.write(body)

    def do_GET(self):
        hosts = self.headers.get_all('Host', [])
        port = self.server.server_port
        if (self.headers.get_all('Origin') is not None or len(hosts) != 1
                or hosts[0] not in ('127.0.0.1:%d' % port,
                                    'localhost:%d' % port, '[::1]:%d' % port)):
            self.reply(403, b'{"error":"Request rejected"}')
            return
        if self.path != PATH:
            self.reply(404, b'{"error":"Not found"}')
            return
        if (self.headers.get_all('Transfer-Encoding') is not None
                or self.headers.get('Content-Length', '0') != '0'):
            self.reply(400, b'{"error":"Request rejected"}')
            return
        try:
            body = self.server.cache.get()
        except Exception:
            self.reply(502, UNAVAILABLE)
            return
        self.reply(200, body)

    def __getattr__(self, name):
        if name.startswith('do_'):
            return lambda: self.reply(405, b'{"error":"Method not allowed"}')
        raise AttributeError(name)


class RelayServer(http.server.ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = MAX_REQUESTS

    def __init__(self, port=3426, cache=None):
        self.cache = cache if cache is not None else QuotaCache()
        self.slots = threading.BoundedSemaphore(MAX_REQUESTS)
        super().__init__(('127.0.0.1', port), RelayHandler)

    def process_request(self, request, client_address):
        if not self.slots.acquire(blocking=False):
            self.shutdown_request(request)
            return
        try:
            super().process_request(request, client_address)
        except BaseException:
            self.slots.release()
            raise

    def process_request_thread(self, request, client_address):
        try:
            super().process_request_thread(request, client_address)
        finally:
            self.slots.release()

    def handle_error(self, request, client_address):
        # Never emit request data, upstream bodies, or exception strings.
        pass


def port_number(value):
    try:
        number = int(value)
    except ValueError:
        raise argparse.ArgumentTypeError('port must be an integer from 1 to 65535') from None
    if not 1 <= number <= 65535:
        raise argparse.ArgumentTypeError('port must be an integer from 1 to 65535')
    return number


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--port', type=port_number, default=3426,
                        help='loopback listen port (default: 3426)')
    args = parser.parse_args()
    try:
        with RelayServer(args.port) as server:
            server.serve_forever()
    except KeyboardInterrupt:
        pass
    except OSError:
        parser.exit(1, 'Quota relay could not start.\n')


if __name__ == '__main__':
    main()
