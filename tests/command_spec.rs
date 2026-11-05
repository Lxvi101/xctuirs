//! Executes upstream command fixtures against isolated fake Apple tools.
//! PATH is changed only in a child test process, never in the test runner.
use serde_json::Value as J;
use serde_yaml::Value as Y;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};
use xctuirs::{
    runtime::{self, Event},
    xcode::{self, Destination, Project, Scheme},
};
fn text(v: &Y) -> &str {
    v.as_str().unwrap_or("")
}
fn list(v: &Y) -> Vec<String> {
    v.as_sequence()
        .unwrap()
        .iter()
        .map(|v| text(v).into())
        .collect()
}
fn spec() -> Y {
    serde_yaml::from_str(include_str!("../spec/tests.yaml")).unwrap()
}
fn d(v: &Y) -> Destination {
    let mut d: Destination = serde_yaml::from_value(v.clone()).unwrap();
    if v["mac"].as_bool() == Some(true) {
        d.udid = "mac".into();
        d.runtime = "macOS".into()
    }
    d
}
const TOOL: &str = r#"#!/usr/bin/python3
import os, sys, json, time, signal
name=os.path.basename(sys.argv[0])
a=sys.argv[1:]
def record(**extra):
    row={'argv':[name]+a, 'env':[k+'='+os.environ[k] for k in ['OS_ACTIVITY_DT_MODE','SIMCTL_CHILD_OS_ACTIVITY_DT_MODE','DEVICECTL_CHILD_OS_ACTIVITY_DT_MODE'] if k in os.environ]}
    row.update(extra)
    with open(os.environ['COMMAND_RECORD'],'a') as f: f.write(json.dumps(row)+'\n')
record()
if name=='xcodebuild':
    if '-list' in a: print('{"project":{"schemes":["Puzzle"]}}')
    elif '-showBuildSettings' in a:
        print(json.dumps([{'target':'Puzzle','buildSettings':{'WRAPPER_EXTENSION':'app','PLATFORM_NAME':os.environ['TEST_PLATFORM'],'BUILT_PRODUCTS_DIR':'/dd','FULL_PRODUCT_NAME':'Puzzle.app','PRODUCT_BUNDLE_IDENTIFIER':'com.example.Puzzle','EXECUTABLE_PATH':os.environ['TEST_EXECUTABLE']}}]))
    elif os.environ.get('HOLD_BUILD')=='1':
        def stop(sig,frame):
            record(signal=signal.Signals(sig).name);sys.exit(0)
        signal.signal(signal.SIGTERM,stop)
        print('ready',flush=True)
        while True: time.sleep(.05)
elif name=='xcrun' and a[:2]==['simctl','list']:
    print('{"devices":{}}')
elif name=='xcrun' and a[:3]==['devicectl','list','devices']:
    print('{"result":{"devices":[]}}')
elif name=='xcrun' and a[:2]==['simctl','boot']:
    error=os.environ.get('BOOT_ERROR','')
    if error: print(error,file=sys.stderr);sys.exit(1)
elif name=='mac_app' or (name=='xcrun' and (a[:2]==['simctl','launch'] or a[:4]==['devicectl','device','process','launch'])):
    def stop(sig,frame):
        record(signal=signal.Signals(sig).name);sys.exit(0)
    signal.signal(signal.SIGINT,stop);signal.signal(signal.SIGTERM,stop)
    time.sleep(.2)
    print('ready',flush=True)
    while True: time.sleep(.05)
elif name=='xcrun' and 'stream' in a:
    signal.signal(signal.SIGTERM,lambda *args: sys.exit(0))
    while True: time.sleep(.05)
