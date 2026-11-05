//! Interactive terminal interface: one screen.
//!
//! ```text
//!  Puzzle   s ◆ Puzzle    d ▯ My iPhone
//!
//!  ⠹ Compiling  BoardView.swift · Puzzle                  1:48 · ~1:05 left
//!  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━──────────────────────  62%
//!  18 of 27 targets · 412 tasks · 3 warnings
//!  ── Build ─ Console ──────────────────────────────────────────────────
//!  …log…
//!  ▶ r Run on My iPhone    b build  t test  c clean  h history  ? keys
//! ```
//!
//! All Apple tool work stays off the input/render thread. Rendering only walks
//! the visible part of the log, so large build logs stay responsive.
use crate::{
    diagnose,
    job::{self, Job},
    progress::{Estimate, Progress, Stage},
    runtime::{self, Event, Task},
    store::{self, Prefs},
    xcode::{self, Destination, Project, Scheme},
};
use anyhow::{Context, Result, bail};
use crossterm::{
    event::{
        self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture,
        Event as TerminalEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent,
        MouseEventKind,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen, SetTitle},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Paragraph},
};
use std::{
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const HELP: &[(&str, &str)] = &[
    ("r", "run (build, install, launch)"),
    ("b  t  c  p", "build · test · clean · resolve packages"),
    ("x", "stop"),
    ("s", "scheme"),
    ("d", "device or simulator"),
    ("h", "history"),
    ("tab", "build log ⇄ app console"),
    ("i", "problems only ⇄ full log"),
    ("/", "filter the log"),
    ("↑ ↓  pgup pgdn", "scroll"),
    ("g  G", "top · follow the end"),
    ("e", "open the log in $EDITOR"),
    ("esc", "clear filter · back to the latest run"),
    ("q", "quit"),
];

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}
fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

struct TerminalGuard;
impl TerminalGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        if let Err(e) = execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableFocusChange
        ) {
            let _ = terminal::disable_raw_mode();
            let _ = execute!(
                io::stdout(),
                DisableFocusChange,
                DisableMouseCapture,
                LeaveAlternateScreen
            );
            return Err(e.into());
        }
        Ok(Self)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            SetTitle(""),
            DisableFocusChange,
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Popup {
    Schemes { cursor: usize, filter: String },
    Devices { cursor: usize, filter: String },
    History { cursor: usize },
    Rename { udid: String, text: String },
    ConfirmDelete,
    Help,
}

enum Discovery {
    Schemes(PathBuf, Result<Vec<Scheme>>),
    Destinations(Result<Vec<Destination>>),
    Renamed {
        udid: String,
        name: String,
        result: Result<()>,
    },
}

/// Visible line numbers for the current run, phase, filter and mode.
#[derive(Default)]
struct ViewCache {
    key: Option<(i64, usize, String, bool)>,
    scanned: usize,
    fresh_done: bool,
    indices: Option<Vec<usize>>,
}

/// The job currently running, with what the screen needs to show its progress.
struct Live {
    task: Option<Task>,
    progress: Progress,
    key: String,
    started: Instant,
    destination: String,
    build_secs: Option<f64>,
}

struct App {
    root: PathBuf,
    project: Project,
    schemes: Vec<Scheme>,
    destinations: Vec<Destination>,
    scheme: Option<usize>,
    destination: Option<usize>,
    schemes_loaded: bool,
    destinations_loaded: bool,
    want_scheme: String,
    want_destination: String,
    jobs: Vec<Job>,
    /// Index into `jobs` of the run on screen; 0 is the newest.
    shown: usize,
    /// 0 build log, 1 app console.
    phase: usize,
    live: Option<Live>,
    follow: bool,
    top: usize,
    skip: usize,
    view: ViewCache,
    diagnosis: Option<((i64, usize), Vec<diagnose::Problem>)>,
    problems_only: bool,
    filter: String,
    typing: bool,
    popup: Option<Popup>,
    toast: Option<(String, Instant)>,
    tx: mpsc::Sender<Discovery>,
    rx: mpsc::Receiver<Discovery>,
    background: Vec<thread::JoinHandle<()>>,
    log_area: Rect,
    popup_area: Rect,
    tick: usize,
    focused: bool,
    last_check: Instant,
    #[cfg(test)]
    test_mode: bool,
}

