use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Phase {
    pub name: String,
    pub lines: Vec<String>,
    #[serde(skip)]
    lazy_path: Option<std::path::PathBuf>,
    #[serde(skip)]
    saved_counts: Option<(usize, usize)>,
    /// Incremental issue index so large logs never need a full rescan per frame.
    #[serde(skip)]
    index: IssueIndex,
}
#[derive(Clone, Debug, Default)]
struct IssueIndex {
    indexed: usize,
    issues: Vec<usize>,
    seen: HashSet<String>,
    errors: usize,
    warnings: usize,
}
impl Phase {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            lines: vec![],
            lazy_path: None,
            saved_counts: None,
            index: IssueIndex::default(),
        }
    }
    pub fn from_history(
        name: &str,
        path: std::path::PathBuf,
        errors: usize,
        warnings: usize,
    ) -> Self {
        Self {
            name: name.into(),
            lines: vec![],
            lazy_path: Some(path),
            saved_counts: Some((errors, warnings)),
            index: IssueIndex::default(),
        }
    }
    pub fn is_loaded(&self) -> bool {
        self.lazy_path.is_none()
    }
    pub fn ensure_loaded(&mut self) {
        if let Some(path) = self.lazy_path.take() {
            self.lines = match std::fs::read_to_string(path) {
                Ok(text) => text
                    .split_terminator('\n')
                    .map(|l| l.trim_end_matches('\r').to_owned())
                    .collect(),
                Err(e) => vec![format!("(log missing: {e})")],
            };
            self.index = IssueIndex::default();
            self.reindex();
        }
    }
    fn reindex(&mut self) {
        if self.index.indexed > self.lines.len() {
            self.index = IssueIndex::default();
        }
        while self.index.indexed < self.lines.len() {
            let i = self.index.indexed;
            let line = &self.lines[i];
            let k = kind(line);
            if k != "plain" && self.index.seen.insert(line.clone()) {
                self.index.issues.push(i);
                if k == "error" {
                    self.index.errors += 1;
                } else {
                    self.index.warnings += 1;
                }
            }
            self.index.indexed += 1;
        }
    }
    pub fn push(&mut self, line: String) {
        self.ensure_loaded();
        self.saved_counts = None;
        self.reindex();
        self.lines.push(line);
        self.reindex();
    }
    /// Line numbers of distinct errors and warnings, in log order.
    pub fn issue_indices(&self) -> std::borrow::Cow<'_, [usize]> {
        if self.index.indexed == self.lines.len() {
            return std::borrow::Cow::Borrowed(&self.index.issues);
        }
        let mut seen = HashSet::new();
        std::borrow::Cow::Owned(
            self.lines
                .iter()
                .enumerate()
                .filter(|(_, l)| kind(l) != "plain" && seen.insert(l.as_str()))
                .map(|(i, _)| i)
                .collect(),
        )
    }
    pub fn issues(&self) -> Vec<String> {
        self.issue_indices()
            .iter()
            .map(|&i| self.lines[i].clone())
            .collect()
    }
    pub fn counts(&self) -> (usize, usize) {
        if let Some(counts) = self.saved_counts {
            return counts;
        }
        if self.index.indexed == self.lines.len() {
            return (self.index.errors, self.index.warnings);
        }
        let issues = self.issues();
        (
            issues.iter().filter(|l| kind(l) == "error").count(),
            issues.iter().filter(|l| kind(l) == "warning").count(),
        )
    }
}
pub fn kind(line: &str) -> &'static str {
    if line.contains("error: ") {
        "error"
    } else if line.contains("warning: ") {
        "warning"
    } else {
        "plain"
    }
}
pub fn short(seconds: f64) -> String {
    let s = seconds.max(0.0).round() as u64;
    if s >= 3600 {
        format!("{}h {}m {}s", s / 3600, s / 60 % 60, s % 60)
    } else if s >= 60 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}
