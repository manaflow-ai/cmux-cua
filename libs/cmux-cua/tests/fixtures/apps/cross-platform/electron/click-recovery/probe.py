import json, subprocess, time, sys
from pathlib import Path
root=Path(__file__).resolve().parent
binary, socket, mode=sys.argv[1:]
if mode not in {'background', 'foreground', 'fallback'}:
 raise SystemExit('mode must be background, foreground, or fallback')
import atexit, selectors
mcp = None
request_id = 0
if socket == 'in-process':
 mcp = subprocess.Popen([binary, 'mcp', '--no-daemon-relaunch', '--no-overlay'],
  stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(root/(mode+'-mcp.log'),'w'),text=True,bufsize=1)
 def stop_mcp():
  mcp.stdin.close()
  try: mcp.wait(timeout=5)
  except subprocess.TimeoutExpired: mcp.terminate(); mcp.wait(timeout=5)
 atexit.register(stop_mcp)
def rpc(method,params):
 global request_id
 request_id += 1
 mcp.stdin.write(json.dumps({'jsonrpc':'2.0','id':request_id,'method':method,'params':params})+'\n');mcp.stdin.flush()
 select=selectors.DefaultSelector(); select.register(mcp.stdout,selectors.EVENT_READ)
 try:
  while True:
   if not select.select(40): raise TimeoutError(method)
   line=mcp.stdout.readline()
   if not line: raise RuntimeError('MCP process exited')
   response=json.loads(line)
   if response.get('id')==request_id: return response
 finally: select.close()
if mcp:
 rpc('initialize',{'protocolVersion':'2024-11-05','capabilities':{},'clientInfo':{'name':'electron-click-probe','version':'1'}})
def call(tool,args):
 t=time.monotonic()
 if mcp:
  response=rpc('tools/call',{'name':tool,'arguments':args})
  result=response.get('result',{})
  out=result.get('structuredContent')
  if out is None:
   text='\n'.join(c.get('text','') for c in result.get('content',[]) if c.get('type')=='text')
   try: out=json.loads(text)
   except Exception: out={'text':text,'error':response.get('error')}
  return out,round((time.monotonic()-t)*1000,1),int(bool(result.get('isError') or response.get('error')))
 p=subprocess.run([binary,'call',tool,json.dumps(args),'--socket',socket],text=True,capture_output=True,timeout=40)
 try: out=json.loads(p.stdout)
 except Exception: out={'raw':p.stdout[:1200],'stderr':p.stderr[:1200]}
 return out, round((time.monotonic()-t)*1000,1),p.returncode
apps,_,_=call('list_apps',{})
candidates=[a for a in apps['apps'] if a.get('bundle_id')=='com.github.Electron' and a.get('running')]
if len(candidates)!=1:
 raise SystemExit('Run exactly one Electron fixture instance before probing')
pid=candidates[0]['pid']
finder=next(a['pid'] for a in apps['apps'] if a.get('bundle_id')=='com.apple.finder')
wins,_,_=call('list_windows',{'pid':pid})
win=max((w for w in wins['windows'] if w['bounds']['height']>100 and w['layer']==0),key=lambda w:w['bounds']['width'])
wid=win['window_id']
snapshot,ms,rc=call('get_window_state',{'pid':pid,'window_id':wid,'screenshot_out_file':str(root/(mode+'-window.png'))})
(root/(mode+'-snapshot.json')).write_text(json.dumps(snapshot,indent=2))
rows=[]
for i in range(20):
 call('bring_to_front',{'pid':finder})
 time.sleep(.15)
 before=json.loads((root/'counts.json').read_text())
 args={'pid':pid,'window_id':wid,'x':85+(i%5)*145,'y':138+(i//5)*65}
 if mode=='fallback': args['fallback']='foreground'
 if mode=='foreground': args['delivery_mode']='foreground'
 result,elapsed,code=call('click',args)
 time.sleep(.2)
 after=json.loads((root/'counts.json').read_text())
 rows.append({'i':i,'landed':after.get(str(i),0)==before.get(str(i),0)+1,'ms':elapsed,'exit':code,'result':result})
 report={'mode':mode,'pid':pid,'window_id':wid,'window_bounds':win['bounds'],'snapshot_ms':ms,'snapshot_exit':rc,'successes':sum(r['landed'] for r in rows),'attempts':len(rows),'rows':rows}
 (root/(mode+'-results.json')).write_text(json.dumps(report,indent=2))
 print(json.dumps({'mode':mode,'progress':len(rows),'successes':report['successes']}),flush=True)
print('DONE '+json.dumps({k:v for k,v in report.items() if k!='rows'}))

if mode in {'foreground', 'fallback'} and report['successes'] != 20:
 raise SystemExit('Foreground delivery/recovery failed the 20-click check')