impl App {
    fn new(root: PathBuf, project: Project, initial_scheme: Option<&str>) -> Self {
        let prefs = store::load_prefs(&project);
        let jobs = store::load_history(&project);
        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            root,
            want_scheme: initial_scheme.unwrap_or(&prefs.scheme).to_owned(),
            want_destination: prefs.simulator,
            project,
            schemes: vec![],
            destinations: vec![],
            scheme: None,
            destination: None,
            schemes_loaded: false,
            destinations_loaded: false,
            jobs,
            shown: 0,
            phase: 0,
            live: None,
            follow: true,
            top: 0,
            skip: 0,
            view: ViewCache::default(),
            diagnosis: None,
            problems_only: false,
            filter: String::new(),
            typing: false,
            popup: None,
            toast: None,
            tx,
            rx,
            background: vec![],
            log_area: Rect::default(),
            popup_area: Rect::default(),
            tick: 0,
            focused: true,
            last_check: Instant::now(),
            #[cfg(test)]
            test_mode: false,
        };
        app.discover_schemes();
        app.discover_destinations();
        app.show_latest();
        app
    }

    // ── discovery ────────────────────────────────────────────────────────────
    fn discover_schemes(&mut self) {
        #[cfg(test)]
        if self.test_mode {
            return;
        }
        let tx = self.tx.clone();
        let project = self.project.clone();
        self.background.push(thread::spawn(move || {
            let result = xcode::schemes(&project);
            let _ = tx.send(Discovery::Schemes(project.name, result));
        }));
    }
    fn discover_destinations(&mut self) {
        #[cfg(test)]
        if self.test_mode {
            return;
        }
        let tx = self.tx.clone();
        self.background.push(thread::spawn(move || {
            let _ = tx.send(Discovery::Destinations(xcode::destinations()));
        }));
    }
    /// Follows the project if its workspace/project was created, moved or deleted.
    fn revalidate_project(&mut self) -> bool {
        let Ok(found) = Project::find(&self.root) else {
            return false;
        };
        if found == self.project {
            return false;
        }
        let old = self
            .project
            .name
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        self.project = found;
        let prefs = store::load_prefs(&self.project);
        if !prefs.scheme.is_empty() {
            self.want_scheme = prefs.scheme;
        } else if let Some(s) = self.selected_scheme() {
            self.want_scheme = s.name.clone();
        }
        if !prefs.simulator.is_empty() {
            self.want_destination = prefs.simulator;
        }
        self.schemes.clear();
        self.scheme = None;
        self.schemes_loaded = false;
        if self.live.is_none() {
            self.jobs = store::load_history(&self.project);
            self.show_latest();
        }
        self.discover_schemes();
        self.say(format!(
            "{old} is gone · now using {}",
            self.project_label()
        ));
        true
    }

    fn project_label(&self) -> String {
        self.project
            .name
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }
    fn selected_scheme(&self) -> Option<&Scheme> {
        self.scheme.and_then(|i| self.schemes.get(i))
    }
    fn selected_destination(&self) -> Option<&Destination> {
        self.destination.and_then(|i| self.destinations.get(i))
    }
    fn say(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), Instant::now()));
    }
    fn save_prefs(&mut self) {
        #[cfg(test)]
        if self.test_mode {
            return;
        }
        let prefs = Prefs {
            scheme: self
                .selected_scheme()
                .map(|s| s.name.clone())
                .unwrap_or_default(),
            simulator: self
                .selected_destination()
                .map(|d| d.udid.clone())
                .unwrap_or_default(),
        };
        if let Err(e) = store::save_prefs(&self.project, &prefs) {
            self.say(format!("Couldn't remember the selection: {e}"));
        }
    }

    // ── events ───────────────────────────────────────────────────────────────
    fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(message) = self.rx.try_recv() {
            changed = true;
            match message {
                Discovery::Schemes(container, result) => {
                    if container != self.project.name {
                        continue; // the project changed while this ran
                    }
                    self.schemes_loaded = true;
                    match result {
                        Ok(schemes) => {
                            let keep = self.selected_scheme().map(|s| s.name.clone());
                            self.schemes = schemes;
                            let want = keep.unwrap_or_else(|| self.want_scheme.clone());
                            self.scheme = self
                                .schemes
                                .iter()
                                .position(|s| s.name == want)
                                .or_else(|| xcode::default_scheme(&self.project, &self.schemes));
                        }
                        Err(e) => self.say(format!("Couldn't list schemes: {e}")),
                    }
                }
                Discovery::Destinations(result) => {
                    self.destinations_loaded = true;
                    match result {
                        Ok(list) => {
                            let keep = self
                                .selected_destination()
                                .map(|d| d.udid.clone())
                                .unwrap_or_else(|| self.want_destination.clone());
                            self.destinations = list;
                            self.destination = self
                                .destinations
                                .iter()
                                .position(|d| d.udid == keep)
                                .or_else(|| xcode::default_destination(&self.destinations));
                        }
                        Err(e) => self.say(format!("Couldn't list devices: {e}")),
                    }
                }
                Discovery::Renamed { udid, name, result } => match result {
                    Ok(()) => {
                        if let Some(d) = self.destinations.iter_mut().find(|d| d.udid == udid) {
                            d.name = name.clone();
                        }
                        self.say(format!("Renamed to {name}"));
                    }
                    Err(e) => self.say(format!("Couldn't rename: {e}")),
                },
            }
        }
        let events: Vec<Event> = self
            .live
            .as_ref()
            .and_then(|l| l.task.as_ref())
            .map(|t| t.events.try_iter().take(20_000).collect())
            .unwrap_or_default();
        changed |= !events.is_empty();
        for event in events {
            self.apply(event);
        }
        if self.live.is_none()
            && self.popup.is_none()
            && self.last_check.elapsed() >= Duration::from_secs(3)
        {
            self.last_check = Instant::now();
            changed |= self.revalidate_project();
        }
        if self
            .toast
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(6))
        {
            self.toast = None;
            changed = true;
        }
        changed
    }

    fn apply(&mut self, event: Event) {
        if self.live.is_none() || self.jobs.is_empty() {
            return;
        }
        let container = self.project.name.clone();
        match event {
            Event::Line(line) => {
                let line = job::strip_paths(&line, &container);
                if let Some(live) = self.live.as_mut() {
                    live.progress.line(&line);
                }
                self.jobs[0].line(line);
            }
            Event::Console(line) => self.jobs[0].line(job::strip_paths(&line, &container)),
            Event::State(state) => match state.as_str() {
                "installing" => {
                    self.jobs[0].build_finished();
                    let Some(live) = self.live.as_mut() else {
                        return;
                    };
                    let secs = live.started.elapsed().as_secs_f64();
                    live.build_secs = Some(secs);
                    let estimate = Estimate {
                        tasks: live.progress.tasks,
                        seconds: secs,
                    };
                    live.progress.set_stage(Stage::Installing);
                    let key = live.key.clone();
                    self.remember(&key, estimate);
                }
                "launching" => {
                    self.jobs[0].status = state;
                    if let Some(live) = self.live.as_mut() {
                        live.progress.set_stage(Stage::Launching);
                    }
                }
                _ => self.jobs[0].status = state,
            },
            Event::Attached(app) => {
                self.jobs[0].line(format!("▸ Launched {app}"));
                self.jobs[0].attach(app.clone());
                if self.shown == 0 {
                    self.phase = 1;
                    self.follow = true;
                }
                let target = self
                    .live
                    .as_ref()
                    .map(|l| l.destination.clone())
                    .unwrap_or_default();
                self.notify(&format!("Running on {target}"), &app);
            }
            Event::Done { code, stopped } => {
                self.jobs[0].finish(code, stopped);
                let Some(live) = self.live.take() else { return };
                let (kind, outcome, attached) = {
                    let j = &self.jobs[0];
                    (j.kind.clone(), j.outcome.clone(), j.attached.is_some())
                };
                if code == 0 && !stopped && kind != "run" && kind != "clean" {
                    self.remember(
                        &live.key,
                        Estimate {
                            tasks: live.progress.tasks,
                            seconds: live.started.elapsed().as_secs_f64(),
                        },
                    );
                }
                #[cfg(test)]
                let skip_save = self.test_mode;
                #[cfg(not(test))]
                let skip_save = false;
                if !skip_save && let Err(e) = store::save_job(&self.project, &self.jobs[0]) {
                    self.say(format!("Couldn't save the run: {e}"));
                }
                if let Some(task) = live.task {
                    task.wait();
                }
                if outcome == "failed" {
                    if self.shown == 0 {
                        self.phase = 0;
                        self.filter.clear();
                        self.problems_only = self.jobs[0].phases[0].counts().0 > 0;
                        self.reset_view();
                    }
                    let stale = self.jobs[0].phases[0].lines.iter().rev().take(40).any(|l| {
                        l.contains("does not contain a scheme named")
                            || l.contains("does not exist")
                    });
                    if stale && !self.revalidate_project() {
                        self.discover_schemes();
                    }
                }
                if live.started.elapsed() > Duration::from_secs(20) && !attached {
                    let title = format!("{} {outcome}", capitalize(&kind));
                    self.notify(&title, &self.project_label());
                }
            }
        }
    }

    fn remember(&mut self, key: &str, estimate: Estimate) {
        #[cfg(test)]
        if self.test_mode {
            return;
        }
        let _ = store::save_estimate(&self.project, key, estimate);
    }

    /// A macOS notification, only when the terminal is in the background.
    fn notify(&self, title: &str, body: &str) {
        #[cfg(test)]
        if self.test_mode {
            return;
        }
        if self.focused {
            return;
        }
        let quote = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            quote(body),
            quote(title)
        );
        let _ = Command::new("osascript")
            .args(["-e", &script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|mut child| thread::spawn(move || child.wait()));
    }

    fn busy(&self) -> bool {
        self.live.is_some()
    }

    fn start(&mut self, kind: &str) {
        if self.busy() {
            self.say("Already running · x stops it");
            return;
        }
        if !self.project.name.exists() {
            self.revalidate_project();
            if !self.project.name.exists() {
                self.say(format!("{} no longer exists", self.project_label()));
            }
            return;
        }
        if kind != "resolve" {
            if self.selected_scheme().is_none() {
                self.open_schemes();
                return;
            }
            if self.selected_destination().is_none() {
                self.open_devices();
                return;
            }
        }
        let scheme = self.selected_scheme().cloned().unwrap_or(Scheme {
            name: String::new(),
            package: false,
        });
        let destination = self.selected_destination().cloned().unwrap_or(Destination {
            name: "My Mac".into(),
            runtime: "macOS".into(),
            udid: "mac".into(),
            ..Destination::default()
        });
        let key = format!(
            "{}|{}|{}",
            if kind == "run" { "build" } else { kind },
            scheme.name,
            destination.udid
        );
        #[cfg(test)]
        let estimate = None;
        #[cfg(not(test))]
        let estimate = store::load_estimate(&self.project, &key);
        self.jobs.insert(0, Job::new(kind));
        self.jobs.truncate(50);
        self.toast = None;
        self.show_latest();
        #[cfg(test)]
        let task = if self.test_mode {
            None
        } else {
            Some(runtime::start(
                kind,
                self.project.clone(),
                scheme,
                destination.clone(),
            ))
        };
        #[cfg(not(test))]
        let task = Some(runtime::start(
            kind,
            self.project.clone(),
            scheme,
            destination.clone(),
        ));
        self.live = Some(Live {
            task,
            progress: Progress::new(estimate),
            key,
            started: Instant::now(),
            destination: destination.name,
            build_secs: None,
        });
    }

    fn stop(&mut self) {
        if let Some(task) = self.live.as_ref().and_then(|l| l.task.as_ref()) {
            task.cancel();
            self.say("Stopping…");
        }
    }

    fn shutdown(&mut self) {
        if let Some(live) = self.live.take()
            && let Some(task) = live.task
        {
            task.cancel();
            // Drain to completion so a bounded channel cannot deadlock shutdown.
            let mut code = 130;
            while let Ok(e) = task.events.recv() {
                if let Some(job) = self.jobs.first_mut() {
                    match e {
                        Event::Line(s) | Event::Console(s) => {
                            job.line(job::strip_paths(&s, &self.project.name))
                        }
                        Event::Attached(a) => job.attach(a),
                        Event::State(s) => job.status = s,
                        Event::Done { code: c, .. } => {
                            code = c;
                            break;
                        }
                    }
                }
            }
            task.wait();
            if let Some(job) = self.jobs.first_mut() {
                job.finish(code, true);
                let _ = store::save_job(&self.project, job);
            }
        }
        for handle in self.background.drain(..) {
            let _ = handle.join();
        }
    }

    // ── log view ─────────────────────────────────────────────────────────────
    fn show_latest(&mut self) {
        self.show(0);
    }
    fn show(&mut self, index: usize) {
        self.shown = index.min(self.jobs.len().saturating_sub(1));
        self.phase = self.jobs.get(self.shown).map_or(0, |j| {
            usize::from(j.phases.len() > 1 && j.outcome != "failed")
        });
        self.filter.clear();
        self.problems_only = self.jobs.get(self.shown).is_some_and(|j| {
            j.outcome == "failed" && j.phases.first().is_some_and(|p| p.counts().0 > 0)
        });
        self.reset_view();
        self.load_shown();
    }
    fn load_shown(&mut self) {
        let phase = self.phase;
        if let Some(job) = self.jobs.get_mut(self.shown)
            && let Some(p) = job.phases.get_mut(phase)
        {
            p.ensure_loaded();
        }
    }
    fn shown_phase(&self) -> Option<&job::Phase> {
        self.jobs.get(self.shown)?.phases.get(self.phase)
    }
    /// Brings the cached list of visible line numbers up to date: everything
    /// (`None`), problems (errors first), or filter matches. Appended lines are
    /// scanned incrementally, so a live 200k-line log is never rescanned per frame.
    fn refresh_view(&mut self) {
        let Some(job) = self.jobs.get(self.shown) else {
            self.view = ViewCache::default();
            return;
        };
        let Some(phase) = job.phases.get(self.phase) else {
            self.view = ViewCache::default();
            return;
        };
        let key = (
            job.started.timestamp_nanos_opt().unwrap_or(0),
            self.phase,
            self.filter.clone(),
            self.problems_only,
        );
        let len = phase.lines.len();
        if self.view.key.as_ref() != Some(&key) || self.view.scanned > len {
            self.view = ViewCache {
                key: Some(key),
                ..ViewCache::default()
            };
        }
        if self.view.fresh_done && self.view.scanned == len {
            return;
        }
        self.view.fresh_done = true;
        if !self.filter.is_empty() {
            let needle = self.filter.to_lowercase();
            let v = self.view.indices.get_or_insert_with(Vec::new);
            for (i, line) in phase.lines.iter().enumerate().skip(self.view.scanned) {
                if line.to_lowercase().contains(&needle) {
                    v.push(i);
                }
            }
        } else if self.problems_only {
            let issues = phase.issue_indices();
            let (errors, warnings): (Vec<usize>, Vec<usize>) = issues
                .iter()
                .partition(|&&i| job::kind(&phase.lines[i]) == "error");
            self.view.indices = Some(errors.into_iter().chain(warnings).collect());
        } else {
            self.view.indices = None;
        }
        self.view.scanned = len;
    }
    fn visible_len(&self) -> usize {
        match &self.view.indices {
            Some(v) => v.len(),
            None => self.shown_phase().map_or(0, |p| p.lines.len()),
        }
    }
    fn line_at(&self, n: usize) -> &str {
        let Some(phase) = self.shown_phase() else {
            return "";
        };
        let i = match &self.view.indices {
            Some(v) => v.get(n).copied().unwrap_or(usize::MAX),
            None => n,
        };
        phase.lines.get(i).map_or("", |l| clean(l))
    }
    fn rows_at(&self, n: usize) -> usize {
        wrapped_rows(self.line_at(n), self.log_area.width.max(1) as usize)
    }
    /// Position (line, rows hidden above it) that shows the last screenful.
    /// A single line taller than the viewport shows its final rows.
    fn tail_pos(&self) -> (usize, usize) {
        let height = self.log_area.height.max(1) as usize;
        let mut rows = 0;
        let mut top = self.visible_len();
        while top > 0 {
            let r = self.rows_at(top - 1);
            if rows + r >= height {
                return (top - 1, rows + r - height);
            }
            rows += r;
            top -= 1;
        }
        (0, 0)
    }
    /// Where the view starts, as (line, rows of that line scrolled away).
    fn view_pos(&self) -> (usize, usize) {
        let tail = self.tail_pos();
        if self.follow {
            tail
        } else {
            (self.top, self.skip).min(tail)
        }
    }
    /// Scrolls by screen rows, so wrapped lines of any length are reachable.
    fn scroll(&mut self, delta: isize) {
        self.refresh_view();
        let tail = self.tail_pos();
        let (mut top, mut skip) = self.view_pos();
        let len = self.visible_len();
        if delta > 0 {
            skip += delta as usize;
            while top < len && skip >= self.rows_at(top) {
                skip -= self.rows_at(top);
                top += 1;
            }
        } else {
            let mut d = delta.unsigned_abs();
            if skip >= d {
                skip -= d;
            } else {
                d -= skip;
                skip = 0;
                while d > 0 && top > 0 {
                    top -= 1;
                    let r = self.rows_at(top);
                    if r >= d {
                        skip = r - d;
                        d = 0;
                    } else {
                        d -= r;
                    }
                }
            }
        }
        if (top, skip) >= tail {
            self.follow = true;
            (self.top, self.skip) = (0, 0);
        } else {
            self.follow = false;
            (self.top, self.skip) = (top, skip);
        }
    }
    fn jump(&mut self, end: bool) {
        self.follow = end;
        (self.top, self.skip) = (0, 0);
    }
    /// Back to the default position: the end of the log, or the top of the
    /// problems list, where the errors are.
    fn reset_view(&mut self) {
        self.follow = !(self.problems_only && self.filter.is_empty());
        (self.top, self.skip) = (0, 0);
    }
    /// Problems for the failure card, cached per run and log length.
    fn problems(&mut self) -> Vec<diagnose::Problem> {
        let Some(job) = self.jobs.get(self.shown) else {
            return vec![];
        };
        let Some(phase) = job.phases.first().filter(|p| p.is_loaded()) else {
            return vec![];
        };
        let key = (
            job.started.timestamp_nanos_opt().unwrap_or(0),
            phase.lines.len(),
        );
        match &self.diagnosis {
            Some((k, problems)) if *k == key => problems.clone(),
            _ => {
                let problems = diagnose::problems(&phase.lines, 2);
                self.diagnosis = Some((key, problems.clone()));
                problems
            }
        }
    }

    fn edit<B: ratatui::backend::Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()> {
        self.refresh_view();
        let len = self.visible_len();
        if len == 0 {
            self.say("Nothing to open");
            return Ok(());
        }
        let text: String = (0..len).map(|n| format!("{}\n", self.line_at(n))).collect();
        let path = std::env::temp_dir().join(format!(
            "xctui-{}-{}.log",
            std::process::id(),
            chrono::Local::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        if let Err(e) = std::fs::write(&path, text) {
            self.say(format!("Couldn't write the log: {e}"));
            return Ok(());
        }
        terminal::disable_raw_mode()?;
        execute!(
            io::stdout(),
            DisableFocusChange,
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        )?;
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".into());
        // $EDITOR may carry arguments ("code -w").
        let mut parts = editor.split_whitespace();
        let result = Command::new(parts.next().unwrap_or("vi"))
            .args(parts)
            .arg(&path)
            .status();
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableFocusChange
        )?;
        terminal::enable_raw_mode()?;
        terminal.clear()?;
        let _ = std::fs::remove_file(path);
        match result {
            Ok(s) if s.success() => (),
            Ok(s) => self.say(format!("Editor: {s}")),
            Err(e) => self.say(format!("Editor: {e}")),
        }
        Ok(())
    }

    // ── pickers ──────────────────────────────────────────────────────────────
    fn open_schemes(&mut self) {
        if !self.schemes_loaded {
            self.say("Still looking for schemes…");
        }
        let cursor = self
            .scheme_rows("")
            .iter()
            .position(|&i| Some(i) == self.scheme)
            .unwrap_or(0);
        self.popup = Some(Popup::Schemes {
            cursor,
            filter: String::new(),
        });
    }
    fn open_devices(&mut self) {
        if !self.destinations_loaded {
            self.say("Still looking for devices…");
        }
        let cursor = self
            .device_rows("")
            .iter()
            .position(|&i| Some(i) == self.destination)
            .unwrap_or(0);
        self.popup = Some(Popup::Devices {
            cursor,
            filter: String::new(),
        });
    }
    fn scheme_rows(&self, filter: &str) -> Vec<usize> {
        let mut rows: Vec<usize> = (0..self.schemes.len())
            .filter(|&i| contains(&self.schemes[i].name, filter))
            .collect();
        rows.sort_by_key(|&i| self.schemes[i].package);
        rows
    }
    /// Devices first, then My Mac, then simulators with booted ones first.
    fn device_rows(&self, filter: &str) -> Vec<usize> {
        let mut rows: Vec<usize> = (0..self.destinations.len())
            .filter(|&i| {
                let d = &self.destinations[i];
                contains(&format!("{} {}", d.name, d.runtime), filter)
            })
            .collect();
        rows.sort_by_key(|&i| {
            let d = &self.destinations[i];
            (
                if d.device {
                    0
                } else if d.is_mac() {
                    1
                } else if d.state == "Booted" {
                    2
                } else {
                    3
                },
                i,
            )
        });
        rows
    }

    // ── input ────────────────────────────────────────────────────────────────
    fn key<B: ratatui::backend::Backend>(
        &mut self,
        key: KeyEvent,
        terminal: &mut Terminal<B>,
    ) -> Result<bool> {
        if key.kind == KeyEventKind::Release {
            return Ok(false);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return Ok(true);
        }
        if self.popup.is_some() {
            self.popup_key(key);
            return Ok(false);
        }
        if self.typing {
            match key.code {
                KeyCode::Enter => self.typing = false,
                KeyCode::Esc => {
                    self.filter.clear();
                    self.typing = false;
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c) if !ctrl => self.filter.push(c),
                _ => (),
            }
            self.reset_view();
            return Ok(false);
        }
        let half = (self.log_area.height / 2).max(1) as isize;
        if ctrl {
            match key.code {
                KeyCode::Char('d') => self.scroll(half),
                KeyCode::Char('u') => self.scroll(-half),
                _ => (),
            }
            return Ok(false);
        }
        match key.code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('r') => self.start("run"),
            KeyCode::Char('b') => self.start("build"),
            KeyCode::Char('t') => self.start("test"),
            KeyCode::Char('c') => self.start("clean"),
            KeyCode::Char('p') => self.start("resolve"),
            KeyCode::Char('x') => self.stop(),
            KeyCode::Char('s') => {
                if self.busy() {
                    self.say("Stop the current run to change the scheme");
                } else {
                    self.open_schemes();
                }
            }
            KeyCode::Char('d') => {
                if self.busy() {
                    self.say("Stop the current run to change the device");
                } else {
                    self.open_devices();
                }
            }
            KeyCode::Char('h') => {
                if self.jobs.is_empty() {
                    self.say("No runs yet");
                } else {
                    self.popup = Some(Popup::History { cursor: self.shown });
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let phases = self.jobs.get(self.shown).map_or(0, |j| j.phases.len());
                if phases > 1 {
                    self.phase = 1 - self.phase;
                    self.reset_view();
                    self.load_shown();
                } else {
                    self.say("The app console appears once the app runs");
                }
            }
            KeyCode::Char('i') => {
                self.problems_only = !self.problems_only;
                self.reset_view();
            }
            KeyCode::Char('/') => {
                self.typing = true;
                self.reset_view();
            }
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.reset_view();
                } else if self.shown != 0 {
                    self.show_latest();
                }
            }
            KeyCode::Char('j') | KeyCode::Down => self.scroll(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.scroll(self.log_area.height.max(1) as isize)
            }
            KeyCode::PageUp => self.scroll(-(self.log_area.height.max(1) as isize)),
            KeyCode::Char('g') | KeyCode::Home => self.jump(false),
            KeyCode::Char('G') | KeyCode::End => self.jump(true),
            KeyCode::Char('e') => self.edit(terminal)?,
            KeyCode::Char('?') | KeyCode::F(1) => self.popup = Some(Popup::Help),
            _ => (),
        }
        Ok(false)
    }

    fn popup_key(&mut self, key: KeyEvent) {
        let Some(popup) = self.popup.take() else {
            return;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        self.popup = match popup {
            Popup::Help => None,
            Popup::ConfirmDelete => {
                if key.code == KeyCode::Char('y') {
                    self.delete_history();
                }
                None
            }
            Popup::Rename { udid, mut text } => match key.code {
                KeyCode::Esc => None,
                KeyCode::Enter => {
                    self.rename(udid, text.trim().to_owned());
                    None
                }
                KeyCode::Backspace => {
                    text.pop();
                    Some(Popup::Rename { udid, text })
                }
                KeyCode::Char(c) if !ctrl => {
                    text.push(c);
                    Some(Popup::Rename { udid, text })
                }
                _ => Some(Popup::Rename { udid, text }),
            },
            Popup::History { cursor } => {
                let last = self.jobs.len().saturating_sub(1);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('h') | KeyCode::Char('q') => None,
                    KeyCode::Enter => {
                        self.show(cursor);
                        None
                    }
                    KeyCode::Down | KeyCode::Char('j') => Some(Popup::History {
                        cursor: (cursor + 1).min(last),
                    }),
                    KeyCode::Up | KeyCode::Char('k') => Some(Popup::History {
                        cursor: cursor.saturating_sub(1),
                    }),
                    KeyCode::Char('D') => {
                        if self.jobs.len() > usize::from(self.busy()) {
                            Some(Popup::ConfirmDelete)
                        } else {
                            None
                        }
                    }
                    _ => Some(Popup::History { cursor }),
                }
            }
            Popup::Schemes { cursor, filter } => {
                let rows = self.scheme_rows(&filter);
                match list_key(key, cursor, filter, rows.len()) {
                    ListKey::Close => None,
                    ListKey::Pick(n) => {
                        if let Some(&i) = rows.get(n) {
                            self.scheme = Some(i);
                            self.save_prefs();
                        }
                        None
                    }
                    ListKey::Stay(cursor, filter) => Some(Popup::Schemes { cursor, filter }),
                }
            }
            Popup::Devices { cursor, filter } => {
                let rows = self.device_rows(&filter);
                if ctrl && key.code == KeyCode::Char('r') {
                    match rows.get(cursor).map(|&i| &self.destinations[i]) {
                        Some(d) if !d.device && !d.is_mac() => Some(Popup::Rename {
                            udid: d.udid.clone(),
                            text: d.name.clone(),
                        }),
                        _ => Some(Popup::Devices { cursor, filter }),
                    }
                } else {
                    match list_key(key, cursor, filter, rows.len()) {
                        ListKey::Close => None,
                        ListKey::Pick(n) => {
                            if let Some(&i) = rows.get(n) {
                                self.destination = Some(i);
                                self.save_prefs();
                            }
                            None
                        }
                        ListKey::Stay(cursor, filter) => Some(Popup::Devices { cursor, filter }),
                    }
                }
            }
        };
    }

    fn rename(&mut self, udid: String, name: String) {
        if name.is_empty() {
            return;
        }
        #[cfg(test)]
        if self.test_mode {
            return;
        }
        let tx = self.tx.clone();
        self.background.push(thread::spawn(move || {
            let result = xcode::rename_simulator(&udid, &name);
            let _ = tx.send(Discovery::Renamed { udid, name, result });
        }));
    }

    fn delete_history(&mut self) {
        #[cfg(test)]
        let result = if self.test_mode {
            Ok(())
        } else {
            store::delete_history(&self.project)
        };
        #[cfg(not(test))]
        let result = store::delete_history(&self.project);
        match result {
            Ok(()) => {
                let keep = usize::from(self.busy());
                let n = self.jobs.len() - keep;
                self.jobs.truncate(keep);
                self.show_latest();
                self.say(format!("Deleted {}", job::count(n, "run")));
            }
            Err(e) => self.say(format!("Couldn't delete the runs: {e}")),
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        let delta: isize = match event.kind {
            MouseEventKind::ScrollUp => -3,
            MouseEventKind::ScrollDown => 3,
            _ => return,
        };
        if self.popup.is_some() {
            let inside = contains_point(self.popup_area, event.column, event.row);
            if inside {
                let code = if delta < 0 {
                    KeyCode::Up
                } else {
                    KeyCode::Down
                };
                for _ in 0..delta.unsigned_abs() {
                    self.popup_key(KeyEvent::from(code));
                }
            }
            return;
        }
        if contains_point(self.log_area, event.column, event.row) {
            self.scroll(delta);
        }
    }
}
impl Drop for App {
    fn drop(&mut self) {
        self.shutdown();
    }
}

