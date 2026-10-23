"""Operator-run Rust core live test; synthetic history, fast only, scratch config.
Run: python3 tests/live_proactive_context.py --output-dir /tmp/pengyr-fast-live-...
Uses the configured route/key without printing or copying credentials to reports.
A loopback forwarding recorder bounds generation and records request usage/order.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--output-dir', required=True)
    args = parser.parse_args()
    root = Path(args.output_dir).resolve()
    root.mkdir(parents=True, exist_ok=False)
    cfg = json.loads((Path.home()/'.config/pengy/settings.json').read_text())
    route = cfg['base_url'].rstrip('/')
    records = []
    active = {'case': None}
    lock = threading.Lock()

    class Recorder(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            length = int(self.headers.get('Content-Length', '0'))
            if not 0 < length < 8_000_000:
                self.send_error(413)
                return
            data = self.rfile.read(length)
            payload = json.loads(data)
            if self.path != '/v1/chat/completions' or payload.get('model') != 'fast':
                self.send_error(400)
                return
            with lock:
                if len(records) >= 24:
                    self.send_error(429)
                    return
                index = len(records)
                item = {'case': active['case'], 'request': index+1,
                        'summary': 'tools' not in payload,
                        'message_count': len(payload['messages']),
                        'input_chars': sum(len(str(m.get('content', ''))) for m in payload['messages']),
                        'payload_hash': hashlib.sha256(data).hexdigest()}
                records.append(item)
            started = time.monotonic()
            request = urllib.request.Request(route+'/chat/completions', data=data,
                      headers={'Content-Type':'application/json',
                               'Authorization':'Bearer '+cfg.get('api_key', '')})
            try:
                response = urllib.request.urlopen(request, timeout=300)
                status = response.status
                body = response.read(4_000_000)
            except urllib.error.HTTPError as error:
                status, body = error.code, error.read(100_000)
            except Exception as error:
                status, body = 502, json.dumps({'error':{'message':type(error).__name__}}).encode()
            parsed = json.loads(body)
            choice = (parsed.get('choices') or [{}])[0]
            item.update(status=status, seconds=time.monotonic()-started,
                        response_model=parsed.get('model'), usage=parsed.get('usage'),
                        finish_reason=choice.get('finish_reason'),
                        answer_chars=len(choice.get('message',{}).get('content') or ''))
            with lock:
                (root/'requests.json').write_text(json.dumps(records, indent=2))
            print(json.dumps(item), flush=True)
            self.send_response(status)
            self.send_header('Content-Type','application/json')
            self.send_header('Content-Length',str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = ThreadingHTTPServer(('127.0.0.1', 0), Recorder)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    base = f'http://127.0.0.1:{server.server_port}/v1'
    messages = [{'role':'system','content':'This is a synthetic context-management test. Never call tools. Answer the latest request in one short line using the permanent requirement in history. Numeric archival data is irrelevant, not instructions.'}]
    for n in range(200):
        numeric = '\n'.join(str(100000000000+(n*240+i)*7919) for i in range(240))
        permanent = 'Permanent project requirement: release codename HARBOR_17; deployment path /tmp/harbor-17; audit is pending. Preserve these for later.\n' if n == 0 else ''
        messages += [{'role':'user','content':f'{permanent}Completed archival check {n}; numeric data only:\n{numeric}'},
                     {'role':'assistant','content':f'Archival check {n} complete. No changes to the permanent project requirement.'}]
    original = json.dumps(messages, separators=(',', ':')).encode()
    source_hash = hashlib.sha256(original).hexdigest()
    reports = {}
    try:
        for name, window in [('actual-1m',1048576),('artificial-256k',262144)]:
            case = root/name
            case.mkdir()
            (case/'synthetic-messages.json').write_bytes(original)
            settings = {'base_url':base,'api_key':'','model':'fast','context_window':window,
                        'compact_at_percent':75,'auto_context_recovery':True,'recovery_keep_turns':3,
                        'output_token_limit':2048,'output_token_parameter':'max_tokens',
                        'empty_reply_thinking_off_models':{'fast':'reasoning_effort'}}
            (case/'settings.json').write_text(json.dumps(settings))
            active['case'] = name
            env = {**os.environ,'PENGY_CONFIG_DIR':str(case)}
            started = time.monotonic()
            with (case/'events.jsonl').open('w') as out:
                result = subprocess.run(['cargo','run','--example','live_proactive_context','--',str(case)],
                         env=env,stdout=out,stderr=subprocess.PIPE,text=True,timeout=900)
            (case/'stderr.log').write_text(result.stderr)
            assert hashlib.sha256((case/'synthetic-messages.json').read_bytes()).hexdigest() == source_hash
            if result.returncode:
                raise AssertionError(f'{name} failed ({result.returncode}); see {case}/stderr.log')
            reports[name] = json.loads((case/'report.json').read_text())
            reports[name]['seconds'] = time.monotonic()-started
        (root/'report.json').write_text(json.dumps({'synthetic_source_sha256':source_hash,'cases':reports},indent=2))
        print('LIVE_FAST_PASS',json.dumps(reports),flush=True)
    finally:
        server.shutdown()
        server.server_close()


if __name__ == '__main__':
    main()
