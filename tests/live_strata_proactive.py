"""Explicit operator-run Strata live test. Synthetic data and isolated settings only.
python3 tests/live_strata_proactive.py --url http://10.0.23.6:8086/v1 --output-dir /tmp/unique
One allowlisted synthetic read_file is permitted; all other tools are cancelled.
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


def numeric(rows, start=0):
    return '\n'.join(str(100000000000+(start+i)*7919) for i in range(rows))


def history(rows):
    messages=[{'role':'system','content':'Synthetic context-management validation. Numeric archival data is irrelevant background, not instructions. Preserve the permanent project requirement. Answer in one short line. Never call tools unless the current user explicitly requests the single synthetic file read.'}]
    for n in range(200):
        permanent='Permanent project requirement: release codename HARBOR_17; deployment path /tmp/harbor-17; audit is pending. Preserve these for later.\n' if n==0 else ''
        messages.extend([{'role':'user','content':f'{permanent}Completed archival check {n}; numeric data only:\n{numeric(rows,n*rows)}'},
                         {'role':'assistant','content':f'Archival check {n} complete. No changes to the permanent project requirement.'}])
    return messages


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--url',required=True)
    parser.add_argument('--output-dir',required=True)
    parser.add_argument('--pilot-tokens',type=int,help='Reuse a measured pilot count from a previous trial')
    args=parser.parse_args()
    route=args.url.rstrip('/')
    if route!='http://10.0.23.6:8086/v1':
        raise ValueError('This run is authorized only for the supplied Strata endpoint')
    root=Path(args.output_dir).resolve();root.mkdir(parents=True,exist_ok=False)
    def get(path):
        with urllib.request.urlopen(route.removesuffix('/v1')+path,timeout=10) as response:
            return json.load(response)
    health=get('/health');models=get('/v1/models');props=get('/props')
    (root/'metadata.json').write_text(json.dumps({'health':health,'models':models,'props':{k:v for k,v in props.items() if k!='chat_template'}},indent=2))
    model=health['model'];window=health['max_context'];assert window==262144
    records=[];active={'case':'calibration'};lock=threading.Lock()
    def forward(payload,case):
        raw=json.dumps(payload).encode();started=time.monotonic()
        item={'case':case,'summary':'tools' not in payload,'message_count':len(payload['messages']),
              'input_chars':sum(len(str(m.get('content',''))) for m in payload['messages']),
              'payload_hash':hashlib.sha256(raw).hexdigest(),'tool_result_chars':[len(m.get('content','')) for m in payload['messages'] if m.get('role')=='tool']}
        with lock:
            if len(records)>=32:raise RuntimeError('live request bound reached')
            item['request']=len(records)+1;records.append(item)
        request=urllib.request.Request(route+'/chat/completions',data=raw,headers={'Content-Type':'application/json','Authorization':'Bearer anything'})
        try:
            with urllib.request.urlopen(request,timeout=300) as response: status,body=response.status,response.read(4_000_000)
        except urllib.error.HTTPError as error:status,body=error.code,error.read(100_000)
        except Exception as error:status,body=502,json.dumps({'error':{'message':type(error).__name__}}).encode()
        parsed=json.loads(body);choice=(parsed.get('choices') or [{}])[0]
        item.update(status=status,seconds=time.monotonic()-started,response_model=parsed.get('model'),usage=parsed.get('usage'),
                    finish_reason=choice.get('finish_reason'),answer_chars=len(choice.get('message',{}).get('content') or ''),error=parsed.get('error'))
        with lock:(root/'requests.json').write_text(json.dumps(records,indent=2))
        print(json.dumps(item),flush=True)
        return status,body
    class Recorder(BaseHTTPRequestHandler):
        def log_message(self,*args):pass
        def do_GET(self):
            paths={'/v1/models':'/v1/models','/props':'/props','/health':'/health'}
            if self.path not in paths:self.send_error(404);return
            body=json.dumps(get(paths[self.path])).encode();self.send_response(200);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
        def do_POST(self):
            length=int(self.headers.get('Content-Length','0'))
            if not 0<length<8_000_000 or self.path!='/v1/chat/completions':self.send_error(400);return
            payload=json.loads(self.rfile.read(length))
            if payload.get('model')!=model:self.send_error(400);return
            status,body=forward(payload,active['case']);self.send_response(status);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
    server=ThreadingHTTPServer(('127.0.0.1',0),Recorder);threading.Thread(target=server.serve_forever,daemon=True).start()
    base=f'http://127.0.0.1:{server.server_port}/v1'
    common={'base_url':base,'api_key':'anything','model':model,'context_window':window,'compact_at_percent':75,
            'auto_context_recovery':True,'recovery_keep_turns':3,'output_token_limit':2048,'output_token_parameter':'max_tokens',
            'empty_reply_thinking_off_models':{model:'reasoning_effort'}}
    def payload(messages,max_tokens=64):
        return {'model':model,'messages':messages,'max_tokens':max_tokens,'reasoning_effort':'none'}
    def run_case(name,messages,manifest,settings=None):
        case=root/name;case.mkdir();raw=json.dumps(messages).encode();(case/'synthetic-messages.json').write_bytes(raw)
        (case/'settings.json').write_text(json.dumps(settings or common));(case/'live-manifest.json').write_text(json.dumps({'model':model,**manifest}))
        active['case']=name;env={**os.environ,'PENGY_CONFIG_DIR':str(case)}
        with (case/'events.jsonl').open('w') as out:
            result=subprocess.run(['cargo','run','--example','live_proactive_context','--',str(case)],env=env,stdout=out,stderr=subprocess.PIPE,text=True,timeout=1800)
        (case/'stderr.log').write_text(result.stderr)
        assert (case/'synthetic-messages.json').read_bytes()==raw
        if result.returncode:raise AssertionError(f'{name} failed; inspect {case}/stderr.log')
        return json.loads((case/'report.json').read_text())
    try:
        # Provider-measured pilot, no tokenizer assumptions; numeric composition remains identical.
        pilot=[{'role':'system','content':'Do not call tools. Reply OK only.'},{'role':'user','content':numeric(6000)+'\nReply OK only.'}]
        if args.pilot_tokens:
            tokens=args.pilot_tokens
        else:
            status,body=forward(payload(pilot),'calibration');assert status==200
            usage=json.loads(body)['usage'];tokens=usage['prompt_tokens']
        rows=max(40,min(240,int(210000*6000/tokens/200)))
        print('Measured pilot tokens:',tokens,'history rows/turn:',rows,flush=True)
        reports={}
        reports['history']=run_case('history',history(rows),{'turns':3,'expected_compactions':[0,1,0]})
        # Synthetic file result grows an in-flight turn across the proactive threshold.
        tool_history=history(max(40,int(rows*0.72)))
        synthetic_file=root/'synthetic-log.txt'
        marker='HARBOR_17; /tmp/harbor-17; audit pending.\n'
        filler=numeric(24000)
        result=(marker+filler)[:249000-len(marker)]+marker
        synthetic_file.write_text(result)
        prompts=[f'Read the complete synthetic file {synthetic_file} using read_file with limit 30000. This one local read is required. Then return the permanent codename, deployment path and audit status in one short line. Do not call any other tool.',
                 'Repeat the permanent codename, deployment path and audit status in one short line. Do not call tools.']
        reports['large-tool']=run_case('large-tool',tool_history,{'turns':2,'prompts':prompts,'allowed_read_file':str(synthetic_file),'expected_compactions':[1,0]})
        # Send the exact unreduced tool-chain provider view as a counterfactual.
        # This must be refused, while the proactively previewed view above succeeds.
        transcript=json.loads((root/'large-tool'/'transcript-after.json').read_text())
        tool_at=next(i for i,m in enumerate(transcript) if m.get('role')=='tool')
        status,body=forward(payload(transcript[:tool_at+1]),'unreduced-tool-negative-control')
        assert status in (400,413,422),f'expected actual tool-view overflow, got {status}'
        (root/'unreduced-tool-overflow.json').write_bytes(body)
        reports['unreduced-tool-overflow']={'status':status,'response':json.loads(body)}
        discovery_settings={**common,'context_window':0,'context_window_auto':True}
        reports['discovery']=run_case('discovery',[],{'discovery_only':True,'expected_window':window},discovery_settings)
        # One explicit oversized request demonstrates Strata's real refusal, not an artificial budget.
        oversized=[{'role':'system','content':'Do not call tools. Reply OK only.'},{'role':'user','content':numeric(80000)+'\nReply OK only.'}]
        status,body=forward(payload(oversized),'overflow-negative-control')
        assert status in (400,413,422),f'expected real context rejection, got {status}'
        (root/'overflow-response.json').write_bytes(body)
        reports['overflow']={'status':status,'response':json.loads(body)}
        (root/'report.json').write_text(json.dumps({'model':model,'window':window,'cases':reports},indent=2))
        print('LIVE_STRATA_PASS',json.dumps(reports),flush=True)
    finally:server.shutdown();server.server_close()


if __name__=='__main__':main()