enum ListKey {
    Close,
    Pick(usize),
    Stay(usize, String),
}
/// Type-to-filter list navigation shared by the scheme and device pickers.
fn list_key(key: KeyEvent, cursor: usize, mut filter: String, len: usize) -> ListKey {
    let last = len.saturating_sub(1);
    match key.code {
        KeyCode::Esc => {
            if filter.is_empty() {
                return ListKey::Close;
            }
            filter.clear();
            ListKey::Stay(0, filter)
        }
        KeyCode::Enter => ListKey::Pick(cursor),
        KeyCode::Down => ListKey::Stay((cursor + 1).min(last), filter),
        KeyCode::Up => ListKey::Stay(cursor.saturating_sub(1), filter),
        KeyCode::PageDown => ListKey::Stay((cursor + 10).min(last), filter),
        KeyCode::PageUp => ListKey::Stay(cursor.saturating_sub(10), filter),
        KeyCode::Backspace => {
            filter.pop();
            ListKey::Stay(0, filter)
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            filter.push(c);
            ListKey::Stay(0, filter)
        }
        _ => ListKey::Stay(cursor.min(last), filter),
    }
}

pub fn run(root: &Path, initial_scheme: Option<&str>) -> Result<()> {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        bail!("the TUI needs an interactive terminal; use --help for CLI commands");
    }
    let project = Project::find(root)?;
    let mut app = App::new(root.to_owned(), project, initial_scheme);
    let stop = Arc::new(AtomicBool::new(false));
    let signal = stop.clone();
    ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed))
        .context("install termination handler")?;
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    let mut title = String::new();
    let result = (|| -> Result<()> {
        let mut dirty = true;
        let mut last_frame = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            dirty |= app.poll();
            // Animate the spinner and time-based progress while something runs.
            if app.busy() && last_frame.elapsed() >= Duration::from_millis(100) {
                dirty = true;
            }
            if dirty {
                app.tick = app.tick.wrapping_add(1);
                terminal.draw(|frame| draw(&mut app, frame))?;
                last_frame = Instant::now();
                dirty = false;
                let next = window_title(&mut app);
                if next != title {
                    let _ = execute!(io::stdout(), SetTitle(&next));
                    title = next;
                }
            }
            if event::poll(Duration::from_millis(50))? {
                // Drain bursts (trackpad scrolling) before the next frame.
                loop {
                    match event::read()? {
                        TerminalEvent::Key(k) => {
                            dirty = true;
                            if app.key(k, &mut terminal)? {
                                return Ok(());
                            }
                        }
                        TerminalEvent::Mouse(m) => {
                            if matches!(
                                m.kind,
                                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                            ) {
                                app.mouse(m);
                                dirty = true;
                            }
                        }
                        TerminalEvent::FocusGained => app.focused = true,
                        TerminalEvent::FocusLost => app.focused = false,
                        TerminalEvent::Resize(_, _) => dirty = true,
                        _ => (),
                    }
                    if !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
        }
        Ok(())
    })();
    app.shutdown();
    result
}