"#;
fn run_task(action: &str, project: Project, dest: Destination) {
    let task = runtime::start(
        action,
        project,
        Scheme {
            name: "Puzzle".into(),
            package: false,
        },
        dest,
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "tool workflow timed out");
        match task.events.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Console(line)) | Ok(Event::Line(line)) if line.contains("ready") => {
                task.cancel()
            }
            Ok(Event::Done { .. }) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("worker ended without Done")
            }
            _ => {}
        }
    }
    task.wait();
}
#[test]
fn command_fixture_child() {
    let Ok(case) = std::env::var("COMMAND_CASE") else {
        return;
    };
    let spec = spec();
    let c = &spec["commands"];
    let root = PathBuf::from(std::env::var("COMMAND_ROOT").unwrap());
    let project = Project {
        name: "/src/Puzzle.xcodeproj".into(),
        root: root.clone(),
    };
    let scheme = Scheme {
        name: "Puzzle".into(),
        package: false,
    };
    match case.as_str() {
        "list_schemes" => {
            xcode::schemes(&project).unwrap();
        }
        "list_simulators" | "list_devices" => {
            xcode::destinations().unwrap();
        }
        "rename" => {
            xcode::rename_simulator(text(&c["rename"]["udid"]), text(&c["rename"]["name"]))
                .unwrap();
        }
        "build_settings" => {
            xcode::build_settings(
                &project,
                &scheme,
                &Destination {
                    udid: "A".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        action if action.starts_with("action:") => run_task(
            &action[7..],
            project,
            Destination {
                udid: "A".into(),
                ..Default::default()
            },
        ),
        "install:0" | "install:1" | "install:2" | "install:3" => {
            let index = case.rsplit(':').next().unwrap().parse::<usize>().unwrap();
            let fixture = &c["install"][index];
            run_task(
                "run",
                project,
                Destination {
                    udid: fixture["udid"].as_str().unwrap_or("ABC-123").into(),
                    device: fixture["device"].as_bool().unwrap_or(false),
                    ..Default::default()
                },
            );
        }
        "launch" | "launch_device" => {
            run_task(
                "run",
                project,
                Destination {
                    udid: if case == "launch" {
                        "ABC-123"
                    } else {
                        "00008140-X"
                    }
                    .into(),
                    device: case == "launch_device",
                    ..Default::default()
                },
            );
        }
        "launch_mac" => run_task(
            "run",
            project,
            Destination {
                udid: "mac".into(),
                runtime: "macOS".into(),
                ..Default::default()
            },
        ),
        "log:0" => run_task(
            "run",
            project,
            Destination {
                udid: "ABC-123".into(),
                ..Default::default()
            },
        ),
        "log:1" => run_task(
            "run",
            project,
            Destination {
                udid: "mac".into(),
                runtime: "macOS".into(),
                ..Default::default()
            },
        ),
        "log:2" => run_task(
            "run",
            project,
            Destination {
                udid: "00008140-X".into(),
                device: true,
                ..Default::default()
            },
        ),
        _ => panic!("unknown command fixture {case}"),
    }
}
fn invoke(case: &str, platform: &str, executable: &str, boot_error: &str) -> Vec<J> {
    let tmp = tempfile::tempdir().unwrap();
    for name in ["xcodebuild", "xcrun", "open", "mac_app"] {
        let path = tmp.path().join(name);
        fs::write(&path, TOOL).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap()
    }
    let record = tmp.path().join("record.jsonl");
    let executable = if executable == "<mac>" {
        tmp.path().join("mac_app").to_string_lossy().into_owned()
    } else {
        executable.into()
    };
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "command_fixture_child", "--nocapture"])
        .env("COMMAND_CASE", case)
        .env("COMMAND_ROOT", tmp.path())
        .env("HOME", tmp.path())
        .env("COMMAND_RECORD", &record)
        .env("TEST_PLATFORM", platform)
        .env("TEST_EXECUTABLE", executable)
        .env(
            "HOLD_BUILD",
            if case.starts_with("action:") {
                "1"
            } else {
                "0"
            },
        )
        .env("BOOT_ERROR", boot_error)
        .env(
            "PATH",
            format!(
                "{}:{}",
                tmp.path().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{case}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    fs::read_to_string(record)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}
fn argv(row: &J) -> Vec<String> {
    row["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().into())
        .collect()
}
fn is_launch(row: &J) -> bool {
    let a = argv(row);
    a[0] == "mac_app"
        || a.get(2).is_some_and(|s| s == "launch")
        || a.get(4).is_some_and(|s| s == "launch")
}
#[test]
fn upstream_commands_23_cases() {
    let spec = spec();
    let c = &spec["commands"];
    let mut count = 0;
    let scheme = Scheme {
        name: "Puzzle".into(),
        package: false,
    };
    for fixture in c["xcodebuild_args"].as_sequence().unwrap() {
        let p = Project {
            name: text(&fixture["container"]).into(),
            root: PathBuf::new(),
        };
        assert_eq!(
            xcode::destination_args(&p, &scheme, &d(fixture)),
            list(&fixture["want"])
        );
        count += 1
    }
    for fixture in c["actions"].as_sequence().unwrap() {
        let case = format!("action:{}", text(&fixture["job"]));
        let rows = invoke(&case, "iphonesimulator", "Puzzle.app/Puzzle", "");
        let mut expected = list(&fixture["argv"]);
        expected.extend(["-jobs".into(), "1".into()]);
        assert_eq!(argv(&rows[0]), expected);
        assert_eq!(
            rows.last().unwrap()["signal"].as_str(),
            fixture["signal"].as_str()
        );
        count += 1
    }
    for name in [
        "list_schemes",
        "list_simulators",
        "list_devices",
        "rename",
        "build_settings",
    ] {
        let rows = invoke(name, "iphonesimulator", "Puzzle.app/Puzzle", "");
        let want = list(&c[name]["argv"]);
        assert!(rows.iter().any(|r| argv(r) == want), "{name}: {rows:?}");
        count += 1;
    }
    for (index, fixture) in c["install"].as_sequence().unwrap().iter().enumerate() {
        let rows = invoke(
            &format!("install:{index}"),
            if index == 3 {
                "iphoneos"
            } else {
                "iphonesimulator"
            },
            "Puzzle.app/Puzzle",
            text(&fixture["boot_error"]),
        );
        let got = rows
            .iter()
            .filter(|r| {
                let a = argv(r);
                a[0] == "open"
                    || a.get(2).is_some_and(|s| s == "boot" || s == "install")
                    || a.get(3).is_some_and(|s| s == "install")
            })
            .map(argv)
            .collect::<Vec<_>>();
        let want = fixture["calls"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(list)
            .collect::<Vec<_>>();
        assert_eq!(got, want, "install {index}");
        count += 1;
    }
    for name in ["launch", "launch_device", "launch_mac"] {
        let fixture = &c[name];
        let rows = invoke(
            name,
            if name == "launch_mac" {
                "macosx"
            } else if name == "launch_device" {
                "iphoneos"
            } else {
                "iphonesimulator"
            },
            if name == "launch_mac" {
                "<mac>"
            } else {
                "Puzzle.app/Puzzle"
            },
            "",
        );
        let row = rows
            .iter()
            .find(|r| is_launch(r))
            .expect("no launch command");
        let mut want = list(&fixture["argv"]);
        if name == "launch_mac" {
            want[0] = "mac_app".into()
        };
        assert_eq!(argv(row), want);
        assert_eq!(
            row["env"],
            serde_json::to_value(list(&fixture["env"])).unwrap()
        );
        let signal = rows
            .iter()
            .find(|r| is_launch(r) && r.get("signal").is_some())
            .expect("no graceful launch cancellation");
        assert_eq!(signal["signal"].as_str(), fixture["signal"].as_str());
        count += 1;
    }
    for (index, fixture) in c["log_stream"].as_sequence().unwrap().iter().enumerate() {
        // The fixture's app basename/executable shape determines the log predicate.
        let is_mac = index == 1;
        let rows = invoke(
            &format!("log:{index}"),
            if is_mac {
                "macosx"
            } else if index == 2 {
                "iphoneos"
            } else {
                "iphonesimulator"
            },
            if is_mac { "<mac>" } else { "Puzzle.app/Puzzle" },
            "",
        );
        let logs = rows
            .iter()
            .filter(|r| argv(r).iter().any(|a| a == "stream"))
            .collect::<Vec<_>>();
        if fixture["argv"].is_null() {
            assert!(logs.is_empty())
        } else {
            let mut want = list(&fixture["argv"]);
            if is_mac {
                let last = want.last_mut().unwrap();
                *last = last
                    .replace("ClipNinja", "mac_app")
                    .replace("/mac_app.app/", "/Puzzle.app/");
            }
            assert_eq!(logs.len(), 1);
            assert_eq!(argv(logs[0]), want)
        }
        count += 1;
    }
    assert_eq!(count, 23);
    println!("upstream commands: {count}/23 cases passed");
}
#[test]
fn upstream_log_9_cases() {
    let spec = spec();
    let mut count = 0;
    for c in spec["log"]["clean_line"].as_sequence().unwrap() {
        assert_eq!(
            runtime::clean_log_line(text(&c["line"])),
            if c["keep"].as_bool().unwrap() {
                Some(text(&c["want"]).into())
            } else {
                None
            }
        );
        count += 1
    }
    let timestamp = regex::Regex::new(r"^\d\d:\d\d:\d\d\.\d{3} ").unwrap();
    for c in spec["log"]["mirror_line"].as_sequence().unwrap() {
        let got = runtime::console_line(text(&c["line"]));
        if c["mirrored"].as_bool().unwrap() {
            assert_eq!(got, text(&c["want"]))
        } else {
            assert!(got.ends_with(&format!(" {}", text(&c["want"]))));
            assert!(timestamp.is_match(&got))
        }
        count += 1
    }
    assert_eq!(count, 9);
    println!("upstream logs: {count}/9 cases passed");
}
