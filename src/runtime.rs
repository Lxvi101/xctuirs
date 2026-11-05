//! Cancellable tool execution shared by the CLI and the terminal UI.
use crate::xcode::{self, Destination, Project, Scheme};
use anyhow::{Context, Result, bail};
use chrono::Local;
use regex::Regex;
use std::{
    fs::{self, File, OpenOptions},
    io::{BufReader, Read},
    os::fd::AsRawFd,
    os::unix::fs::OpenOptionsExt,
    os::unix::process::{CommandExt, ExitStatusExt},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Debug)]
pub enum Event {
    Line(String),
    Console(String),
    Attached(String),
    State(String),
    Done { code: i32, stopped: bool },
}

pub struct Task {
    pub events: Receiver<Event>,
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Task {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
    /// Drain before joining: a cancelled child can still write its final diagnostic.
    pub fn wait(mut self) {
        while self.events.recv().is_ok() {}
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for Task {
    fn drop(&mut self) {
        self.cancel();
        if let Some(worker) = self.worker.take() {
            while self.events.recv().is_ok() {}
            let _ = worker.join();
        }
    }
}

pub fn start(action: &str, project: Project, scheme: Scheme, destination: Destination) -> Task {
    // Unbounded: a slow consumer must never stall process supervision (and with
    // it cancellation). Every line is kept in memory by the consumer anyway.
    let (tx, events) = mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let stop = cancelled.clone();
    let action = action.to_owned();
    let worker = thread::spawn(move || {
        let result = workflow(&action, &project, &scheme, &destination, &stop, &tx);
        let stopped = stop.load(Ordering::SeqCst);
        let code = match result {
            Ok(code) => code,
            Err(error) => {
                if !stopped {
                    for line in error.to_string().lines() {
                        let _ = tx.send(Event::Line(format!("error: {line}")));
                    }
                }
                1
            }
        };
        let _ = tx.send(Event::Done {
            code: if stopped { 130 } else { code },
            stopped,
        });
    });
    Task {
        events,
        cancelled,
        worker: Some(worker),
    }
}

fn workflow(
    action: &str,
    project: &Project,
    scheme: &Scheme,
    destination: &Destination,
    stop: &Arc<AtomicBool>,
    tx: &Sender<Event>,
) -> Result<i32> {
    let lock = BuildLock::acquire(project)?;
    let free = ensure_build_space(&project.root, START_FREE_DISK)?;
    if free < LOW_FREE_DISK {
        let _ = tx.send(Event::Line(format!(
            "warning: only {:.1} GiB of disk space free; large builds may fail",
            gib(free)
        )));
    }
    let mut args = if action == "resolve" {
        xcode::resolve_args(project)
    } else {
        xcode::action_args(
            if action == "run" { "build" } else { action },
            project,
            scheme,
            destination,
        )
    };
    args.extend(extra_build_args(action, destination));
    let _ = tx.send(Event::State(
        if action == "run" { "building" } else { action }.into(),
    ));
    let mut command = Command::new("xcodebuild");
    command
        .args(args)
        // Line-by-line output keeps the progress bar and log live.
        .env("NSUnbufferedIO", "YES")
        .current_dir(&project.root);
    let process = Process::spawn(command)?;
    let code = process.finish(stop, libc::SIGTERM, |line| {
        let _ = tx.send(Event::Line(line));
    })?;
    if stop.load(Ordering::SeqCst) {
        return Ok(130);
    }
    if code != 0 {
        let _ = tx.send(Event::Line(format!("error: exit status {code}")));
        return Ok(code);
    }
    if action != "run" {
        return Ok(0);
    }
    let _ = tx.send(Event::State("installing".into()));
    let _ = tx.send(Event::Line(format!(
        "▸ Installing and launching on {}",
        destination.name
    )));
    let stream = |program: &str, args: &[String]| -> Result<()> {
        if stop.load(Ordering::SeqCst) {
            bail!("stopped");
        }
        let mut command = Command::new(program);
        command.args(args).current_dir(&project.root);
        let mut tail: Vec<String> = vec![];
        let code = Process::spawn(command)?.finish(stop, libc::SIGTERM, |line| {
            if tail.len() == 20 {
                tail.remove(0);
            }
            tail.push(line.clone());
            let _ = tx.send(Event::Line(line));
        })?;
        if stop.load(Ordering::SeqCst) {
            bail!("stopped");
        }
        if code != 0 {
            bail!(
                "{program} {}: exit status {code}: {}",
                args.join(" "),
                tail.join("\n").trim()
            );
        }
        Ok(())
    };
    let mut settings_args = vec!["-showBuildSettings".to_owned(), "-json".to_owned()];
    settings_args.extend(project.container_args());
    settings_args.extend([
        "-scheme".into(),
        scheme.name.clone(),
        "-destination".into(),
        destination.destination(),
    ]);
    let json = capture("xcodebuild", &settings_args, stop)?;
    // The scheme's Run action names the app to launch; without it, the first
    // app for the platform is used.
    let runnable = xcode::runnable_target(project, scheme);
    let settings =
        xcode::parse_build_settings_for(&json, &destination.platform_name(), runnable.as_deref())?;
    let app_path = settings.app_path.to_string_lossy().into_owned();
    if destination.device {
        stream(
            "xcrun",
            &strings(&[
                "devicectl",
                "device",
                "install",
                "app",
                "--device",
                &destination.udid,
                &app_path,
            ]),
        )?;
    } else if !destination.is_mac() {
        let boot = capture(
            "xcrun",
            &strings(&["simctl", "boot", &destination.udid]),
            stop,
        );
        if let Err(error) = boot
            && !error.to_string().contains("current state: Booted")
        {
            return Err(error);
        }
        capture("open", &strings(&["-a", "Simulator"]), stop)?;
        stream(
            "xcrun",
            &strings(&["simctl", "install", &destination.udid, &app_path]),
        )?;
    }
    if stop.load(Ordering::SeqCst) {
        return Ok(130);
    }
    let _ = tx.send(Event::State("launching".into()));
    let mut launch = if destination.is_mac() {
        let mut cmd = Command::new(&settings.executable);
        cmd.env("OS_ACTIVITY_DT_MODE", "YES");
        cmd
    } else {
        let mut cmd = Command::new("xcrun");
        if destination.device {
            cmd.args([
                "devicectl",
                "device",
                "process",
                "launch",
                "--console",
                "--terminate-existing",
                "--device",
                &destination.udid,
                &settings.bundle_id,
            ]);
            cmd.env("DEVICECTL_CHILD_OS_ACTIVITY_DT_MODE", "YES");
        } else {
            cmd.args([
                "simctl",
                "launch",
                "--console-pty",
                "--terminate-running-process",
                &destination.udid,
                &settings.bundle_id,
            ]);
            cmd.env("SIMCTL_CHILD_OS_ACTIVITY_DT_MODE", "YES");
        }
        cmd
    };
    launch.current_dir(&project.root);
    // Building and installing are done; another session may build while the app runs.
    drop(lock);
    let process = Process::spawn(launch)?;
    // simctl and devicectl report a successful launch before any app output.
    // Attach only then, so a refused launch (locked phone, missing app) is a
    // failure rather than an app that "exited".
    let mut attached = false;
    let attach = |attached: &mut bool| {
        if !*attached {
            *attached = true;
            let _ = tx.send(Event::Attached(settings.bundle_id.clone()));
            let _ = tx.send(Event::State("attached".into()));
        }
    };
    if destination.is_mac() {
        attach(&mut attached);
    }
    let launch_started = Instant::now();
    let bundle_id = settings.bundle_id.clone();
    let log_stop = Arc::new(AtomicBool::new(false));
    let log_worker = if !destination.device {
        let mut command = Command::new("xcrun");
        if !destination.is_mac() {
            command.args(["simctl", "spawn", &destination.udid]);
        }
        let escape = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let executable = settings
            .executable
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let app_name = settings
            .app_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let predicate = format!(
            "process == \"{}\" AND senderImagePath CONTAINS \"/{}/\" AND messageType == debug AND NOT subsystem BEGINSWITH \"com.apple.Previews\"",
            escape(&executable),
            escape(&app_name)
        );
        command.args([
            "log",
            "stream",
            "--style",
            "compact",
            "--level",
            "debug",
            "--predicate",
            &predicate,
        ]);
        match Process::spawn(command) {
            Ok(log_process) => {
                let log_stop = log_stop.clone();
                let tx = tx.clone();
                Some(thread::spawn(move || {
                    let _ = log_process.finish(&log_stop, libc::SIGTERM, |line| {
                        if let Some(line) = clean_log_line(&line) {
                            let _ = tx.send(Event::Console(line));
                        }
                    });
                }))
            }
            Err(_) => None,
        }
    } else {
        None
    };
    let result = process.finish(
        stop,
        if destination.is_mac() {
            libc::SIGTERM
        } else {
            libc::SIGINT
        },
        |line| {
            if !attached {
                if launch_marker(&line, &bundle_id)
                    || launch_started.elapsed() > Duration::from_secs(3)
                {
                    attach(&mut attached);
                } else {
                    // Tool preamble (device tunnel, disk image services).
                    let _ = tx.send(Event::Line(line));
                    return;
                }
            }
            let _ = tx.send(Event::Console(console_line(&line)));
        },
    );
    log_stop.store(true, Ordering::SeqCst);
    if let Some(worker) = log_worker {
        let _ = worker.join();
    }
    let code = result?;
    if !attached && !stop.load(Ordering::SeqCst) {
        if code != 0 {
            bail!(
                "couldn't launch {bundle_id} on {} (exit status {code})",
                destination.name
            );
        }
        attach(&mut attached);
    }
    Ok(0) // app exit status is a console event, not a failed build.
}

/// Flags added to every build-like xcodebuild invocation.
///
/// * `-jobs N` bounds parallel compiler processes by installed memory (about
///   one per 3 GiB, at least 2, at most the CPU count). `XCTUIRS_JOBS=N`
///   overrides it; `XCTUIRS_JOBS=0` leaves parallelism to xcodebuild.
/// * `-hideShellScriptEnvironment` drops the multi-kilobyte environment dump
///   printed for every script phase.
/// * `-allowProvisioningUpdates` lets device builds create or refresh signing
///   profiles the way Xcode does, using the accounts already added to Xcode.
pub fn extra_build_args(action: &str, destination: &Destination) -> Vec<String> {
    if action == "resolve" {
        return vec![];
    }
    let mut args = vec![];
    if let Some(jobs) = parallel_jobs() {
        args.extend(["-jobs".to_owned(), jobs.to_string()]);
    }
    if action != "clean" {
        args.push("-hideShellScriptEnvironment".to_owned());
        if destination.device {
            args.push("-allowProvisioningUpdates".to_owned());
        }
    }
    args
}
pub fn parallel_jobs() -> Option<usize> {
    if let Some(value) = std::env::var_os("XCTUIRS_JOBS")
        && let Some(n) = value.to_str().and_then(|v| v.trim().parse::<usize>().ok())
    {
        return (n > 0).then_some(n);
    }
    let cores = thread::available_parallelism().map_or(4, |n| n.get());
    let memory = physical_memory().unwrap_or(8 << 30);
    Some(((memory >> 30) as usize / 3).clamp(2, cores.max(2)))
}
fn physical_memory() -> Option<u64> {
    let mut value: u64 = 0;
    let mut size = std::mem::size_of::<u64>();
    let name = c"hw.memsize";
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut value as *mut u64).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && value > 0).then_some(value)
}

/// The first line simctl (`com.example.App: 4242`) or devicectl
/// (`Launched application with com.example.App bundle identifier.`) prints
/// once the app process exists.
fn launch_marker(line: &str, bundle_id: &str) -> bool {
    line.contains("Launched application")
        || line
            .strip_prefix(bundle_id)
            .and_then(|rest| rest.strip_prefix(": "))
            .is_some_and(|pid| !pid.is_empty() && pid.trim().chars().all(|c| c.is_ascii_digit()))
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).into()).collect()
}
fn capture(name: &str, args: &[String], stop: &Arc<AtomicBool>) -> Result<String> {
    if stop.load(Ordering::SeqCst) {
        bail!("stopped");
    }
    // One-shot JSON commands require stdout alone. Diagnostics on stderr must
    // never be prepended to a successful xcodebuild JSON document.
    let mut command = Command::new(name);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("couldn't start {name}"))?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let err = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let status = loop {
        if stop.load(Ordering::SeqCst) {
            signal_group(child.id(), libc::SIGKILL);
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        thread::sleep(Duration::from_millis(20));
    };
    // A descendant that inherited the pipes can keep them open after the
    // tool exits. Give output a moment to drain, then end the whole group.
    let drained = Instant::now();
    while !(out.is_finished() && err.is_finished()) {
        if stop.load(Ordering::SeqCst) || drained.elapsed() > Duration::from_secs(5) {
            signal_group(child.id(), libc::SIGKILL);
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let stdout = out
        .join()
        .map_err(|_| anyhow::anyhow!("stdout reader panicked"))??;
    let stderr = err
        .join()
        .map_err(|_| anyhow::anyhow!("stderr reader panicked"))??;
    if stop.load(Ordering::SeqCst) {
        bail!("stopped");
    }
    if !status.success() {
        let code = status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1));
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
        bail!(
            "{} {}: exit status {}: {}",
            name,
            args.join(" "),
            code,
            output.trim()
        );
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

const GIB: u64 = 1024 * 1024 * 1024;
/// Refuse to start below this: there is no room for build products or swap.
const START_FREE_DISK: u64 = 3 * GIB;
/// Warn (but build) below this.
const LOW_FREE_DISK: u64 = 10 * GIB;
/// Stop a running build below this, before the startup disk is completely full.
const MIN_FREE_DISK: u64 = GIB;
fn gib(bytes: u64) -> f64 {
    bytes as f64 / GIB as f64
}
/// Returns the smallest free space among the volumes a build writes to.
fn ensure_build_space(project_root: &std::path::Path, minimum: u64) -> Result<u64> {
    let mut free = ensure_disk_space(project_root, minimum)?;
    // DerivedData and macOS swap normally live on the home/startup volume,
    // which can differ from a project on an external drive.
    if let Some(home) = std::env::var_os("HOME") {
        let home = std::path::PathBuf::from(home);
        if home != project_root {
            free = free.min(ensure_disk_space(&home, minimum)?);
        }
    }
    Ok(free)
}
fn ensure_disk_space(path: &std::path::Path, minimum: u64) -> Result<u64> {
    let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("cannot check free disk space before building");
    }
    let stats = unsafe { stats.assume_init() };
    let available = (stats.f_bavail as u64).saturating_mul(stats.f_frsize);
    validate_available_disk(available, minimum)?;
    Ok(available)
}
fn validate_available_disk(available: u64, minimum: u64) -> Result<()> {
    if available < minimum {
        bail!(
            "build stopped: {:.1} GiB free; xctuirs requires at least {} GiB of free disk space for build output and macOS swap",
            gib(available),
            minimum / GIB
        );
    }
    Ok(())
}

// Lock files are never removed: unlinking an in-use flock file permits a second
// inode (and therefore a second build) to acquire the same apparent lock path.
struct BuildLock(File);
impl BuildLock {
    fn acquire(project: &Project) -> Result<Self> {
        use sha1::{Digest, Sha1};
        let home = std::env::var_os("HOME").context("HOME is required for the build lock")?;
        let dir = std::path::PathBuf::from(home).join("Library/Application Support/xctui/locks");
        fs::create_dir_all(&dir)?;
        let container = project
            .name
            .canonicalize()
            .unwrap_or_else(|_| project.name.clone());
        let digest = Sha1::digest(container.as_os_str().as_encoded_bytes());
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join(format!("{digest:x}.lock")))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
                bail!(
                    "another xctuirs job is already using {}; stop it before starting another build",
                    project.name.display()
                );
            }
            return Err(error).context("cannot lock the Xcode project");
        }
        Ok(Self(file))
    }
}
impl Drop for BuildLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
fn signal_group(pid: u32, signal: i32) {
    // The process was spawned with process_group(0), so its group is ours alone.
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}
fn group_exists(pid: u32) -> bool {
    unsafe { libc::kill(-(pid as i32), 0) == 0 }
}