fn window_title(app: &mut App) -> String {
    let name = app.project_label();
    let name = name
        .rsplit_once('.')
        .map_or(name.as_str(), |(n, _)| n)
        .to_owned();
    match app.live.as_mut() {
        Some(live) if live.build_secs.is_none() => {
            let f = live.progress.fraction(live.started.elapsed().as_secs_f64());
            format!("{:.0}% · {name}", f * 100.0)
        }
        Some(live) => format!("{} · {name}", live.progress.stage.label()),
        None => name,
    }
}

// ── drawing ──────────────────────────────────────────────────────────────────

fn draw(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    app.popup_area = Rect::default();
    if area.width < 40 || area.height < 10 {
        frame.render_widget(Paragraph::new("Make the terminal at least 40 × 10"), area);
        app.log_area = Rect::default();
        return;
    }
    let x = area.x + 1;
    let w = area.width - 2;
    draw_header(app, frame, Rect::new(x, area.y, w, 1));
    app.refresh_view();
    let status = status_lines(app, w as usize);
    let status_h = status.len() as u16;
    frame.render_widget(
        Paragraph::new(status),
        Rect::new(x, area.y + 2, w, status_h),
    );
    let rule_y = area.y + 2 + status_h + 1;
    draw_rule(app, frame, Rect::new(x, rule_y, w, 1));
    let log_y = rule_y + 1;
    let footer_y = area.bottom() - 1;
    app.log_area = Rect::new(x, log_y, w, footer_y.saturating_sub(log_y + 1));
    draw_log(app, frame);
    draw_footer(app, frame, Rect::new(x, footer_y, w, 1));
    if let Some(popup) = app.popup.clone() {
        draw_popup(app, frame, popup);
    }
}

