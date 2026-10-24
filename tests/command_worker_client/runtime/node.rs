//! Actual installed Node/npm compatibility, independent of Rust's toolchain inputs.
use super::*;

#[tokio::test]
#[ignore = "requires installed Node/npm; unix-sdk-native workflow runs this exact test"]
async fn installed_node_builds_and_tests_in_default_auto() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().canonicalize().unwrap();
    let workspace = home.join("workspace");
    let host_workspace = home.join("host-workspace");
    let package = json!({"name":"qq-node-proof","version":"1.0.0","private":true,"scripts":{"build":"node build.js","test":"node test.js"}});
    for directory in [&workspace, &host_workspace] {
        std::fs::create_dir(directory).unwrap();
        std::fs::write(directory.join("package.json"), package.to_string()).unwrap();
        std::fs::write(
            directory.join("build.js"),
            "const os=require('os'),fs=require('fs'),assert=require('assert');assert.ok(os.type().length>0);assert.ok(os.version().length>0);fs.writeFileSync('node-os-proof.json',JSON.stringify({type:os.type(),version:os.version()}));fs.writeFileSync('built.txt','built');\n",
        )
        .unwrap();
        std::fs::write(directory.join("test.js"), "const fs=require('fs');require('assert').strictEqual(fs.readFileSync('built.txt','utf8'),'built');fs.writeFileSync('tested.txt','tested');\n").unwrap();
        std::fs::write(directory.join("spawn-probe.js"), r#"
const cp=require('child_process'),fs=require('fs'),path=require('path');
(async()=>{
const probes=['direct','shell'].map(kind=>{
  const script=`require('fs').writeFileSync('child-${kind}.txt','yes')`;
  const file=kind==='direct'?process.execPath:'/bin/sh';
  const args=kind==='direct'?['-e',script]:['-c','node -e '+JSON.stringify(script)];
  const result=cp.spawnSync(file,args,{stdio:'inherit',detached:false,timeout:5000});
  const error=result.error?{code:result.error.code,errno:result.error.errno,syscall:result.error.syscall,message:result.error.message}:null;
  return {kind,status:result.status,signal:result.signal,error};
});
const paths=[];
for(let current=process.cwd();;current=path.dirname(current)){
  paths.push(path.join(current,'node_modules','.bin'));
  if(path.dirname(current)===current)break;
}
paths.push(process.env.PATH);
for(const kind of ['async-absolute','async-path','async-npm-path']){
  const file=kind==='async-absolute'?'/bin/sh':'sh';
  const env=kind==='async-npm-path'?{...process.env,PATH:paths.join(path.delimiter)}:process.env;
  const result=await new Promise(resolve=>{
    const failed=error=>resolve({kind,status:null,signal:null,error:{code:error.code,errno:error.errno,syscall:error.syscall,message:error.message}});
    try{
      const child=cp.spawn(file,['-c',"node -e \"require('fs').writeFileSync('child-async.txt','yes')\""],{cwd:process.cwd(),env,stdio:'inherit',detached:false,signal:AbortSignal.timeout(5000)});
      child.once('error',failed);
      child.once('close',(status,signal)=>resolve({kind,status,signal,error:null}));
    }catch(error){failed(error)}
  });
  console.log(JSON.stringify(result));
  probes.push(result);
}
fs.writeFileSync('node-spawn-probe.json',JSON.stringify(probes));
console.log(JSON.stringify(probes));
})().catch(error=>{console.error(error);process.exitCode=1});
"#).unwrap();
    }
    assert!(
        std::process::Command::new("git")
            .args(["-c", "init.templateDir=", "init", "-q", "--template="])
            .arg(&workspace)
            .status()
            .unwrap()
            .success()
    );
    const COMMAND: &str = "command -v node; command -v npm; node --version && node spawn-probe.js && npm run build --offline && npm test --offline && cat tested.txt";
    let host = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new("/bin/sh")
            .args(["-c", COMMAND])
            .current_dir(&host_workspace)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    println!(
        "Host control stdout: {}\nHost control stderr: {}",
        String::from_utf8_lossy(&host.stdout),
        String::from_utf8_lossy(&host.stderr)
    );
    assert!(host.status.success());
    let host_os: Value =
        serde_json::from_slice(&std::fs::read(host_workspace.join("node-os-proof.json")).unwrap())
            .unwrap();
    assert_eq!(
        std::fs::read_to_string(host_workspace.join("tested.txt")).unwrap(),
        "tested"
    );
    let worker = client::CommandWorker::new(
        data_directory(&home).join("command-owner-v3"),
        env!("CARGO_BIN_EXE_qq").into(),
        concat!(
            env!("CARGO_PKG_VERSION"),
            "+",
            env!("QQ_BUILD_REVISION"),
            ".",
            env!("QQ_BUILD_DATE")
        ),
    )
    .unwrap();
    worker.identity().await.unwrap();
    let completed = json!({"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":2}}});
    let provider = LoopbackServer::respond_sequence_with_timeout(
        vec![
            reply(vec![
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_node","call_id":"call_node","name":"shell","arguments":""}}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_node","call_id":"call_node","name":"shell","arguments":json!({"command":COMMAND}).to_string()}}),
                completed.clone(),
            ]),
            reply(vec![
                json!({"type":"response.output_text.delta","delta":"Node build result recorded"}),
                completed,
            ]),
        ],
        Duration::from_secs(90),
    );
    let config = format!(
        r#"(version: 1, model: "fake/model", providers: {{"fake": Custom(connection: (base_url: "{}/v1", api: OpenAiResponses, auth: NoAuth), models: {{"model": (name: "Fake")}})}})"#,
        provider.base_url
    );
    let output = tokio::time::timeout(
        Duration::from_secs(90),
        configured_cli(&home, &workspace, &config)
            .args([
                "run",
                "--format",
                "jsonl",
                "Build and test this Node project offline.",
            ])
            .output(),
    )
    .await;
    worker.stop_spawned().await;
    let output = output.unwrap().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    println!(
        "Auto stdout: {stdout}\nAuto stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success());
    let events: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        !events
            .iter()
            .any(|event| event["envelope"]["event"]["type"] == "tool_approval_requested")
    );
    let calls: Vec<_> = events
        .iter()
        .filter(|event| event["envelope"]["event"]["type"] == "tool_call_finished")
        .collect();
    assert_eq!(calls.len(), 1);
    let call = &calls[0]["envelope"]["event"]["tool_call"];
    if call["is_error"] == true {
        let logs = data_directory(&home)
            .join("task-workspaces")
            .join(call["run_id"].as_str().unwrap())
            .join(".qq/task-home/.npm/_logs");
        tokio::task::spawn_blocking(move || {
            let entries = match std::fs::read_dir(&logs) {
                Ok(entries) => entries,
                Err(error) => {
                    println!("Private task npm logs unavailable: {error}");
                    return;
                }
            };
            for entry in entries.take(4).flatten() {
                if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                    continue;
                }
                if entry
                    .path()
                    .extension()
                    .is_none_or(|extension| extension != "log")
                {
                    continue;
                }
                let file = rustix::fs::open(
                    entry.path(),
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::NOFOLLOW
                        | rustix::fs::OFlags::CLOEXEC,
                    rustix::fs::Mode::empty(),
                )
                .map(std::fs::File::from);
                if let Ok(file) = file {
                    let mut bytes = Vec::new();
                    let mut bounded = std::io::Read::take(file, 16 * 1024);
                    if std::io::Read::read_to_end(&mut bounded, &mut bytes).is_ok() {
                        println!(
                            "Private task npm log {}: {}",
                            entry.file_name().to_string_lossy(),
                            String::from_utf8_lossy(&bytes)
                        );
                    }
                }
            }
        })
        .await
        .unwrap();
    }
    assert_eq!(call["is_error"], false, "{call}");
    assert!(
        call["result"].as_str().unwrap().contains("tested"),
        "{call}"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("built.txt")).unwrap(),
        "built"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("tested.txt")).unwrap(),
        "tested"
    );
    assert!(!workspace.join(".qq/task-home").exists());
    let task_os: Value =
        serde_json::from_slice(&std::fs::read(workspace.join("node-os-proof.json")).unwrap())
            .unwrap();
    assert_eq!(task_os, host_os);
    let probes: Vec<Value> =
        serde_json::from_slice(&std::fs::read(workspace.join("node-spawn-probe.json")).unwrap())
            .unwrap();
    assert_eq!(probes.len(), 5);
    for probe in probes {
        assert_eq!(probe["status"], 0, "{probe}");
        assert!(probe["signal"].is_null(), "{probe}");
        assert!(probe["error"].is_null(), "{probe}");
    }
    let captured = tokio::task::spawn_blocking(move || provider.capture_all())
        .await
        .unwrap();
    assert_eq!(captured.len(), 2);
}