struct Process {
    child: Child,
    lines: Receiver<String>,
    reader: JoinHandle<()>,
    disk_path: Option<std::path::PathBuf>,
}
impl Process {
    fn spawn(mut command: Command) -> Result<Self> {
        let disk_path = if std::path::Path::new(command.get_program())
            .file_name()
            .is_some_and(|name| name == "xcodebuild")
        {
            Some(
                command
                    .get_current_dir()
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or(std::env::current_dir()?),
            )
        } else {
            None
        };
        let (reader, writer) = os_pipe::pipe()?;
        command
            .stdout(Stdio::from(writer.try_clone()?))
            .stderr(Stdio::from(writer))
            .stdin(Stdio::null())
            .process_group(0);
        let description = format!("{command:?}");
        let child = command
            .spawn()
            .with_context(|| format!("couldn't start {description}"))?;
        // The Command owns the writing handles too, so drop it before reading EOF.
        drop(command);
        let (tx, lines) = mpsc::sync_channel(256);
        let reader = thread::spawn(move || {
            scan(reader, |line| {
                let _ = tx.send(line);
            })
        });
        Ok(Self {
            child,
            lines,
            reader,
            disk_path,
        })
    }
    fn finish(
        mut self,
        stop: &Arc<AtomicBool>,
        signal: i32,
        mut emit: impl FnMut(String),
    ) -> Result<i32> {
        let pid = self.child.id();
        let mut sent: Option<Instant> = None;
        let mut killed = false;
        let mut status: Option<ExitStatus> = None;
        let mut closed = false;
        let mut disk_checked = Instant::now();
        loop {
            if !stop.load(Ordering::SeqCst) && disk_checked.elapsed() >= Duration::from_secs(1) {
                if let Some(path) = &self.disk_path
                    && let Err(error) = ensure_build_space(path, MIN_FREE_DISK).map(|_| ())
                {
                    emit(format!("error: {error}"));
                    stop.store(true, Ordering::SeqCst);
                }
                disk_checked = Instant::now();
            }
            if stop.load(Ordering::SeqCst) {
                let at = *sent.get_or_insert_with(|| {
                    // Give xcodebuild/simctl their own graceful signal first.
                    // Unlike the previous implementation, cleanup continues even
                    // after the parent exits: compiler descendants can outlive it.
                    unsafe {
                        libc::kill(pid as i32, signal);
                    }
                    Instant::now()
                });
                if !killed && at.elapsed() >= Duration::from_secs(5) {
                    signal_group(pid, libc::SIGKILL);
                    killed = true;
                }
            }
            if !closed {
                match self.lines.recv_timeout(Duration::from_millis(20)) {
                    Ok(line) => emit(line),
                    Err(RecvTimeoutError::Disconnected) => closed = true,
                    Err(RecvTimeoutError::Timeout) => {}
                }
            } else {
                thread::sleep(Duration::from_millis(10));
            }
            if status.is_none() {
                status = self.child.try_wait()?;
            }
            if closed && status.is_some() {
                if sent.is_none() || !group_exists(pid) {
                    break;
                }
                // A killed zombie can remain in its old group until launchd
                // reaps it. It cannot compile or retain output handles.
                if killed && sent.unwrap().elapsed() >= Duration::from_secs(7) {
                    break;
                }
            }
        }
        let _ = self.reader.join();
        let status = status.unwrap();
        Ok(status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)))
    }
}