fn draw_header(app: &App, frame: &mut Frame, area: Rect) {
    let chip = |key: &'static str, icon: &str, text: String, empty: bool| -> Vec<Span<'static>> {
        vec![
            Span::styled(format!("{key} "), dim()),
            Span::styled(
                format!(" {icon} {text} "),
                if empty {
                    dim().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default().add_modifier(Modifier::REVERSED)
                },
            ),
            Span::raw("   "),
        ]
    };
    let project = app.project_label();
    let mut spans = vec![
        Span::styled(
            project
                .rsplit_once('.')
                .map_or(project.as_str(), |(n, _)| n)
                .to_owned(),
            bold(),
        ),
        Span::raw("   "),
    ];
    let (scheme, no_scheme) = match app.selected_scheme() {
        Some(s) => (s.name.clone(), false),
        None if !app.schemes_loaded => ("…".into(), true),
        None => ("Scheme".into(), true),
    };
    spans.extend(chip("s", "◆", scheme, no_scheme));
    let (device, icon, no_device) = match app.selected_destination() {
        Some(d) if d.device => (d.name.clone(), "▯", false),
        Some(d) if d.is_mac() => (d.name.clone(), "▭", false),
        Some(d) => (
            format!(
                "{} {}",
                d.name,
                d.runtime.split_whitespace().last().unwrap_or_default()
            ),
            "▢",
            false,
        ),
        None if !app.destinations_loaded => ("…".into(), "▯", true),
        None => ("Device".into(), "▯", true),
    };
    spans.extend(chip("d", icon, device, no_device));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn bar(fraction: f64, width: usize, style: Style) -> Vec<Span<'static>> {
    let cells = width.max(1);
    let filled = ((fraction.clamp(0.0, 1.0)) * cells as f64).round() as usize;
    vec![
        Span::styled("━".repeat(filled), style),
        Span::styled("─".repeat(cells - filled), dim()),
    ]
}
/// A short segment bouncing across the bar, for work without a measurable end.
fn sweep(tick: usize, width: usize) -> Vec<Span<'static>> {
    let seg = (width / 6).max(3).min(width);
    let span = width.saturating_sub(seg).max(1);
    let pos = tick % (span * 2);
    let at = if pos > span { span * 2 - pos } else { pos }.min(width - seg);
    vec![
        Span::styled("─".repeat(at), dim()),
        Span::styled("━".repeat(seg), bold()),
        Span::styled("─".repeat(width - seg - at), dim()),
    ]
}

fn clock(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Left text and right text on one line, the left truncated to fit.
fn split_line(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let right_w: usize = right.iter().map(|s| s.content.width()).sum();
    let mut budget = width.saturating_sub(right_w + 2);
    let mut spans = vec![];
    for s in left {
        if budget == 0 {
            break;
        }
        let text = truncate(&s.content, budget);
        budget -= text.width();
        spans.push(Span::styled(text, s.style));
    }
    let used: usize = spans.iter().map(|s| s.content.width()).sum();
    spans.push(Span::raw(" ".repeat(width.saturating_sub(used + right_w))));
    spans.extend(right);
    Line::from(spans)
}

fn status_lines(app: &mut App, width: usize) -> Vec<Line<'static>> {
    let tick = app.tick;
    let spin = SPINNER[tick % SPINNER.len()];
    if let Some(live) = app.live.as_mut() {
        let elapsed = live.started.elapsed().as_secs_f64();
        let job = &app.jobs[0];
        let (errors, warnings) = job.phases[0].counts();
        let p = &mut live.progress;
        let installing = live.build_secs.is_some();
        let attached = job.attached.is_some();
        if attached {
            let since = job
                .attached
                .map(|t| (chrono::Local::now() - t).num_seconds() as f64)
                .unwrap_or(0.0);
            return vec![
                split_line(
                    vec![
                        Span::styled("● ", Style::default().fg(Color::Green)),
                        Span::styled("Running ", bold()),
                        Span::raw(format!("{} on {}", job.app, live.destination)),
                    ],
                    vec![Span::styled(clock(since), dim())],
                    width,
                ),
                Line::from(""),
                Line::styled(
                    format!(
                        "built in {}",
                        job::short(live.build_secs.unwrap_or(job.build_elapsed))
                    ),
                    dim(),
                ),
            ];
        }
        let label = match (installing, p.stage) {
            (true, Stage::Launching) => format!("Launching on {}", live.destination),
            (true, _) => format!("Installing on {}", live.destination),
            (false, _) if job.kind == "clean" => "Cleaning".to_owned(),
            (false, s) => s.label().to_owned(),
        };
        let mut left = vec![Span::raw(format!("{spin} ")), Span::styled(label, bold())];
        if !p.detail.is_empty() && !installing {
            left.push(Span::raw(format!("  {}", p.detail)));
            if !p.target.is_empty() && !p.detail.starts_with(&p.target) {
                left.push(Span::styled(format!(" · {}", p.target), dim()));
            }
        }
        let mut right = vec![Span::raw(clock(elapsed))];
        if !installing && let Some(left) = p.remaining(elapsed) {
            right.push(Span::styled(format!(" · ~{} left", clock(left)), dim()));
        }
        let pct_w = 5;
        let bar_w = width.saturating_sub(pct_w + 1);
        let (mut bar_spans, pct) = if installing {
            match p.install {
                Some(f) => (bar(f, bar_w, bold()), format!("{:>3.0}%", f * 100.0)),
                None => (sweep(tick, bar_w), String::new()),
            }
        } else if job.kind == "clean" || job.kind == "resolve" {
            (sweep(tick, bar_w), String::new())
        } else {
            let f = p.fraction(elapsed);
            (bar(f, bar_w, bold()), format!("{:>3.0}%", f * 100.0))
        };
        bar_spans.push(Span::raw(format!(" {pct:>4}")));
        let mut facts = vec![];
        if p.total_targets > 0 && !installing {
            facts.push(format!(
                "{} of {} targets",
                p.targets_started().min(p.total_targets),
                p.total_targets
            ));
        }
        if p.tasks > 0 && !installing {
            facts.push(job::count(p.tasks, "task"));
        }
        if p.tests_passed + p.tests_failed > 0 {
            facts.push(format!("{} passed", p.tests_passed));
        }
        let mut third: Vec<Span> = vec![Span::styled(facts.join(" · "), dim())];
        let mut sep = !facts.is_empty();
        let mut add = |text: String, style: Style| {
            if sep {
                third.push(Span::styled(" · ", dim()));
            }
            third.push(Span::styled(text, style));
            sep = true;
        };
        if p.tests_failed > 0 {
            add(
                format!("{} failed", p.tests_failed),
                Style::default().fg(Color::Red),
            );
        }
        if errors > 0 {
            add(job::count(errors, "error"), Style::default().fg(Color::Red));
        }
        if warnings > 0 {
            add(
                job::count(warnings, "warning"),
                Style::default().fg(Color::Yellow),
            );
        }
        return vec![
            split_line(left, right, width),
            Line::from(bar_spans),
            Line::from(third),
        ];
    }
    // Idle: the shown run's outcome, or a ready state.
    let problems = app.problems();
    let Some(job) = app.jobs.get(app.shown) else {
        let ready = match (app.selected_scheme(), app.selected_destination()) {
            (Some(_), Some(d)) => format!("Ready to run on {}", d.name),
            _ if !app.schemes_loaded || !app.destinations_loaded => {
                "Looking for schemes and devices…".into()
            }
            (None, _) => "Choose a scheme".into(),
            (_, None) => "Choose a device".into(),
        };
        return vec![Line::styled(ready, dim()), Line::from(""), Line::from("")];
    };
    let (errors, warnings) = job.phases.first().map(|p| p.counts()).unwrap_or_default();
    let (mark, color) = match job.outcome.as_str() {
        "succeeded" | "exited" => ("✓", Color::Green),
        "failed" => ("✗", Color::Red),
        "stopped" => ("■", Color::Yellow),
        _ => ("•", Color::Reset),
    };
    let what = match (
        job.kind.as_str(),
        job.outcome.as_str(),
        job.build_outcome.as_str(),
    ) {
        ("run", "exited", _) => "App exited".to_owned(),
        ("run", "stopped", "succeeded") => "App stopped".to_owned(),
        ("run", "failed", "succeeded") => "Install or launch failed".to_owned(),
        ("run", o, _) => format!("Build {o}"),
        (k, o, _) => format!("{} {o}", capitalize(k)),
    };
    let when = job::when_at(
        job.started.naive_local(),
        chrono::Local::now().naive_local(),
    );
    let mut facts = vec![job::short(job.elapsed)];
    if errors > 0 {
        facts.push(job::count(errors, "error"));
    }
    if warnings > 0 {
        facts.push(job::count(warnings, "warning"));
    }
    let mut left = vec![
        Span::styled(format!("{mark} "), Style::default().fg(color)),
        Span::styled(what, bold()),
        Span::styled(format!("  {}", facts.join(" · ")), dim()),
    ];
    if app.shown != 0 {
        left.push(Span::styled("  · esc latest", dim()));
    }
    let mut lines = vec![split_line(left, vec![Span::styled(when, dim())], width)];
    if job.outcome == "failed" {
        for p in problems.iter().take(2) {
            let mut spans = vec![];
            if !p.place.is_empty() {
                spans.push(Span::styled(format!("{}  ", p.place), bold()));
            }
            spans.push(Span::styled(
                p.message.clone(),
                Style::default().fg(Color::Red),
            ));
            lines.push(split_line(spans, vec![], width));
            if let Some(hint) = p.hint {
                lines.push(split_line(
                    vec![Span::styled(format!("→ {hint}"), dim())],
                    vec![],
                    width,
                ));
                break;
            }
        }
    }
    while lines.len() < 3 {
        lines.push(Line::from(""));
    }
    lines.truncate(3);
    lines
}

