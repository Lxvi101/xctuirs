//! Discovery and typed command construction for Apple's public developer tools.
use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fmt, fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub name: PathBuf,
    pub root: PathBuf,
}
impl Project {
    pub fn find(dir: &Path) -> Result<Self> {
        let root = dir
            .canonicalize()
            .with_context(|| format!("cannot read {}", dir.display()))?;
        for pattern in [
            "*.xcworkspace",
            "*.xcodeproj",
            "*/*.xcworkspace",
            "*/*.xcodeproj",
        ] {
            for path in matching(&root, pattern)? {
                if path.is_dir()
                    && path
                        .parent()
                        .is_none_or(|p| p.extension().is_none_or(|x| x != "xcodeproj"))
                {
                    return Ok(Self { name: path, root });
                }
            }
        }
        bail!(
            "no .xcworkspace or .xcodeproj found in {} or one folder down",
            root.display()
        )
    }
    pub fn container_args(&self) -> Vec<String> {
        vec![
            if self.name.extension().is_some_and(|x| x == "xcworkspace") {
                "-workspace"
            } else {
                "-project"
            }
            .into(),
            self.name.to_string_lossy().into_owned(),
        ]
    }
}
impl fmt::Display for Project {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name.display())
    }
}

fn matching(root: &Path, pattern: &str) -> Result<Vec<PathBuf>> {
    let escaped = glob::Pattern::escape(&root.to_string_lossy());
    let mut paths = glob::glob(&format!("{escaped}/{pattern}"))?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scheme {
    pub name: String,
    pub package: bool,
}
impl fmt::Display for Scheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {}",
            if self.package { "package" } else { "scheme" },
            self.name
        )
    }
}