const MAX_LINE: usize = 4 * 1024 * 1024;
fn scan(reader: impl Read, mut emit: impl FnMut(String)) {
    let mut reader = BufReader::new(reader);
    let mut buffer = [0; 8192];
    let mut line = Vec::new();
    let mut discard = false;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                for &byte in &buffer[..n] {
                    if discard {
                        // Skip the rest of the oversized line, then carry on.
                        discard = byte != b'\n';
                        continue;
                    }
                    if byte == b'\n' {
                        if line.last() == Some(&b'\r') {
                            line.pop();
                        }
                        emit(String::from_utf8_lossy(&line).into_owned());
                        line.clear();
                    } else if line.len() >= MAX_LINE {
                        emit("note: a line over 4 MiB was left out".into());
                        discard = true;
                        line.clear();
                    } else {
                        line.push(byte);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                emit(format!("scanner error: {error}"));
                return;
            }
        }
    }
    if !discard && !line.is_empty() {
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        emit(String::from_utf8_lossy(&line).into_owned());
    }
}

pub fn clean_log_line(line: &str) -> Option<String> {
    if [
        "Filtering the log data",
        "Timestamp ",
        "getpwuid_r did not find",
    ]
    .iter()
    .any(|p| line.starts_with(p))
    {
        return None;
    }
    static PREFIX: OnceLock<Regex> = OnceLock::new();
    let re = PREFIX.get_or_init(|| {
        Regex::new(r"^\d{4}-\d\d-\d\d (\d\d:\d\d:\d\d\.\d+) \S+\s+\S+\[[^\]]*\] ").unwrap()
    });
    Some(re.replace(line, "$1 ").into_owned())
}
pub fn console_line(line: &str) -> String {
    static PREFIX: OnceLock<Regex> = OnceLock::new();
    let re = PREFIX.get_or_init(|| {
        Regex::new(r"^\d{4}-\d\d-\d\d (\d\d:\d\d:\d\d\.\d{3})\d*[+-]\d{4} .+?\[\d+:\d+\] ").unwrap()
    });
    if re.is_match(line) {
        re.replace(line, "$1 ").into_owned()
    } else {
        format!("{} {line}", Local::now().format("%H:%M:%S%.3f"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn upstream_stream_seven_cases() {
        let spec: serde_yaml::Value =
            serde_yaml::from_str(include_str!("../spec/tests.yaml")).unwrap();
        let stream = &spec["stream"];
        let mut count = 0;
        for case in stream["scan_lines"].as_sequence().unwrap() {
            let input = case["input"]
                .as_str()
                .unwrap()
                .replace("<100000 x characters>", &"x".repeat(100_000));
            let expected: Vec<String> = case["want"]
                .as_sequence()
                .unwrap()
                .iter()
                .map(|x| {
                    x.as_str()
                        .unwrap()
                        .replace("<the 100000 x characters>", &"x".repeat(100_000))
                })
                .collect();
            let mut lines = vec![];
            scan(input.as_bytes(), |l| lines.push(l));
            assert_eq!(lines, expected);
            count += 1;
        }
        let case = &stream["exit_code"];
        let mut command = Command::new("/bin/sh");
        command.args(["-c", case["script"].as_str().unwrap()]);
        let process = Process::spawn(command).unwrap();
        let mut lines = vec![];
        let code = process
            .finish(&Arc::new(AtomicBool::new(false)), libc::SIGTERM, |l| {
                lines.push(l)
            })
            .unwrap();
        let expected: Vec<&str> = case["lines"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert_eq!(lines, expected);
        assert_eq!(
            format!("exit status {code}"),
            case["wait"].as_str().unwrap()
        );
        count += 1;
        for case in stream["cancel"].as_sequence().unwrap() {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", case["script"].as_str().unwrap()]);
            let process = Process::spawn(command).unwrap();
            let cancelled = Arc::new(AtomicBool::new(false));
            let cancel = cancelled.clone();
            let mut after = vec![];
            let began = Instant::now();
            let signal = if case["signal"].as_str().unwrap() == "SIGINT" {
                libc::SIGINT
            } else {
                libc::SIGTERM
            };
            process
                .finish(&cancelled, signal, |line| {
                    if line == "ready" {
                        cancel.store(true, Ordering::SeqCst);
                    } else {
                        after.push(line);
                    }
                })
                .unwrap();
            let expected: Vec<&str> = case["after_ready_and_cancel"]
                .as_sequence()
                .unwrap()
                .iter()
                .map(|x| x.as_str().unwrap())
                .collect();
            assert_eq!(after, expected);
            assert!(began.elapsed() < Duration::from_secs(5));
            count += 1;
        }
        let case = &stream["one_shot_error"];
        let error = capture(
            "/bin/sh",
            &strings(&["-c", case["script"].as_str().unwrap()]),
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap_err()
        .to_string();
        for expected in case["error_contains"].as_sequence().unwrap() {
            assert!(error.contains(expected.as_str().unwrap()), "{error}");
        }
        count += 1;
        assert_eq!(count, 7);
        println!("upstream stream: {count}/7 fixture cases passed");
    }
    #[test]
    fn scan_crlf_and_final_fragment() {
        let mut lines = vec![];
        scan("first\r\n\nlast\r".as_bytes(), |l| lines.push(l));
        assert_eq!(lines, ["first", "", "last"]);
    }
    #[test]
    fn oversized_line_drains() {
        let mut input = vec![b'x'; MAX_LINE + 1];
        input.extend_from_slice(b"\nafter\n");
        let mut lines = vec![];
        scan(&input[..], |l| lines.push(l));
        assert_eq!(lines, ["note: a line over 4 MiB was left out", "after"]);
    }
    #[test]
    fn merged_pipe_preserves_write_order_and_exit_code() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "printf 'one\\n'; printf 'two\\n' >&2; printf 'three\\n'; exit 65",
        ]);
        let process = Process::spawn(command).unwrap();
        let mut lines = vec![];
        let code = process
            .finish(&Arc::new(AtomicBool::new(false)), libc::SIGTERM, |l| {
                lines.push(l)
            })
            .unwrap();
        assert_eq!(code, 65);
        assert_eq!(lines, ["one", "two", "three"]);
    }
    #[test]
    fn graceful_stop_drains_final_diagnostic() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "trap 'echo stopped; exit 0' TERM; echo ready; while :; do sleep 0.05; done",
        ]);
        let process = Process::spawn(command).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = cancelled.clone();
        let mut lines = vec![];
        process
            .finish(&cancelled, libc::SIGTERM, |line| {
                if line == "ready" {
                    cancel.store(true, Ordering::SeqCst);
                }
                lines.push(line);
            })
            .unwrap();
        assert_eq!(lines, ["ready", "stopped"]);
    }
    #[test]
    fn cancelled_parent_cannot_leave_compiler_descendant_running() {
        for inherit_stdout in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let pid_file = temp.path().join("compiler.pid");
            let child_script =
                "import signal,time;signal.signal(signal.SIGTERM,signal.SIG_IGN);time.sleep(60)";
            let parent_script = r#"
import os,signal,subprocess,sys,time
signal.signal(signal.SIGTERM,lambda *args:sys.exit(0))
output={} if sys.argv[2]=='true' else {'stdout':subprocess.DEVNULL,'stderr':subprocess.DEVNULL}
p=subprocess.Popen([sys.executable,'-c',sys.argv[3]],**output)
with open(sys.argv[1],'w') as f:f.write(str(p.pid))
time.sleep(.1)
print('ready',flush=True)
while True:time.sleep(.05)
"#;
            let mut command = Command::new("/usr/bin/python3");
            command.args([
                "-c",
                parent_script,
                pid_file.to_str().unwrap(),
                if inherit_stdout { "true" } else { "false" },
                child_script,
            ]);
            let process = Process::spawn(command).unwrap();
            let cancelled = Arc::new(AtomicBool::new(false));
            let began = Instant::now();
            process
                .finish(&cancelled, libc::SIGTERM, |line| {
                    if line == "ready" {
                        cancelled.store(true, Ordering::SeqCst);
                    }
                })
                .unwrap();
            assert!(began.elapsed() < Duration::from_secs(10));
            let pid: i32 = fs::read_to_string(pid_file).unwrap().parse().unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            assert_ne!(
                unsafe { libc::kill(pid, 0) },
                0,
                "compiler descendant {pid} survived cancellation"
            );
        }
    }
    #[test]
    fn disk_reserve_refuses_low_space_without_starting_a_tool() {
        assert!(
            validate_available_disk(MIN_FREE_DISK - 1, MIN_FREE_DISK)
                .unwrap_err()
                .to_string()
                .contains("at least 1 GiB")
        );
        assert!(validate_available_disk(MIN_FREE_DISK, MIN_FREE_DISK).is_ok());
    }
    #[test]
    fn project_lock_rejects_second_holder_and_releases_on_drop() {
        let temp = tempfile::tempdir().unwrap();
        let project = Project {
            name: temp.path().join("Unique.xcodeproj"),
            root: temp.path().to_owned(),
        };
        let first = BuildLock::acquire(&project).unwrap();
        assert!(
            BuildLock::acquire(&project)
                .err()
                .unwrap()
                .to_string()
                .contains("another xctuirs job")
        );
        drop(first);
        let _next = BuildLock::acquire(&project).unwrap();
    }
    #[test]
    fn launch_markers() {
        assert!(launch_marker("com.example.App: 4242", "com.example.App"));
        assert!(launch_marker(
            "Launched application with com.example.App bundle identifier.",
            "com.example.App"
        ));
        assert!(!launch_marker("com.example.App: hello", "com.example.App"));
        assert!(!launch_marker(
            "Acquired tunnel connection to device.",
            "com.example.App"
        ));
    }
    #[test]
    fn capture_returns_when_a_descendant_keeps_the_pipe_open() {
        let began = Instant::now();
        let out = capture(
            "/bin/sh",
            &strings(&["-c", "echo done; (sleep 30) & exit 0"]),
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert_eq!(out.trim(), "done");
        assert!(began.elapsed() < Duration::from_secs(10));
    }
    #[test]
    fn cleans_mirror_and_unified_log() {
        assert_eq!(clean_log_line("Timestamp foo"), None);
        assert_eq!(
            clean_log_line("2026-09-25 10:23:23.361 Df Puzzle[19921:134cebb] hello"),
            Some("10:23:23.361 hello".into())
        );
        assert_eq!(
            console_line(
                "2026-09-28 10:33:35.230501+0200 Puzzle[9836:34476303] [Session] Token expired"
            ),
            "10:33:35.230 [Session] Token expired"
        );
    }
}
