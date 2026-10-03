import json, subprocess, time, sys
from pathlib import Path
root=Path(__file__).resolve().parent
binary, socket, mode=sys.argv[1:]
if mode not in {'cache'}:
 raise SystemExit('mode must be cache')
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
pid=next(a['pid'] for a in apps['apps'] if a.get('bundle_id')=='com.github.Electron' and a.get('running'))
wins,_,_=call('list_windows',{'pid':pid})
win=next(w for w in wins['windows'] if w['bounds']['height']>100 and w['layer']==0)
base={'pid':pid,'window_id':win['window_id'],'include_screenshot':False}
rows=[]
def state(extra=None):
 out,ms,rc=call('get_window_state',{**base,**(extra or {})})
 assert rc==0,out
 rows.append({'args':extra or {},'ms':ms,'snapshot':out})
 (root/'cache-results.json').write_text(json.dumps({'mode':mode,'assertions_passed':False,'snapshots':rows},indent=2))
 return out
def compact(snapshot): return snapshot['ax_snapshot']['compact']
def find(snapshot,label): return next(n for n in compact(snapshot) if n.get('label')==label)
def mutate(message):
 temp=root/'mutation.tmp'
 temp.write_text(json.dumps(message))
 temp.replace(root/'mutation.json')
 time.sleep(.5)
first=state()
assert compact(first), 'fixture must expose compact controls'
assert all(n['id'].startswith(f'ax-{pid}-{win["window_id"]}-') for n in compact(first)), compact(first)
time.sleep(.25)
second=state()
assert second['ax_snapshot']['cache_hit'], second['ax_snapshot']
assert second['ax_snapshot']['ax_reads']==0
assert not any(second['ax_snapshot']['diff'][key] for key in ('added','removed','updated'))
original=find(second,'Probe 0')
filtered=state({'role':'AXButton','label':'Probe 0'})
assert all(n['role']=='AXButton' and 'Probe 0' in n.get('label','') for n in compact(filtered))
assert find(filtered,'Probe 0')['element_index']==original['element_index']
region=state({'region':{'x':0,'y':90,'w':220,'h':100}})
assert all(n.get('frame') and n['frame']['x']<220 and n['frame']['x']+n['frame']['w']>0 for n in compact(region))
mutate({'kind':'value','value':'after'})
value=state()
assert find(value,'Text probe').get('value')=='after',compact(value)
assert find(value,'Probe 0')['id']==original['id']
assert value['ax_snapshot']['diff']['updated'],value['ax_snapshot']
mutate({'kind':'children','add':True})
children=state()
added=find(children,'Added control')
assert added['id'] in children['ax_snapshot']['diff']['added'],children['ax_snapshot']
mutate({'kind':'layout','x':240})
layout=state()
moved=find(layout,'Added control')
assert moved['id']==added['id']
assert moved['frame']['x']>added['frame']['x']+100,(added,moved)
assert moved['id'] in layout['ax_snapshot']['diff']['updated'],layout['ax_snapshot']
mutate({'kind':'children','add':False})
removed=state()
assert added['id'] in removed['ax_snapshot']['diff']['removed'],removed['ax_snapshot']
assert find(removed,'Probe 0')['id']==original['id']
shot=state({'include_accessibility_tree':False,'include_screenshot':True,'screenshot_out_file':str(root/'cache-only.png')})
assert shot['element_count']==0,shot
_,_,rc=call('click',{'pid':pid,'window_id':win['window_id'],'element_index':original['element_index']})
assert rc,'screenshot-only retained stale action indices'
report={'mode':mode,'assertions_passed':True,'snapshots':rows}
(root/'cache-results.json').write_text(json.dumps(report,indent=2))
print(json.dumps({'mode':mode,'assertions_passed':True,'snapshots':len(rows),'refreshes':[{'ms':r['ms'],'ax':r['snapshot'].get('ax_walk')} for r in rows]}))