fn draw_rule(app: &App, frame: &mut Frame, area: Rect) {
    let job = app.jobs.get(app.shown);
    let has_console = job.is_some_and(|j| j.phases.len() > 1);
    let tab = |name: &str, on: bool| {
        if on {
            Span::styled(format!(" {name} "), bold().add_modifier(Modifier::REVERSED))
        } else {
            Span::styled(format!(" {name} "), dim())
        }
    };
    let mut spans = vec![Span::styled("─", dim()), tab("Build", app.phase == 0)];
    if has_console {
        spans.push(tab("Console", app.phase == 1));
    }
    if app.problems_only && app.filter.is_empty() {
        spans.push(Span::styled(
            " · problems",
            Style::default().fg(Color::Yellow),
        ));
    }
    if !app.filter.is_empty() || app.typing {
        spans.push(Span::styled(
            format!(" /{}{}", app.filter, if app.typing { "▏" } else { "" }),
            Style::default().fg(Color::Cyan),
        ));
    }
    if !app.follow {
        spans.push(Span::styled(" · G follow", dim()));
    }
    let used: usize = spans.iter().map(|s| s.content.width()).sum();
    spans.push(Span::styled(
        format!(
            " {}",
            "─".repeat((area.width as usize).saturating_sub(used + 1))
        ),
        dim(),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_log(app: &App, frame: &mut Frame) {
    let area = app.log_area;
    if area.height == 0 {
        return;
    }
    let len = app.visible_len();
    let width = area.width.max(1) as usize;
    let height = area.height as usize;
    if len == 0 {
        let text = if app.jobs.is_empty() {
            ""
        } else if !app.filter.is_empty() {
            "no matching lines"
        } else if app.problems_only {
            "no problems · i shows the full log"
        } else if app.live.is_some() {
            "waiting for output…"
        } else {
            ""
        };
        frame.render_widget(Paragraph::new(Line::styled(text, dim())), area);
        return;
    }
    let (top, mut skip) = app.view_pos();
    let mut rows: Vec<Line> = Vec::with_capacity(height);
    let mut n = top;
    while rows.len() < height && n < len {
        let text = app.line_at(n);
        let style = line_style(text);
        for part in wrap(text, width)
            .into_iter()
            .skip(std::mem::take(&mut skip))
        {
            if rows.len() == height {
                break;
            }
            rows.push(Line::styled(part, style));
        }
        n += 1;
    }
    frame.render_widget(Paragraph::new(rows), area);
}

fn draw_footer(app: &App, frame: &mut Frame, area: Rect) {
    let primary_style = Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD);
    let (primary, ready) = if app.typing {
        (" enter keep · esc clear ".to_owned(), true)
    } else if app.live.is_some() {
        let attached = app.jobs.first().is_some_and(|j| j.attached.is_some());
        (
            if attached {
                " ■ x  Stop app ".to_owned()
            } else {
                " ■ x  Stop ".to_owned()
            },
            true,
        )
    } else {
        match app.selected_destination() {
            Some(d) if app.selected_scheme().is_some() => {
                (format!(" ▶ r  Run on {} ", d.name), true)
            }
            _ => (" ▶ r  Run ".to_owned(), false),
        }
    };
    let style = if ready {
        primary_style
    } else {
        dim().add_modifier(Modifier::REVERSED)
    };
    let secondary = if app.typing {
        ""
    } else if app.busy() {
        "tab console · i problems · / filter · ? keys"
    } else {
        "b build · t test · c clean · h history · ? keys"
    };
    let mut spans = vec![Span::styled(primary.clone(), style), Span::raw("   ")];
    let mut rest = secondary.to_owned();
    if let Some((toast, _)) = &app.toast {
        rest = toast.clone();
        spans.push(Span::styled(
            truncate(
                &rest,
                (area.width as usize).saturating_sub(primary.width() + 3),
            ),
            Style::default().fg(Color::Cyan),
        ));
    } else {
        spans.push(Span::styled(
            truncate(
                &rest,
                (area.width as usize).saturating_sub(primary.width() + 3),
            ),
            dim(),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn popup_rect(frame: &Frame, width: u16, height: u16) -> Rect {
    let screen = frame.area();
    let width = width.min(screen.width.saturating_sub(4)).max(10);
    let height = height.min(screen.height.saturating_sub(2)).max(3);
    Rect::new(
        screen.x + (screen.width - width) / 2,
        screen.y + (screen.height.saturating_sub(height)) / 3,
        width,
        height,
    )
}

fn draw_list(
    app: &mut App,
    frame: &mut Frame,
    title: &str,
    filter: Option<&str>,
    rows: Vec<(String, String, Style, bool)>,
    cursor: usize,
    footer: &str,
) {
    let screen = frame.area();
    let visible = (screen.height.saturating_sub(8) as usize).clamp(3, 18);
    let width = rows
        .iter()
        .map(|(l, r, _, _)| l.width() + r.width() + 8)
        .max()
        .unwrap_or(30)
        .clamp(40, 76) as u16;
    let extra = 2 + u16::from(filter.is_some()) + u16::from(!footer.is_empty());
    let rect = popup_rect(frame, width, visible.min(rows.len().max(1)) as u16 + extra);
    app.popup_area = rect;
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim())
        .title(Span::styled(format!(" {title} "), bold()));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let mut lines = vec![];
    if let Some(f) = filter {
        lines.push(if f.is_empty() {
            Line::styled("type to filter", dim())
        } else {
            Line::styled(format!("/{f}▏"), Style::default().fg(Color::Cyan))
        });
    }
    let room =
        (inner.height as usize).saturating_sub(lines.len() + usize::from(!footer.is_empty()));
    let first = cursor.saturating_sub(room.saturating_sub(1));
    let w = inner.width as usize;
    if rows.is_empty() {
        lines.push(Line::styled("nothing here", dim()));
    }
    for (n, (left, right, style, current)) in rows.into_iter().enumerate().skip(first).take(room) {
        let mark = if current { "● " } else { "  " };
        let text = truncate(
            &format!("{mark}{left}"),
            w.saturating_sub(right.width() + 1),
        );
        let pad = w.saturating_sub(text.width() + right.width());
        let selected = n == cursor;
        let line = Line::from(vec![
            Span::styled(text, style),
            Span::raw(" ".repeat(pad)),
            Span::styled(right, if selected { Style::default() } else { dim() }),
        ]);
        lines.push(if selected {
            line.style(Style::default().add_modifier(Modifier::REVERSED))
        } else {
            line
        });
    }
    if !footer.is_empty() {
        while lines.len() + 1 < inner.height as usize {
            lines.push(Line::from(""));
        }
        lines.push(Line::styled(footer.to_owned(), dim()));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_popup(app: &mut App, frame: &mut Frame, popup: Popup) {
    match popup {
        Popup::Schemes { cursor, filter } => {
            let rows = app
                .scheme_rows(&filter)
                .into_iter()
                .map(|i| {
                    let s = &app.schemes[i];
                    (
                        s.name.clone(),
                        if s.package {
                            "package".into()
                        } else {
                            String::new()
                        },
                        Style::default(),
                        app.scheme == Some(i),
                    )
                })
                .collect();
            draw_list(app, frame, "Scheme", Some(&filter), rows, cursor, "");
        }
        Popup::Devices { cursor, filter } => {
            let rows = app
                .device_rows(&filter)
                .into_iter()
                .map(|i| {
                    let d = &app.destinations[i];
                    let right = if d.is_mac() {
                        String::new()
                    } else if d.state == "Booted" {
                        format!("{} · booted", d.runtime)
                    } else {
                        d.runtime.clone()
                    };
                    let icon = if d.device {
                        "▯ "
                    } else if d.is_mac() {
                        "▭ "
                    } else {
                        "▢ "
                    };
                    (
                        format!("{icon}{}", d.name),
                        right,
                        if d.device { bold() } else { Style::default() },
                        app.destination == Some(i),
                    )
                })
                .collect();
            draw_list(
                app,
                frame,
                "Run on",
                Some(&filter),
                rows,
                cursor,
                "ctrl+r rename simulator",
            );
        }
        Popup::History { cursor } => {
            let rows = app
                .jobs
                .iter()
                .enumerate()
                .map(|(n, j)| {
                    let (e, w) = j.phases.first().map(|p| p.counts()).unwrap_or_default();
                    let mut facts = vec![];
                    if j.outcome != "running" {
                        facts.push(job::short(j.elapsed));
                    }
                    if e > 0 {
                        facts.push(job::count(e, "error"));
                    }
                    if w > 0 {
                        facts.push(job::count(w, "warning"));
                    }
                    let when =
                        job::when_at(j.started.naive_local(), chrono::Local::now().naive_local());
                    (
                        format!(
                            "{} {:<6} {}",
                            job::mark(&j.outcome, j.attached.is_some()),
                            j.kind,
                            when
                        ),
                        facts.join(" · "),
                        match j.outcome.as_str() {
                            "failed" => Style::default().fg(Color::Red),
                            "succeeded" | "exited" => Style::default().fg(Color::Green),
                            "stopped" => Style::default().fg(Color::Yellow),
                            _ => Style::default(),
                        },
                        n == app.shown,
                    )
                })
                .collect();
            draw_list(
                app,
                frame,
                "History",
                None,
                rows,
                cursor,
                "enter open · D delete all",
            );
        }
        Popup::Rename { text, .. } => {
            let rect = popup_rect(frame, 50, 4);
            app.popup_area = rect;
            frame.render_widget(Clear, rect);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(format!("{text}▏")),
                    Line::styled("enter rename · esc cancel", dim()),
                ])
                .block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .border_style(dim())
                        .title(Span::styled(" Rename simulator ", bold())),
                ),
                rect,
            );
        }
        Popup::ConfirmDelete => {
            let n = app.jobs.len() - usize::from(app.busy());
            let rect = popup_rect(frame, 40, 4);
            app.popup_area = rect;
            frame.render_widget(Clear, rect);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(format!("Delete {}?", job::count(n, "run"))),
                    Line::styled("y delete · any other key keeps them", dim()),
                ])
                .block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .border_style(dim()),
                ),
                rect,
            );
        }
        Popup::Help => {
            let mut lines: Vec<Line> = HELP
                .iter()
                .map(|(k, d)| {
                    Line::from(vec![
                        Span::styled(format!("{k:<16}"), bold()),
                        Span::raw(*d),
                    ])
                })
                .collect();
            lines.push(Line::from(""));
            lines.push(Line::styled(
                format!(
                    "{} parallel compile jobs · XCTUIRS_JOBS changes it",
                    runtime::parallel_jobs().map_or("xcodebuild's".into(), |n| n.to_string())
                ),
                dim(),
            ));
            let rect = popup_rect(frame, 62, lines.len() as u16 + 2);
            app.popup_area = rect;
            frame.render_widget(Clear, rect);
            frame.render_widget(
                Paragraph::new(lines).block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .border_style(dim())
                        .title(Span::styled(" Keys ", bold())),
                ),
                rect,
            );
        }
    }
}