/// The container plus, for a workspace, the projects it references.
fn scheme_roots(project: &Project) -> Result<Vec<PathBuf>> {
    let mut roots = vec![project.name.clone()];
    if project.name.extension().is_some_and(|x| x == "xcworkspace")
        && let Some(parent) = project.name.parent()
    {
        match fs::read_to_string(project.name.join("contents.xcworkspacedata")) {
            Ok(text) => roots.extend(workspace_projects(&text, parent, &project.name)),
            // Unreadable workspace description: fall back to sibling projects.
            Err(_) => roots.extend(matching(parent, "*.xcodeproj")?),
        }
    }
    Ok(roots)
}
/// Project paths a workspace's contents.xcworkspacedata refers to. Only these
/// contribute schemes xcodebuild will accept for the workspace.
pub fn workspace_projects(text: &str, parent: &Path, workspace: &Path) -> Vec<PathBuf> {
    let re = Regex::new(r#"location\s*=\s*"(group|container|absolute|self):([^"]+\.xcodeproj)""#)
        .expect("valid regex");
    re.captures_iter(text)
        .map(|c| match &c[1] {
            "absolute" => PathBuf::from(&c[2]),
            "self" => workspace.join(&c[2]),
            _ => parent.join(&c[2]),
        })
        .filter(|p| p.is_dir())
        .collect()
}
/// The target the scheme's Run action launches, from its .xcscheme file.
pub fn runnable_target(project: &Project, scheme: &Scheme) -> Option<String> {
    let block = Regex::new(r"(?s)<LaunchAction\b.*?</LaunchAction>").ok()?;
    let name =
        Regex::new(r#"(?s)<BuildableProductRunnable\b.*?BlueprintName\s*=\s*"([^"]+)""#).ok()?;
    for root in scheme_roots(project).ok()? {
        for pattern in [
            format!(
                "xcshareddata/xcschemes/{}.xcscheme",
                glob::Pattern::escape(&scheme.name)
            ),
            format!(
                "xcuserdata/*/xcschemes/{}.xcscheme",
                glob::Pattern::escape(&scheme.name)
            ),
        ] {
            for path in matching(&root, &pattern).ok()? {
                let Ok(text) = fs::read_to_string(path) else {
                    continue;
                };
                if let Some(launch) = block.find(&text)
                    && let Some(c) = name.captures(launch.as_str())
                {
                    return Some(c[1].to_owned());
                }
            }
        }
    }
    None
}
pub fn scheme_files(project: &Project) -> Result<Vec<String>> {
    let roots = scheme_roots(project)?;
    let mut names = BTreeSet::new();
    for root in roots {
        for pattern in [
            "xcshareddata/xcschemes/*.xcscheme",
            "xcuserdata/*/xcschemes/*.xcscheme",
        ] {
            for path in matching(&root, pattern)? {
                if let Some(n) = path.file_stem() {
                    names.insert(n.to_string_lossy().into_owned());
                }
            }
        }
    }
    Ok(names.into_iter().collect())
}
pub fn parse_scheme_list(json: &str) -> Result<Vec<String>> {
    let value: Value = serde_json::from_str(json).context("invalid xcodebuild scheme JSON")?;
    let schemes = value
        .pointer("/workspace/schemes")
        .filter(|v| !v.is_null())
        .or_else(|| value.pointer("/project/schemes"));
    Ok(schemes
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect())
}
pub fn package_products(root: &Path) -> Result<Vec<String>> {
    if root.as_os_str().is_empty() {
        return Ok(vec![]);
    }
    let products = Regex::new(r#"\.(?:library|executable)\(\s*name:\s*"([^"]+)""#)?;
    let package = Regex::new(r#"Package\(\s*name:\s*"([^"]+)""#)?;
    let mut out = vec![];
    for pattern in ["Package.swift", "*/Package.swift", "*/*/Package.swift"] {
        for path in matching(root, pattern)? {
            if path
                .components()
                .any(|c| c.as_os_str() == ".build" || c.as_os_str() == "checkouts")
            {
                continue;
            }
            let Ok(text) = fs::read_to_string(path) else {
                continue;
            };
            let found = products
                .captures_iter(&text)
                .map(|c| c[1].to_owned())
                .collect::<Vec<_>>();
            if found.is_empty() {
                if let Some(c) = package.captures(&text) {
                    out.push(c[1].to_owned())
                }
            } else {
                out.extend(found)
            }
        }
    }
    Ok(out)
}
pub fn merge_schemes(listed: Vec<String>, products: Vec<String>) -> Vec<Scheme> {
    let mut out = vec![];
    for name in listed.iter().chain(products.iter()) {
        if !out.iter().any(|s: &Scheme| &s.name == name) {
            out.push(Scheme {
                name: name.clone(),
                package: products.contains(name),
            })
        }
    }
    out.sort_by_key(|s| s.package);
    out
}
pub fn schemes(project: &Project) -> Result<Vec<Scheme>> {
    let mut names = scheme_files(project)?;
    if names.is_empty() {
        let mut args = vec!["-list".into(), "-json".into()];
        args.extend(project.container_args());
        // Large workspaces (and first-time package resolution) list slowly.
        names = parse_scheme_list(&one_shot_with_timeout(
            "xcodebuild",
            &args,
            std::time::Duration::from_secs(90),
        )?)?;
    }
    Ok(merge_schemes(names, package_products(&project.root)?))
}
pub fn default_scheme(project: &Project, list: &[Scheme]) -> Option<usize> {
    let stem = project
        .name
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    list.iter()
        .position(|s| s.name == stem)
        .or_else(|| (!list.is_empty()).then_some(0))
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Destination {
    pub name: String,
    pub runtime: String,
    pub state: String,
    pub udid: String,
    pub device: bool,
}
impl Destination {
    pub fn platform(&self) -> String {
        match self.runtime.split_whitespace().next().unwrap_or("iOS") {
            "xrOS" => "visionOS",
            s => s,
        }
        .into()
    }
    pub fn is_mac(&self) -> bool {
        self.udid == "mac"
    }
    pub fn destination(&self) -> String {
        if self.is_mac() {
            "platform=macOS".into()
        } else {
            format!(
                "platform={}{}{}",
                self.platform(),
                if self.device {
                    ",id="
                } else {
                    " Simulator,id="
                },
                self.udid
            )
        }
    }
    pub fn platform_name(&self) -> String {
        if self.is_mac() {
            return "macosx".into();
        }
        match (self.platform().as_str(), self.device) {
            ("macOS", _) => "macosx",
            ("tvOS", false) => "appletvsimulator",
            ("tvOS", true) => "appletvos",
            ("watchOS", false) => "watchsimulator",
            ("watchOS", true) => "watchos",
            ("visionOS", false) => "xrsimulator",
            ("visionOS", true) => "xros",
            (_, true) => "iphoneos",
            (_, false) => "iphonesimulator",
        }
        .into()
    }
    pub fn label(&self) -> String {
        self.label_for_platform(false)
    }
    pub fn label_for_platform(&self, one_platform: bool) -> String {
        let prefix = if one_platform {
            format!("{} ", self.platform())
        } else {
            "iOS ".into()
        };
        let runtime = self.runtime.strip_prefix(&prefix).unwrap_or(&self.runtime);
        format!(
            "{} {}{}",
            self.name,
            runtime,
            if self.state == "Booted" { " ●" } else { "" }
        )
    }
}
pub fn runtime_name(id: &str) -> String {
    if let Some(tail) = id.strip_prefix("com.apple.CoreSimulator.SimRuntime.") {
        if let Some((platform, version)) = tail.split_once('-') {
            format!("{platform} {}", version.replace('-', "."))
        } else {
            tail.into()
        }
    } else {
        id.into()
    }
}
pub fn parse_simulators(json: &str) -> Result<Vec<Destination>> {
    let v: Value = serde_json::from_str(json).context("invalid simctl device JSON")?;
    let devices = v
        .get("devices")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("simctl output contains no devices"))?;
    let mut out = vec![];
    for (runtime, list) in devices {
        if let Some(list) = list.as_array() {
            for d in list {
                out.push(Destination {
                    name: field(d, "name"),
                    runtime: runtime_name(runtime),
                    state: field(d, "state"),
                    udid: field(d, "udid"),
                    device: false,
                });
            }
        }
    }
    sort_simulators(&mut out);
    Ok(out)
}
fn field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .into()
}
pub fn parse_devices(json: &str) -> Result<Vec<Destination>> {
    let v: Value = serde_json::from_str(json).context("invalid devicectl device JSON")?;
    Ok(v.pointer("/result/devices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|d| {
            d.pointer("/connectionProperties/pairingState")
                .and_then(Value::as_str)
                == Some("paired")
                && d.pointer("/connectionProperties/tunnelState")
                    .and_then(Value::as_str)
                    != Some("unavailable")
        })
        .map(|d| Destination {
            name: field(&d["deviceProperties"], "name"),
            runtime: format!(
                "{} {}",
                field(&d["hardwareProperties"], "platform"),
                field(&d["deviceProperties"], "osVersionNumber")
            ),
            state: String::new(),
            udid: field(&d["hardwareProperties"], "udid"),
            device: true,
        })
        .collect())
}
pub fn sort_simulators(list: &mut [Destination]) {
    fn platform_rank(d: &Destination) -> i32 {
        ["iOS", "tvOS", "watchOS", "visionOS", "macOS"]
            .iter()
            .position(|p| *p == d.platform())
            .map(|i| i as i32)
            .unwrap_or(-1)
    }
    fn version(d: &Destination) -> Vec<u64> {
        d.runtime
            .split_once(' ')
            .map(|(_, v)| v.split('.').map(|n| n.parse().unwrap_or(0)).collect())
            .unwrap_or_default()
    }
    list.sort_by(|a, b| {
        (b.state == "Booted")
            .cmp(&(a.state == "Booted"))
            .then_with(|| platform_rank(a).cmp(&platform_rank(b)))
            .then_with(|| {
                let av = version(a);
                let bv = version(b);
                (0..av.len().max(bv.len()))
                    .map(|i| bv.get(i).unwrap_or(&0).cmp(av.get(i).unwrap_or(&0)))
                    .find(|x| !x.is_eq())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
}
pub fn destinations() -> Result<Vec<Destination>> {
    let timeout = std::time::Duration::from_secs(45);
    // Simulators and devices come from separate services; either may be slow
    // to start. One failing must not hide the other.
    let (sims, devices) = std::thread::scope(|scope| {
        let sims = scope.spawn(|| {
            one_shot_with_timeout(
                "xcrun",
                &["simctl", "list", "devices", "available", "-j"].map(str::to_owned),
                timeout,
            )
            .and_then(|s| parse_simulators(&s))
        });
        let devices = scope.spawn(|| {
            one_shot_with_timeout(
                "xcrun",
                &["devicectl", "list", "devices", "--json-output", "-", "-q"].map(str::to_owned),
                timeout,
            )
            .and_then(|s| parse_devices(&s))
        });
        (
            sims.join()
                .unwrap_or_else(|_| Err(anyhow!("simulator listing panicked"))),
            devices
                .join()
                .unwrap_or_else(|_| Err(anyhow!("device listing panicked"))),
        )
    });
    let (sims, devices) = match (sims, devices) {
        (Err(e), Err(_)) => return Err(e),
        (s, d) => (s.unwrap_or_default(), d.unwrap_or_default()),
    };
    let mut out = vec![Destination {
        name: "My Mac".into(),
        runtime: "macOS".into(),
        udid: "mac".into(),
        ..Default::default()
    }];
    out.extend(devices);
    out.extend(sims);
    Ok(out)
}
pub fn default_destination(list: &[Destination]) -> Option<usize> {
    list.iter()
        .position(|d| !d.device && !d.is_mac() && d.state == "Booted" && d.platform() == "iOS")
        .or_else(|| {
            list.iter()
                .position(|d| !d.device && !d.is_mac() && d.name.starts_with("iPhone"))
        })
        .or_else(|| (!list.is_empty()).then_some(0))
}
pub fn destination_args(project: &Project, scheme: &Scheme, dest: &Destination) -> Vec<String> {
    let mut args = project.container_args();
    args.extend([
        "-scheme".into(),
        scheme.name.clone(),
        "-destination".into(),
        dest.destination(),
    ]);
    args
}
pub fn action_args(
    action: &str,
    project: &Project,
    scheme: &Scheme,
    dest: &Destination,
) -> Vec<String> {
    let mut args = vec![action.into()];
    args.extend(destination_args(project, scheme, dest));
    args
}
pub fn resolve_args(project: &Project) -> Vec<String> {
    let mut args = vec!["-resolvePackageDependencies".into()];
    args.extend(project.container_args());
    args
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildSettings {
    pub app_path: PathBuf,
    pub bundle_id: String,
    pub executable: PathBuf,
}
pub fn parse_build_settings(json: &str, platform: &str) -> Result<BuildSettings> {
    parse_build_settings_for(json, platform, None)
}
/// Like [`parse_build_settings`], preferring the target named `runnable`
/// (the scheme's Run action) when a scheme builds several apps.
pub fn parse_build_settings_for(
    json: &str,
    platform: &str,
    runnable: Option<&str>,
) -> Result<BuildSettings> {
    let mut targets: Vec<Value> =
        serde_json::from_str(json).context("invalid xcodebuild settings JSON")?;
    if let Some(name) = runnable {
        // Stable sort: the runnable target first, others keep their order.
        targets.sort_by_key(|t| t["target"].as_str() != Some(name));
    }
    for target in targets {
        let settings = &target["buildSettings"];
        if settings["WRAPPER_EXTENSION"].as_str() != Some("app")
            || settings["PLATFORM_NAME"].as_str() != Some(platform)
        {
            continue;
        }
        let required = |key: &str| -> Result<String> {
            settings[key]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("target {}: missing {key}", field(&target, "target")))
        };
        let dir = PathBuf::from(required("BUILT_PRODUCTS_DIR")?);
        let product = required("FULL_PRODUCT_NAME")?;
        let bundle_id = required("PRODUCT_BUNDLE_IDENTIFIER")?;
        let executable = required("EXECUTABLE_PATH")?;
        return Ok(BuildSettings {
            app_path: dir.join(product),
            bundle_id,
            executable: dir.join(executable),
        });
    }
    bail!("the scheme builds no {platform} app")
}
pub fn build_settings(
    project: &Project,
    scheme: &Scheme,
    dest: &Destination,
) -> Result<BuildSettings> {
    let mut args = vec!["-showBuildSettings".into(), "-json".into()];
    args.extend(destination_args(project, scheme, dest));
    parse_build_settings(&one_shot("xcodebuild", &args)?, &dest.platform_name())
}
pub fn one_shot(program: &str, args: &[String]) -> Result<String> {
    one_shot_with_timeout(program, args, std::time::Duration::from_secs(15))
}
fn one_shot_with_timeout(
    program: &str,
    args: &[String],
    timeout: std::time::Duration,
) -> Result<String> {
    use std::{
        io::Read,
        os::fd::AsRawFd,
        os::unix::process::CommandExt,
        process::Stdio,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
        time::Instant,
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .with_context(|| format!("could not run {program}"))?;
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    let mut handles = vec![];
    let read_stop = Arc::new(AtomicBool::new(false));
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            return Err(std::io::Error::last_os_error())
                .context("cannot configure bounded tool capture");
        }
    }
    for (index, mut reader) in [Box::new(stdout) as Box<dyn Read + Send>, Box::new(stderr)]
        .into_iter()
        .enumerate()
    {
        let tx = tx.clone();
        let read_stop = read_stop.clone();
        handles.push(thread::spawn(move || {
            let mut bytes = vec![];
            let result = (|| -> std::io::Result<Vec<u8>> {
                let mut buffer = [0u8; 8192];
                let mut oversized = false;
                while !read_stop.load(Ordering::SeqCst) {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            if bytes.len() + n <= 32 * 1024 * 1024 {
                                bytes.extend_from_slice(&buffer[..n]);
                            } else {
                                oversized = true;
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(std::time::Duration::from_millis(10))
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => (),
                        Err(error) => return Err(error),
                    }
                }
                if oversized {
                    return Err(std::io::Error::other("tool output exceeds 32 MiB"));
                }
                Ok(bytes)
            })();
            let _ = tx.send((index, result));
        }));
    }
    drop(tx);
    let start = Instant::now();
    let mut status = None;
    let mut streams = [None, None];
    let mut timed_out = false;
    loop {
        while let Ok((index, result)) = rx.try_recv() {
            streams[index] = Some(result);
        }
        if status.is_none() {
            status = child.try_wait()?;
        }
        if status.is_some() && streams.iter().all(Option::is_some) {
            break;
        }
        if start.elapsed() >= timeout && !timed_out {
            // Includes descendants holding the JSON pipes after the tool exits.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
            timed_out = true;
            read_stop.store(true, Ordering::SeqCst);
        }
        thread::sleep(std::time::Duration::from_millis(10));
    }
    for handle in handles {
        let _ = handle.join();
    }
    if timed_out {
        bail!(
            "{program}: timed out after {} seconds",
            timeout.as_secs_f64()
        );
    }
    let stdout = streams[0].take().unwrap()?;
    let stderr = streams[1].take().unwrap()?;
    let status = status.unwrap();
    if !status.success() {
        bail!(
            "{program}: {status}\n{}",
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

pub fn rename_simulator(udid: &str, name: &str) -> Result<()> {
    one_shot(
        "xcrun",
        &["simctl".into(), "rename".into(), udid.into(), name.into()],
    )
    .map(|_| ())
}

#[cfg(test)]
mod timeout_tests {
    use super::*;
    #[test]
    fn discovery_timeout_kills_descendant_that_holds_output_open() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("child.pid");
        let script = r#"import os,subprocess,sys,time
p=subprocess.Popen([sys.executable,'-c','import time;time.sleep(60)'])
with open(sys.argv[1],'w') as f:f.write(str(p.pid))
"#;
        let start = std::time::Instant::now();
        let error = one_shot_with_timeout(
            "/usr/bin/python3",
            &[
                "-c".into(),
                script.into(),
                marker.to_string_lossy().into_owned(),
            ],
            // Generous enough for python to start on a loaded machine.
            std::time::Duration::from_millis(2000),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(start.elapsed() < std::time::Duration::from_secs(6));
        let pid: i32 = fs::read_to_string(marker).unwrap().parse().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while unsafe { libc::kill(pid, 0) } == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "timed-out discovery left a child alive"
        );
    }
}

#[cfg(test)]
mod scheme_scope_tests {
    use super::*;
    #[test]
    fn workspace_schemes_come_from_referenced_projects_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for p in ["App.xcodeproj", "Old.xcodeproj", "Pods/Pods.xcodeproj"] {
            fs::create_dir_all(root.join(p).join("xcshareddata/xcschemes")).unwrap();
        }
        fs::write(root.join("App.xcodeproj/xcshareddata/xcschemes/App.xcscheme"), "<Scheme><LaunchAction><BuildableProductRunnable><BuildableReference BlueprintName = \"AppTarget\"/></BuildableProductRunnable></LaunchAction></Scheme>").unwrap();
        fs::write(
            root.join("Old.xcodeproj/xcshareddata/xcschemes/Old.xcscheme"),
            "",
        )
        .unwrap();
        fs::write(
            root.join("Pods/Pods.xcodeproj/xcshareddata/xcschemes/Pod.xcscheme"),
            "",
        )
        .unwrap();
        fs::create_dir_all(root.join("App.xcworkspace")).unwrap();
        fs::write(
            root.join("App.xcworkspace/contents.xcworkspacedata"),
            r#"<Workspace version = "1.0"><FileRef location = "group:App.xcodeproj"></FileRef><FileRef location = "group:Pods/Pods.xcodeproj"></FileRef></Workspace>"#,
        )
        .unwrap();
        let project = Project {
            name: root.join("App.xcworkspace"),
            root: root.to_owned(),
        };
        assert_eq!(scheme_files(&project).unwrap(), ["App", "Pod"]);
        let scheme = Scheme {
            name: "App".into(),
            package: false,
        };
        assert_eq!(
            runnable_target(&project, &scheme).as_deref(),
            Some("AppTarget")
        );
    }
    #[test]
    fn runnable_target_wins_over_json_order() {
        let json = r#"[
          {"target":"Helper","buildSettings":{"WRAPPER_EXTENSION":"app","PLATFORM_NAME":"iphoneos","BUILT_PRODUCTS_DIR":"/b","FULL_PRODUCT_NAME":"Helper.app","PRODUCT_BUNDLE_IDENTIFIER":"h","EXECUTABLE_PATH":"Helper.app/Helper"}},
          {"target":"Main","buildSettings":{"WRAPPER_EXTENSION":"app","PLATFORM_NAME":"iphoneos","BUILT_PRODUCTS_DIR":"/b","FULL_PRODUCT_NAME":"Main.app","PRODUCT_BUNDLE_IDENTIFIER":"m","EXECUTABLE_PATH":"Main.app/Main"}}
        ]"#;
        assert_eq!(
            parse_build_settings(json, "iphoneos").unwrap().bundle_id,
            "h"
        );
        assert_eq!(
            parse_build_settings_for(json, "iphoneos", Some("Main"))
                .unwrap()
                .bundle_id,
            "m"
        );
    }
}
