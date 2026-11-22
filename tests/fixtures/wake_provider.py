
import datetime,json,os,pathlib,sys,time
mode=os.environ['ASWAP_TEST_MODE']
provider=pathlib.Path(sys.argv[0]).name
p=pathlib.Path(os.environ['CLAUDE_CONFIG_DIR'] if provider=='claude' else os.environ['CODEX_HOME'])
args=sys.argv[1:]
if 'app-server' in args:
    for line in sys.stdin:
        req=json.loads(line)
        if req.get('method')=='initialize': result={}
        elif req.get('method')=='model/list': result={'data':[] if mode=='model' and 'synthetic-other' in (p/'auth.json').read_text() else [{'id':'synthetic-model','model':'synthetic-model'}]}
        else: continue
        print(json.dumps({'id':req['id'],'result':result}),flush=True)
    sys.exit(0)
if provider=='codex':
    assert args[:2]==['-c','cli_auth_credentials_store="file"'];args=args[2:]
id=args[1]
base=['--resume' if provider=='claude' else 'resume',id,'--model','synthetic-model']
auth=(p/('.credentials.json' if provider=='claude' else 'auth.json')).read_text()
label='current' if 'synthetic-current' in auth else 'other'
log=pathlib.Path(os.environ['ASWAP_TEST_LOG'])
transcript=p/'projects'/'-synthetic'/(id+'.jsonl') if provider=='claude' else p/'sessions'/('rollout-synthetic-'+id+'.jsonl')
if (log/(id+'-initial')).exists():
    assert label==('current' if mode=='model' else os.environ['ASWAP_TEST_RECOVERED'])
    assert args==base+['--','You were interrupted by a usage-limit account switch. Continue exactly where you left off.']
    assert 'synthetic conversation' in transcript.read_text()
    if os.environ['ASWAP_TEST_EXITED']=='yes':
        import select
        assert not select.select([sys.stdin],[],[],0)[0], 'input typed while waiting was replayed'
    (log/(id+'-resumed')).write_text(json.dumps(args))
    sys.exit(0)
assert label=='current' and args==base
transcript.parent.mkdir(parents=True,exist_ok=True)
transcript.write_text('synthetic conversation\n')
if mode=='delayed':
    (log/(id+'-initial')).write_text('ready before reporting the limit')
    while not (log.parent/'release-limit').exists(): time.sleep(.01)
if provider=='claude':
    (p/'sessions'/(str(os.getpid())+'.json')).write_text(json.dumps({'pid':os.getpid(),'sessionId':id}))
    with transcript.open('a') as out:
        out.write(json.dumps({'type':'assistant','timestamp':datetime.datetime.now(datetime.timezone.utc).isoformat(),'isApiErrorMessage':True,'error':'rate_limit'})+'\n')
if mode=='race':
    import signal
    def quota_lost(*unused):
        cache=pathlib.Path(os.environ['XDG_DATA_HOME'])/'aswap'/'profiles'/provider/'other'/'usage-cache.json'
        value=json.loads(cache.read_text())
        value['error']='provider-limit'
        value['usage']['windows'][0]['used_percent']=100
        draft=cache.with_name(cache.name+'.new.'+str(os.getpid()))
        draft.write_text(json.dumps(value));draft.replace(cache)
        (log/'revoked').write_text('quota revoked before commit')
        sys.exit(1)
    signal.signal(signal.SIGTERM,quota_lost)
if provider=='codex':
    # A real limit: the account's quota reads exhausted.
    cache=pathlib.Path(os.environ['XDG_DATA_HOME'])/'aswap'/'profiles'/provider/label/'usage-cache.json'
    value=json.loads(cache.read_text())
    value['usage']['windows'][0]['used_percent']=100
    draft=cache.with_name(cache.name+'.limit.'+str(os.getpid()))
    draft.write_text(json.dumps(value));draft.replace(cache)
print("You've hit your weekly limit" if provider=='claude' else 'usage_limit_reached',flush=True)
(log/(id+'-initial')).write_text('limited')
if os.environ['ASWAP_TEST_EXITED']=='yes': sys.exit(1)
if mode=='race':
    # Remain alive until the supervisor has selected a target and stops us.
    while True:
        signal.pause()
time.sleep(12 if mode=='model' else 25)
sys.exit(9)
