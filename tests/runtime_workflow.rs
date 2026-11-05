//! Workflow integration tests use fake Apple tools only, with PATH isolated to
//! each CLI subprocess. They never boot a simulator or access a real device.
use serde_json::Value;
use std::{
    fs,
    io::Read,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

const TOOLS: &str = r#"#!/usr/bin/python3
import os,sys,json,time,signal
name=os.path.basename(sys.argv[0]); a=sys.argv[1:]
row={'tool':name,'argv':a,'pid':os.getpid(),'env':{k:os.environ[k] for k in ['OS_ACTIVITY_DT_MODE','SIMCTL_CHILD_OS_ACTIVITY_DT_MODE','DEVICECTL_CHILD_OS_ACTIVITY_DT_MODE'] if k in os.environ}}
with open(os.environ['WORKFLOW_CALLS'],'a') as f: f.write(json.dumps(row)+'\n')
scenario=os.environ['WORKFLOW_CASE']
if name=='xcodebuild':
    if '-showBuildSettings' in a:
        platform='macosx' if scenario=='mac' else ('iphoneos' if scenario=='device' else 'iphonesimulator')
        print(json.dumps([{'target':'Puzzle','buildSettings':{'WRAPPER_EXTENSION':'app','PLATFORM_NAME':platform,'BUILT_PRODUCTS_DIR':os.environ['WORKFLOW_ROOT'],'FULL_PRODUCT_NAME':'Puzzle.app','PRODUCT_BUNDLE_IDENTIFIER':'com.example.Puzzle','EXECUTABLE_PATH':'mac_app' if scenario=='mac' else 'Puzzle.app/Puzzle'}}]))
    elif scenario=='hold-build':
        signal.signal(signal.SIGTERM,lambda *args:sys.exit(0))
        with open(os.path.join(os.environ['WORKFLOW_ROOT'],'build-started'),'w') as f:f.write(str(os.getpid()))
        while True:time.sleep(.05)
    else: print('BUILD SUCCEEDED',flush=True)
elif name=='xcrun' and a[:2]==['simctl','list']:
    print(json.dumps({'devices':{'com.apple.CoreSimulator.SimRuntime.iOS-26-0':[{'name':'iPhone fake','state':'Booted','udid':'SIM-FAKE'}]}}))
elif name=='xcrun' and a[:3]==['devicectl','list','devices']:
    print(json.dumps({'result':{'devices':[{'connectionProperties':{'pairingState':'paired','tunnelState':'connected'},'deviceProperties':{'name':'Phone fake','osVersionNumber':'26.0'},'hardwareProperties':{'platform':'iOS','udid':'DEVICE-FAKE'}}]}}))
elif name=='xcrun' and (a[:2]==['simctl','install'] or a[:4]==['devicectl','device','install','app']):
    if scenario=='cancel-install':
        with open(os.path.join(os.environ['WORKFLOW_ROOT'],'install-started'),'w') as f: f.write(str(os.getpid()))
        while True: time.sleep(.05)
    if scenario=='install-failure':
        print('INSTALL DENIED',file=sys.stderr,flush=True); sys.exit(17)
elif name=='mac_app' or (name=='xcrun' and (a[:2]==['simctl','launch'] or a[:4]==['devicectl','device','process','launch'])):
    print('APP CONSOLE LINE',flush=True)
    time.sleep(.15)
    sys.exit(7 if scenario=='app-nonzero' else 0)
elif name=='xcrun' and 'stream' in a:
    signal.signal(signal.SIGTERM,lambda *args:sys.exit(0))
    print('Filtering the log data',flush=True)
    while True: time.sleep(.05)
"#;

struct Sandbox {
    dir: tempfile::TempDir,
}
impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("bin")).unwrap();
        fs::create_dir_all(dir.path().join("home")).unwrap();
        fs::create_dir_all(dir.path().join("Puzzle.xcodeproj/xcshareddata/xcschemes")).unwrap();
        fs::write(
            dir.path()
                .join("Puzzle.xcodeproj/xcshareddata/xcschemes/Puzzle.xcscheme"),
            "",
        )
        .unwrap();
        for name in ["xcodebuild", "xcrun", "open"] {
            executable(&dir.path().join("bin").join(name));
        }
        executable(&dir.path().join("mac_app"));
        Self { dir }
    }
    fn root(&self) -> &Path {
        self.dir.path()
    }
    fn spawn(&self, scenario: &str, destination: &str) -> Child {
        Command::new(env!("CARGO_BIN_EXE_xctuirs"))
            .args(["run", "Puzzle", destination])
            .current_dir(self.root())
            .env("PATH", self.root().join("bin"))
            .env("HOME", self.root().join("home"))
            .env("WORKFLOW_CALLS", self.root().join("calls.jsonl"))
            .env("WORKFLOW_ROOT", self.root())
            .env("WORKFLOW_CASE", scenario)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }
    fn calls(&self) -> Vec<Value> {
        fs::read_to_string(self.root().join("calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
    fn finish(&self, mut child: Child) -> Output {
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                self.kill_survivors();
                let _ = child.kill();
                let _ = child.wait();
                panic!("fake workflow timed out");
            }
            thread::sleep(Duration::from_millis(10));
        };
        let mut stdout = vec![];
        let mut stderr = vec![];
        child
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut stdout)
            .unwrap();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_end(&mut stderr)
            .unwrap();
        let output = Output {
            status,
            stdout,
            stderr,
        };
        self.assert_reaped();
        output
    }
    fn assert_reaped(&self) {
        for call in self.calls() {
            let pid = call["pid"].as_i64().unwrap() as i32;
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                -1,
                "surviving fake process: {call}"
            );
        }
    }
    fn kill_survivors(&self) {
        for call in self.calls() {
            if let Some(pid) = call["pid"].as_i64() {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
        }
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        self.kill_survivors();
    }
}
fn executable(path: &Path) {
    fs::write(path, TOOLS).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
fn args(call: &Value) -> Vec<&str> {
    call["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect()
}
fn is_launch(call: &Value) -> bool {
    let a = args(call);
    a.starts_with(&["simctl", "launch"])
        || a.starts_with(&["devicectl", "device", "process", "launch"])
}

#[test]
fn physical_device_installs_launches_and_propagates_console_environment() {
    let s = Sandbox::new();
    let output = s.finish(s.spawn("device", "DEVICE-FAKE"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = s.calls();
    let install = calls
        .iter()
        .find(|c| args(c).starts_with(&["devicectl", "device", "install", "app"]))
        .unwrap();
    assert_eq!(
        args(install),
        [
            "devicectl",
            "device",
            "install",
            "app",
            "--device",
            "DEVICE-FAKE",
            &format!("{}/Puzzle.app", s.root().display())
        ]
    );
    let launch = calls.iter().find(|c| is_launch(c)).unwrap();
    assert_eq!(
        args(launch),
        [
            "devicectl",
            "device",
            "process",
            "launch",
            "--console",
            "--terminate-existing",
            "--device",
            "DEVICE-FAKE",
            "com.example.Puzzle"
        ]
    );
    assert_eq!(launch["env"]["DEVICECTL_CHILD_OS_ACTIVITY_DT_MODE"], "YES");
    assert!(
        !calls
            .iter()
            .any(|c| args(c).starts_with(&["simctl", "boot"])
                || args(c).contains(&"stream")
                || c["tool"] == "open")
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("APP CONSOLE LINE"));
}

#[test]
fn cancelling_install_kills_step_and_does_not_launch() {
    let s = Sandbox::new();
    let mut child = s.spawn("cancel-install", "SIM-FAKE");
    let deadline = Instant::now() + Duration::from_secs(8);
    while !s.root().join("install-started").exists() {
        if child.try_wait().unwrap().is_some() || Instant::now() >= deadline {
            s.kill_survivors();
            let _ = child.kill();
            let _ = child.wait();
            panic!("install never started");
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let output = s.finish(child);
    assert_eq!(output.status.code(), Some(130));
    assert!(!s.calls().iter().any(is_launch));
    assert!(String::from_utf8_lossy(&output.stderr).contains("stopped"));
}

#[test]
fn install_failure_keeps_diagnostic_and_skips_launch() {
    let s = Sandbox::new();
    let output = s.finish(s.spawn("install-failure", "SIM-FAKE"));
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stdout).contains("INSTALL DENIED"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("failed"));
    assert!(!s.calls().iter().any(is_launch));
}

#[test]
fn app_nonzero_exit_is_exited_and_unified_logger_is_reaped() {
    let s = Sandbox::new();
    let output = s.finish(s.spawn("app-nonzero", "SIM-FAKE"));
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("run exited"));
    let calls = s.calls();
    let launch = calls.iter().find(|c| is_launch(c)).unwrap();
    assert_eq!(launch["env"]["SIMCTL_CHILD_OS_ACTIVITY_DT_MODE"], "YES");
    assert!(calls.iter().any(|c| args(c).contains(&"stream")));
}

#[test]
fn mac_runs_executable_directly_with_console_environment() {
    let s = Sandbox::new();
    let output = s.finish(s.spawn("mac", "mac"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = s.calls();
    let app = calls.iter().find(|c| c["tool"] == "mac_app").unwrap();
    assert!(args(app).is_empty());
    assert_eq!(app["env"]["OS_ACTIVITY_DT_MODE"], "YES");
    assert!(
        !calls
            .iter()
            .any(|c| is_launch(c) || args(c).starts_with(&["simctl", "install"]))
    );
    assert!(
        calls
            .iter()
            .any(|c| args(c).starts_with(&["log", "stream"]))
    );
}

#[test]
fn separate_cli_instances_cannot_build_the_same_project_concurrently() {
    let s = Sandbox::new();
    let first = s.spawn("hold-build", "SIM-FAKE");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !s.root().join("build-started").exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(s.root().join("build-started").exists());
    let second = s.spawn("device", "DEVICE-FAKE").wait_with_output().unwrap();
    assert_eq!(second.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&second.stdout).contains("another xctuirs job"));
    let builds = s
        .calls()
        .iter()
        .filter(|call| call["tool"] == "xcodebuild" && !args(call).contains(&"-showBuildSettings"))
        .count();
    assert_eq!(
        builds, 1,
        "rejected instance launched a second compiler build"
    );
    let build = s
        .calls()
        .into_iter()
        .find(|call| call["tool"] == "xcodebuild")
        .unwrap();
    assert!(args(&build).ends_with(&["-jobs", "1"]));
    unsafe {
        libc::kill(first.id() as i32, libc::SIGINT);
    }
    assert_eq!(s.finish(first).status.code(), Some(130));
    assert!(
        s.finish(s.spawn("device", "DEVICE-FAKE")).status.success(),
        "project lock was not released after stop"
    );
}
