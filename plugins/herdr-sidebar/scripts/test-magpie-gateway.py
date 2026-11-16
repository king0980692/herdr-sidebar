#!/usr/bin/env python3
"""Hermetic smoke test of a built sidebar against a fake local Magpie, no credentials.
Usage: python3 scripts/test-magpie-gateway.py target/magpie-review/release/herdr-sidebar
"""
import http.server
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
from datetime import datetime, timezone

binary = Path(sys.argv[1]).resolve(strict=True)
requests = []
mode = 'ok'


class Gateway(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        requests.append(self.path)
        assert self.path == '/v1/magpie/quotas'
        if mode == 'error':
            self.send_response(503)
            self.end_headers()
            self.wfile.write(b'Bearer fixture-credential-must-not-be-rendered')
            return
        if mode == 'redirect':
            self.send_response(302)
            self.send_header('Location', '/mutation-must-not-be-followed')
            self.end_headers()
            return
        cards = []
        for provider, user, alias, plan, used in [
            ('claude', 'same@example.test', 'Claude MAX', 'max', 49),
            ('codex', 'same@example.test', 'Personal', 'plus', 99),
            ('codex', 'same@example.test · Team', 'Work', 'team', 100),
            ('codex', 'other@example.test · Team', 'Other Work', 'team', 42),
        ]:
            cards.append(dict(provider=provider, user=user, alias=alias, plan=plan,
                              readAt=datetime.now(timezone.utc).isoformat(),
                              windows=[dict(name='Weekly', used=used),
                                       dict(name='Images', used=0, remaining=100,
                                            display='Unknown', text=True)]))
        payload = json.dumps(dict(object='list', data=cards)).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):
        pass


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Gateway)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
try:
    with tempfile.TemporaryDirectory(prefix='sidebar-magpie-smoke-') as root:
        root = Path(root)
        config = root / 'config/herdr-sidebar'
        config.mkdir(parents=True)
        (config / 'settings.json').write_text(json.dumps(dict(
            usage_source='magpie', magpie_url=f'http://127.0.0.1:{server.server_port}')))
        env = {**os.environ, 'XDG_CONFIG_HOME': str(root / 'config'),
               'HERDR_PLUGIN_STATE_DIR': str(root / 'state')}

        def run():
            result = subprocess.run([str(binary), '--usage'], env=env,
                                    capture_output=True, text=True, timeout=10, check=True)
            return result.stdout

        first, second = run(), run()
        assert first == second, 'second process must use identical cached cards'
        assert requests == ['/v1/magpie/quotas'], requests
        for identity in ['Personal · same@example.test', 'Work · same@example.test · Team',
                         'Other Work · other@example.test · Team', 'Claude MAX']:
            assert identity in first, identity
        assert first.count('Images: Unknown') == 4
        assert not any('Images' in line and '%' in line for line in first.splitlines())
        for scenario in ['error', 'redirect']:
            mode = scenario
            env['HERDR_PLUGIN_STATE_DIR'] = str(root / scenario)
            before = len(requests)
            failed = run()
            assert 'Magpie unavailable' in failed, failed
            assert 'fixture-credential' not in failed
            assert not any(line.startswith(('Claude ', 'Codex ')) for line in failed.splitlines()), failed
            assert len(requests) == before + 1, requests
        assert all(path == '/v1/magpie/quotas' for path in requests)
        print('PASS: real HTTP envelope, four distinct contexts, aliases, unknown windows, cross-process cache, sanitized errors, no redirects/native fallback')
finally:
    server.shutdown()
    server.server_close()
    thread.join(timeout=3)