pub fn count(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}
pub fn mark(outcome: &str, attached: bool) -> &'static str {
    match outcome {
        "succeeded" | "exited" => "✓",
        "failed" => "✗",
        "stopped" => "■",
        _ if attached => "●",
        _ => "⟳",
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub kind: String,
    pub status: String,
    pub outcome: String,
    pub build_outcome: String,
    pub app: String,
    pub started: DateTime<Local>,
    pub attached: Option<DateTime<Local>>,
    pub elapsed: f64,
    pub build_elapsed: f64,
    pub phases: Vec<Phase>,
}
impl Job {
    pub fn new(kind: &str) -> Self {
        Self {
            kind: kind.into(),
            status: if kind == "run" { "building" } else { "running" }.into(),
            outcome: "running".into(),
            build_outcome: String::new(),
            app: String::new(),
            started: Local::now(),
            attached: None,
            elapsed: 0.,
            build_elapsed: 0.,
            phases: vec![Phase::new(if kind == "run" { "build" } else { kind })],
        }
    }
    pub fn line(&mut self, line: String) {
        if let Some(p) = self.phases.last_mut() {
            p.push(line)
        }
    }
    pub fn build_finished(&mut self) {
        self.build_outcome = "succeeded".into();
        self.build_elapsed = self.age();
        self.status = "installing".into();
    }
    pub fn attach(&mut self, app: String) {
        if self.build_outcome.is_empty() {
            self.build_finished();
        }
        self.app = app;
        self.attached = Some(Local::now());
        self.status = "attached".into();
        self.phases.push(Phase::new("console"));
    }
    pub fn age(&self) -> f64 {
        (Local::now() - self.started).num_milliseconds().max(0) as f64 / 1000.
    }
    pub fn finish(&mut self, code: i32, stopped: bool) {
        self.elapsed = self.age();
        self.outcome = if stopped {
            "stopped"
        } else if self.attached.is_some() {
            "exited"
        } else if code == 0 {
            "succeeded"
        } else {
            "failed"
        }
        .into();
        if self.build_outcome.is_empty() {
            self.build_outcome = self.outcome.clone();
            self.build_elapsed = self.elapsed;
        }
    }
    pub fn summary(&self) -> String {
        let m = mark(
            &self.outcome,
            self.attached.is_some() || self.status == "attached",
        );
        if self.outcome == "running" {
            return format!(
                "{m} {} {}",
                self.kind,
                if self.status.is_empty() {
                    "running"
                } else {
                    &self.status
                }
            );
        }
        let (e, w) = self.phases.first().map(Phase::counts).unwrap_or_default();
        format!(
            "{m} {} {} {} · {} · {}",
            self.kind,
            self.outcome,
            short(self.elapsed),
            count(e, "error"),
            count(w, "warning")
        )
    }
    pub fn build_summary(&self) -> String {
        if self.kind != "run" {
            return self.summary();
        }
        if self.build_outcome.is_empty() {
            return "⟳ run building".into();
        }
        let (e, w) = self.phases.first().map(Phase::counts).unwrap_or_default();
        format!(
            "{} run build {} {} · {} · {}",
            mark(&self.build_outcome, false),
            self.build_outcome,
            short(self.build_elapsed),
            count(e, "error"),
            count(w, "warning")
        )
    }
    pub fn console_summary(&self) -> String {
        format!(
            "{} console {} · {}",
            mark(&self.outcome, self.attached.is_some()),
            self.app,
            if self.outcome == "running" {
                "attached"
            } else {
                &self.outcome
            }
        )
    }
    pub fn row(&self, console: bool) -> String {
        let (outcome, elapsed) = if self.kind == "run" && !console {
            (&*self.build_outcome, self.build_elapsed)
        } else {
            (&*self.outcome, self.elapsed)
        };
        let running = outcome.is_empty() || outcome == "running";
        let when = if self.started.date_naive() == Local::now().date_naive() {
            self.started.format("%H:%M").to_string()
        } else {
            self.started.format("%b %-d %H:%M").to_string()
        };
        format!(
            "{} {:<7}  {}  {}",
            mark(outcome, console && running),
            if console { "console" } else { &self.kind },
            when,
            if running {
                "…".into()
            } else {
                short(elapsed)
            }
        )
    }
}

pub fn strip_paths(line: &str, container: &std::path::Path) -> String {
    let prefix = format!(
        "{}/",
        container
            .parent()
            .unwrap_or(std::path::Path::new(""))
            .display()
    );
    line.replace(&prefix, "")
}
pub fn when_at(started: chrono::NaiveDateTime, now: chrono::NaiveDateTime) -> String {
    if started.date() == now.date() {
        started.format("%H:%M").to_string()
    } else {
        started.format("%b %-d %H:%M").to_string()
    }
}
