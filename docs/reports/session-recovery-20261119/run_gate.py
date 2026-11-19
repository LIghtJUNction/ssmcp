import datetime,hashlib,json,os,pathlib,re,subprocess,sys,time
root=next(p for p in pathlib.Path(__file__).resolve().parents if (p/"Cargo.toml").is_file() and (p/"src/runtime/recovery.rs").is_file())
os.chdir(root)
run=root/"artifacts/session-recovery"/("gate-"+datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ"))
run.mkdir()
shared_target=os.environ.get("CARGO_TARGET_DIR")
if not shared_target:
    common=pathlib.Path(subprocess.check_output(["git","rev-parse","--path-format=absolute","--git-common-dir"],text=True).strip())
    shared_target=str(common.parent/"target")
env=dict(os.environ,CARGO_BUILD_JOBS="1",CARGO_TARGET_DIR=shared_target,CARGO_TERM_COLOR="never")
def digest():
    raw=subprocess.check_output(["git","ls-files","-z","--cached","--others","--exclude-standard"])
    files=[]
    for n in raw.decode().split("\0"):
        p=root/n
        if n and p.is_file() and (p.suffix in (".rs",".js",".toml",".lock") or n=="build.rs") and not n.startswith("artifacts/"):
            files.append((n,hashlib.sha256(p.read_bytes()).hexdigest()))
    files.sort()
    return {"sha256":hashlib.sha256(json.dumps(files,separators=(",",":")).encode()).hexdigest(),"files":len(files)}
def test(name,flt,threads=2):
    return (name,["cargo","test","--lib",flt,"--","--nocapture","--test-threads="+str(threads)])
steps=[
 ("fmt",["cargo","fmt","--all","--","--check"]),
 test("native-recovery","runtime::recovery::tests"),
 test("history-source-index","client::interface::history::tests::recovery_"),
 test("context-memory","client::context_memory::tests"),
 test("event-source-index","backend::events::tests::recovery_event_index"),
 test("node-history-helper","code_mode::tests::history_helpers_route_metadata_search_and_reject_action_overrides"),
 test("observation-classification","code_mode::tests::context_reads_are_observation_and_context_mutations_are_interaction"),
 test("native-capabilities","runtime::capabilities::tests"),
 test("saved-admission","session::storage::tests",1),
 ("clippy",["cargo","clippy","--all-targets","--","-D","warnings"])
]
if "--saved-resume" in sys.argv:
    steps=[steps[0],steps[-2],steps[-1]]
data={"complete":False,"ok":False,"started_at":datetime.datetime.now(datetime.timezone.utc).isoformat(),"head":subprocess.check_output(["git","rev-parse","HEAD"],text=True).strip(),"source_before":digest(),"environment":{"cargo_build_jobs":1,"target":env["CARGO_TARGET_DIR"],"rustc_wrapper_inherited":bool(env.get("RUSTC_WRAPPER"))},"steps":[]}
def save():
    t=run/"summary.tmp"
    t.write_text(json.dumps(data,indent=2)+"\n")
    t.replace(run/"summary.json")
save()
print("GATE_DIR="+str(run),flush=True)
unique=set()
for name,cmd in steps:
    print("START "+name,flush=True)
    start=time.monotonic()
    log=run/(name+".log")
    with log.open("w") as out:
        try:
            result=subprocess.run(cmd,stdout=out,stderr=subprocess.STDOUT,env=env,timeout=900)
            code=result.returncode
        except subprocess.TimeoutExpired:
            code=124
    content=log.read_text(errors="replace")
    passed=re.findall(r"^test (\S+) \.\.\. ok$",content,re.M)
    unique.update(passed)
    is_test="test" in cmd[1:2]
    ok=code==0 and (not is_test or (len(passed)>0 and "test result: ok." in content))
    row={"name":name,"command":cmd,"exit_code":code,"ok":ok,"duration_seconds":round(time.monotonic()-start,3),"passed_tests":passed,"log":log.name,"log_sha256":hashlib.sha256(log.read_bytes()).hexdigest()}
    data["steps"].append(row)
    data["unique_tests"]=sorted(unique)
    data["unique_test_count"]=len(unique)
    save()
    print("END "+name+" "+json.dumps({"ok":ok,"exit_code":code,"passed":len(passed),"seconds":row["duration_seconds"]}),flush=True)
    if not ok:
        break
data["source_after"]=digest()
data["complete"]=len(data["steps"])==len(steps)
data["ok"]=data["complete"] and all(s["ok"] for s in data["steps"]) and data["source_before"]==data["source_after"]
data["finished_at"]=datetime.datetime.now(datetime.timezone.utc).isoformat()
save()
print(json.dumps({"directory":str(run),"complete":data["complete"],"ok":data["ok"],"unique":data["unique_test_count"]}),flush=True)
sys.exit(0 if data["ok"] else 1)