// ── text helpers ─────────────────────────────────────────────────────────────

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}
fn contains(text: &str, filter: &str) -> bool {
    filter.is_empty() || text.to_lowercase().contains(&filter.to_lowercase())
}
fn contains_point(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.right() && y >= r.y && y < r.bottom()
}
/// The last carriage-return segment, as a terminal would show it.
fn clean(line: &str) -> &str {
    line.trim_end_matches('\r')
        .rsplit('\r')
        .next()
        .unwrap_or("")
}
fn line_style(text: &str) -> Style {
    match job::kind(text) {
        "error" => Style::default().fg(Color::Red),
        "warning" => Style::default().fg(Color::Yellow),
        _ if text.starts_with("▸ ") || text.starts_with("** ") => bold(),
        _ => Style::default(),
    }
}
fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    if width > 0 {
        out.push('…');
    }
    out
}
fn wrapped_rows(text: &str, width: usize) -> usize {
    let w: usize = text
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| c.width().unwrap_or(0))
        .sum();
    w.div_ceil(width.max(1)).max(1)
}
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = vec![];
    let mut line = String::new();
    let mut used = 0;
    for c in text.chars().filter(|c| !c.is_control()) {
        let w = c.width().unwrap_or(0);
        if used + w > width && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        line.push(c);
        used += w;
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn app() -> App {
        let (tx, rx) = mpsc::channel();
        // A real (empty) project folder, so revalidation sees it as unchanged.
        let dir = tempfile::tempdir().unwrap().keep();
        std::fs::create_dir_all(dir.join("Puzzle.xcodeproj")).unwrap();
        let project = Project::find(&dir).unwrap();
        App {
            root: dir,
            project,
            schemes: vec![
                Scheme {
                    name: "Puzzle".into(),
                    package: false,
                },
                Scheme {
                    name: "PuzzleKit".into(),
                    package: true,
                },
            ],
            destinations: vec![
                Destination {
                    name: "iPhone 17".into(),
                    runtime: "iOS 26.0".into(),
                    state: "Shutdown".into(),
                    udid: "SIM".into(),
                    device: false,
                },
                Destination {
                    name: "My iPhone".into(),
                    runtime: "iOS 26.0".into(),
                    state: String::new(),
                    udid: "DEV".into(),
                    device: true,
                },
            ],
            scheme: Some(0),
            destination: Some(1),
            schemes_loaded: true,
            destinations_loaded: true,
            want_scheme: String::new(),
            want_destination: String::new(),
            jobs: vec![],
            shown: 0,
            phase: 0,
            live: None,
            follow: true,
            top: 0,
            skip: 0,
            view: ViewCache::default(),
            diagnosis: None,
            problems_only: false,
            filter: String::new(),
            typing: false,
            popup: None,
            toast: None,
            tx,
            rx,
            background: vec![],
            log_area: Rect::new(1, 7, 78, 15),
            popup_area: Rect::default(),
            tick: 0,
            focused: true,
            last_check: Instant::now(),
            test_mode: true,
        }
    }
    fn press(app: &mut App, term: &mut Terminal<TestBackend>, code: KeyCode) {
        app.key(KeyEvent::from(code), term).unwrap();
    }
    fn screen(app: &mut App, term: &mut Terminal<TestBackend>) -> String {
        term.draw(|f| draw(app, f)).unwrap();
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }
    fn term() -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(80, 24)).unwrap()
    }

    #[test]
    fn idle_screen_has_one_primary_action() {
        let mut a = app();
        let mut t = term();
        let s = screen(&mut a, &mut t);
        assert!(s.contains("Puzzle"), "{s}");
        assert!(s.contains("◆ Puzzle"));
        assert!(s.contains("▯ My iPhone"));
        assert!(s.contains("▶ r  Run on My iPhone"));
        assert!(s.contains("Ready to run on My iPhone"));
    }

    #[test]
    fn build_shows_progress_then_failure_card() {
        let mut a = app();
        let mut t = term();
        press(&mut a, &mut t, KeyCode::Char('b'));
        assert!(a.busy());
        for line in [
            "Target dependency graph (2 targets)",
            "SwiftCompile normal arm64 /src/Puzzle/Board.swift (in target 'Puzzle' from project 'Puzzle')",
        ] {
            a.apply(Event::Line(line.into()));
        }
        let s = screen(&mut a, &mut t);
        assert!(s.contains("Compiling  Board.swift"), "{s}");
        assert!(s.contains("1 of 2 targets"), "{s}");
        assert!(s.contains('%'));
        assert!(s.contains("■ x  Stop"));
        for n in 0..40 {
            a.apply(Event::Line(format!(
                "Puzzle/W.swift:{n}:1: warning: unused"
            )));
        }
        a.apply(Event::Line(
            "/src/Puzzle/Board.swift:12:5: error: cannot find 'tile' in scope".into(),
        ));
        a.apply(Event::Line("error: exit status 65".into()));
        a.apply(Event::Done {
            code: 65,
            stopped: false,
        });
        assert!(!a.busy());
        let s = screen(&mut a, &mut t);
        assert!(s.contains("✗ Build failed"), "{s}");
        assert!(
            s.contains("Board.swift:12  cannot find 'tile' in scope"),
            "{s}"
        );
        assert!(s.contains("problems"), "{s}");
        // Errors lead the problems list and are on screen, even behind many warnings.
        assert!(!a.follow);
        assert!(s.contains("Puzzle/Board.swift:12:5: error"), "{s}");
    }

    #[test]
    fn run_switches_to_console_when_attached() {
        let mut a = app();
        let mut t = term();
        press(&mut a, &mut t, KeyCode::Char('r'));
        a.apply(Event::Line("** BUILD SUCCEEDED **".into()));
        a.apply(Event::State("installing".into()));
        let s = screen(&mut a, &mut t);
        assert!(s.contains("Installing on My iPhone"), "{s}");
        a.apply(Event::State("launching".into()));
        a.apply(Event::Attached("com.example.Puzzle".into()));
        a.apply(Event::Console("hello from the app".into()));
        assert_eq!(a.phase, 1);
        let s = screen(&mut a, &mut t);
        assert!(
            s.contains("Running com.example.Puzzle on My iPhone"),
            "{s}"
        );
        assert!(s.contains("hello from the app"));
        assert!(s.contains("Stop app"));
        press(&mut a, &mut t, KeyCode::Tab);
        assert_eq!(a.phase, 0);
        let s = screen(&mut a, &mut t);
        assert!(s.contains("BUILD SUCCEEDED"), "{s}");
    }

    #[test]
    fn pickers_filter_and_select() {
        let mut a = app();
        let mut t = term();
        press(&mut a, &mut t, KeyCode::Char('d'));
        // Physical devices are listed first.
        assert_eq!(a.device_rows(""), vec![1, 0]);
        for c in "17".chars() {
            press(&mut a, &mut t, KeyCode::Char(c));
        }
        let s = screen(&mut a, &mut t);
        assert!(s.contains("/17"), "{s}");
        press(&mut a, &mut t, KeyCode::Enter);
        assert_eq!(a.destination, Some(0));
        assert!(a.popup.is_none());
        press(&mut a, &mut t, KeyCode::Char('s'));
        press(&mut a, &mut t, KeyCode::Down);
        press(&mut a, &mut t, KeyCode::Enter);
        assert_eq!(a.scheme, Some(1));
    }

    #[test]
    fn run_without_a_device_opens_the_picker() {
        let mut a = app();
        let mut t = term();
        a.destination = None;
        press(&mut a, &mut t, KeyCode::Char('r'));
        assert!(!a.busy());
        assert!(matches!(a.popup, Some(Popup::Devices { .. })));
    }

    #[test]
    fn scrolling_pauses_and_resumes_follow() {
        let mut a = app();
        let mut t = term();
        press(&mut a, &mut t, KeyCode::Char('b'));
        for n in 0..500 {
            a.apply(Event::Line(format!("line {n}")));
        }
        screen(&mut a, &mut t);
        assert!(a.follow);
        press(&mut a, &mut t, KeyCode::Up);
        assert!(!a.follow);
        let s = screen(&mut a, &mut t);
        assert!(!s.contains("line 499"), "{s}");
        assert!(s.contains("G follow"));
        press(&mut a, &mut t, KeyCode::Char('G'));
        let s = screen(&mut a, &mut t);
        assert!(s.contains("line 499"));
        press(&mut a, &mut t, KeyCode::Char('g'));
        let s = screen(&mut a, &mut t);
        assert!(s.contains("line 0 "), "{s}");
        for _ in 0..100 {
            press(&mut a, &mut t, KeyCode::PageDown);
        }
        assert!(a.follow);
    }

    #[test]
    fn a_line_taller_than_the_screen_is_fully_reachable() {
        let mut a = app();
        let mut t = term();
        press(&mut a, &mut t, KeyCode::Char('b'));
        a.apply(Event::Line("before".into()));
        a.apply(Event::Line(format!("START{}END", "x".repeat(5000))));
        let s = screen(&mut a, &mut t);
        assert!(
            s.contains("END"),
            "following shows the end of the long line\n{s}"
        );
        assert!(!s.contains("START"));
        for _ in 0..200 {
            press(&mut a, &mut t, KeyCode::Up);
        }
        let s = screen(&mut a, &mut t);
        assert!(s.contains("before") && s.contains("START"), "{s}");
        press(&mut a, &mut t, KeyCode::Char('G'));
        assert!(screen(&mut a, &mut t).contains("END"));
    }

    #[test]
    fn history_opens_an_older_run() {
        let mut a = app();
        let mut t = term();
        press(&mut a, &mut t, KeyCode::Char('b'));
        a.apply(Event::Line("first".into()));
        a.apply(Event::Done {
            code: 0,
            stopped: false,
        });
        press(&mut a, &mut t, KeyCode::Char('c'));
        a.apply(Event::Done {
            code: 0,
            stopped: false,
        });
        press(&mut a, &mut t, KeyCode::Char('h'));
        press(&mut a, &mut t, KeyCode::Down);
        press(&mut a, &mut t, KeyCode::Enter);
        assert_eq!(a.shown, 1);
        let s = screen(&mut a, &mut t);
        assert!(s.contains("Build succeeded"), "{s}");
        assert!(s.contains("first"));
        press(&mut a, &mut t, KeyCode::Esc);
        assert_eq!(a.shown, 0);
    }

    #[test]
    fn large_logs_render_quickly() {
        let mut a = app();
        let mut t = term();
        press(&mut a, &mut t, KeyCode::Char('b'));
        for n in 0..200_000 {
            a.apply(Event::Line(format!(
                "CompileC /very/long/path/number/{n}/file.m normal arm64 objective-c (in target 'Kit' from project 'Pods')"
            )));
        }
        let began = Instant::now();
        for _ in 0..20 {
            screen(&mut a, &mut t);
        }
        assert!(
            began.elapsed() < Duration::from_millis(1500),
            "20 frames took {:?}",
            began.elapsed()
        );
        // Filter and problems views are cached, not rebuilt per frame/scroll.
        a.filter = "number/1999".into();
        screen(&mut a, &mut t);
        a.problems_only = true;
        a.filter.clear();
        a.apply(Event::Line("x.swift:1:1: error: boom".into()));
        screen(&mut a, &mut t);
        let began = Instant::now();
        for _ in 0..20 {
            press(&mut a, &mut t, KeyCode::Up);
            screen(&mut a, &mut t);
        }
        assert!(
            began.elapsed() < Duration::from_millis(1500),
            "20 problem-view frames took {:?}",
            began.elapsed()
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(wrap("abc def", 4), vec!["abc ", "def"]);
        assert_eq!(wrap("a界b", 2), vec!["a", "界", "b"]);
        assert_eq!(wrapped_rows("abcdefgh", 4), 2);
        assert_eq!(wrapped_rows("", 4), 1);
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(clean("50%\r100%"), "100%");
        assert_eq!(clock(125.0), "2:05");
        assert!(contains("My iPhone", "IPHONE"));
    }
}
