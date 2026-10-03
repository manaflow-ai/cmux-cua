import os, signal, subprocess, sys, time, json
from pathlib import Path
root=Path(__file__).resolve().parent
binary,mode=sys.argv[1:]
if mode not in {"background", "foreground", "fallback", "cache"}:
 raise SystemExit("mode must be background, foreground, fallback, or cache")
fixture=None
probe_proc=None
started=time.monotonic()
receipt={'mode':mode,'start':time.time(),'deadline_seconds':240}
def cleanup():
 if probe_proc is not None:
  try: os.killpg(probe_proc.pid,signal.SIGTERM)
  except ProcessLookupError: pass
  try: probe_proc.wait(timeout=5)
  except subprocess.TimeoutExpired:
   try: os.killpg(probe_proc.pid,signal.SIGKILL)
   except ProcessLookupError: pass
   probe_proc.wait(timeout=5)
 if fixture is not None:
  try: os.killpg(fixture.pid,signal.SIGTERM)
  except ProcessLookupError: pass
  try: fixture.wait(timeout=5)
  except subprocess.TimeoutExpired:
   try: os.killpg(fixture.pid,signal.SIGKILL)
   except ProcessLookupError: pass
   fixture.wait(timeout=5)
 receipt['elapsed_seconds']=round(time.monotonic()-started,2)
 receipt['fixture_cleaned']=fixture is None or fixture.poll() is not None
 (root/(mode+'-bounded-receipt.json')).write_text(json.dumps(receipt,indent=2))
def deadline(*_): raise TimeoutError('fixture probe deadline')
signal.signal(signal.SIGALRM,deadline)
signal.alarm(235)
try:
 fixture=subprocess.Popen([str(root/'node_modules/.bin/electron'),str(root)],
  stdout=open(root/(mode+'-fixture.log'),'w'),stderr=subprocess.STDOUT,stdin=subprocess.DEVNULL,start_new_session=True)
 receipt['fixture_launcher_pid']=fixture.pid
 time.sleep(3)
 command=[sys.executable,str(root/('cache_probe.py' if mode=='cache' else 'probe.py')),binary,'in-process',mode]
 probe_proc=subprocess.Popen(command,stdout=open(root/(mode+'-bounded-probe.log'),'w'),stderr=subprocess.STDOUT,start_new_session=True)
 probe_proc.wait(timeout=215)
 receipt['exit']=probe_proc.returncode
 if probe_proc.returncode: raise RuntimeError('probe failed; inspect log')
 report=json.loads((root/(mode+'-results.json')).read_text())
 print(json.dumps({k:report[k] for k in (('mode','assertions_passed') if mode=='cache' else ('mode','successes','attempts','snapshot_ms'))}))
finally:
 cleanup()
 print(json.dumps(receipt))
