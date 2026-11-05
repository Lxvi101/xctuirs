//! Build progress estimated from xcodebuild's streamed task lines.
//!
//! xcodebuild prints no percentage. It does print the size of the target graph
//! ("Target dependency graph (27 targets)") and one header line per task
//! ("SwiftCompile normal arm64 … (in target 'App' from project 'App')").
//! Progress combines how many targets have started with, when an earlier build
//! of the same scheme and destination succeeded, how many tasks and seconds that
//! build took. The estimate only moves forward and stays below 100% until the
//! tool reports completion.
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, sync::OnceLock};

/// What an earlier successful build of the same scheme and destination took.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    pub tasks: usize,
    pub seconds: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Preparing,
    Resolving,
    Compiling,
    Linking,
    Resources,
    Script,
    Signing,
    Testing,
    Installing,
    Launching,
}
impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Preparing => "Preparing",
            Stage::Resolving => "Resolving packages",
            Stage::Compiling => "Compiling",
            Stage::Linking => "Linking",
            Stage::Resources => "Processing resources",
            Stage::Script => "Running script",
            Stage::Signing => "Signing",
            Stage::Testing => "Testing",
            Stage::Installing => "Installing",
            Stage::Launching => "Launching",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Progress {
    pub stage: Stage,
    /// File, script or target the newest task works on.
    pub detail: String,
    pub target: String,
    pub total_targets: usize,
    pub tasks: usize,
    pub tests_passed: usize,
    pub tests_failed: usize,
    /// Install percentage reported by devicectl/simctl, when it prints one.
    pub install: Option<f64>,
    estimate: Option<Estimate>,
    targets: HashSet<String>,
    tasks_in_target: usize,
    shown: f64,
}

fn task_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^([A-Z][A-Za-z0-9]+)\s(.*)\(in target '([^']+)' from project '[^']+'\)\s*$")
            .unwrap()
    })
}

impl Progress {
    pub fn new(estimate: Option<Estimate>) -> Self {
        Self {
            stage: Stage::Preparing,
            detail: String::new(),
            target: String::new(),
            total_targets: 0,
            tasks: 0,
            tests_passed: 0,
            tests_failed: 0,
            install: None,
            estimate: estimate.filter(|e| e.tasks > 0 && e.seconds > 0.0),
            targets: HashSet::new(),
            tasks_in_target: 0,
            shown: 0.0,
        }
    }
    pub fn has_estimate(&self) -> bool {
        self.estimate.is_some()
    }
    pub fn targets_started(&self) -> usize {
        self.targets.len()
    }

    /// Feed one line of tool output.
    pub fn line(&mut self, line: &str) {
        if matches!(self.stage, Stage::Installing | Stage::Launching) {
            static PCT: OnceLock<Regex> = OnceLock::new();
            let re = PCT.get_or_init(|| Regex::new(r"(\d{1,3}(?:\.\d+)?)\s?%").unwrap());
            if let Some(c) = re.captures(line)
                && let Ok(p) = c[1].parse::<f64>()
                && p <= 100.0
            {
                self.install = Some(p / 100.0);
            }
            return;
        }
        if let Some(rest) = line.strip_prefix("Target dependency graph (") {
            if let Some(n) = rest.split_whitespace().next().and_then(|n| n.parse().ok()) {
                self.total_targets = n;
            }
            return;
        }
        if line.starts_with("Resolve Package Graph")
            || line.starts_with("Fetching from")
            || line.starts_with("Resolving ")
        {
            if self.tasks == 0 {
                self.stage = Stage::Resolving;
            }
            return;
        }
        if line.starts_with("Test Case '") || line.starts_with("Test case '") {
            self.stage = Stage::Testing;
            if line.contains("' passed") || line.contains(" passed on ") {
                self.tests_passed += 1;
            } else if line.contains("' failed") || line.contains(" failed on ") {
                self.tests_failed += 1;
            }
            if let Some(name) = line.split('\'').nth(1) {
                self.detail = name.to_owned();
            }
            return;
        }
        let Some(c) = task_re().captures(line) else {
            return;
        };
        let kind = &c[1];
        let args = &c[2];
        let target = &c[3];
        self.tasks += 1;
        if self.targets.insert(target.to_owned()) || self.target != target {
            if self.target != target {
                self.tasks_in_target = 0;
            }
            self.target = target.to_owned();
        }
        self.tasks_in_target += 1;
        self.stage = stage_for(kind).unwrap_or(self.stage);
        if self.stage == Stage::Preparing || self.stage == Stage::Resolving {
            self.stage = Stage::Compiling;
        }
        self.detail = detail_for(kind, args).unwrap_or_default();
    }

    pub fn set_stage(&mut self, stage: Stage) {
        self.stage = stage;
        self.detail.clear();
        if matches!(stage, Stage::Installing | Stage::Launching) {
            self.install = None;
        }
    }

    /// Fraction of the build done, in 0.0..=0.99. Never moves backwards.
    pub fn fraction(&mut self, elapsed: f64) -> f64 {
        let raw = if let Some(e) = self.estimate {
            let by_tasks = self.tasks as f64 / e.tasks as f64;
            let by_time = elapsed / e.seconds;
            // Tasks track real work; time smooths long single tasks (a big
            // whole-module compile prints one line and then works for minutes).
            if self.tasks == 0 {
                (by_time * 0.5).min(0.05)
            } else {
                0.6 * by_tasks.min(1.0) + 0.4 * by_time.min(1.0)
            }
        } else if self.total_targets > 0 && !self.targets.is_empty() {
            let started = self.targets.len().min(self.total_targets) as f64;
            // Within the newest target, creep towards (but never reach) its end.
            let inner = 1.0 - 1.0 / (1.0 + self.tasks_in_target as f64 / 40.0);
            0.04 + 0.92 * ((started - 1.0 + inner * 0.9) / self.total_targets as f64)
        } else {
            // Before the graph is known: a slow, bounded start.
            (elapsed / 60.0).min(1.0) * 0.03
        };
        self.shown = self.shown.max(raw.clamp(0.0, 0.99));
        self.shown
    }

    /// Seconds left, only when an earlier build gives a basis for it.
    pub fn remaining(&mut self, elapsed: f64) -> Option<f64> {
        let e = self.estimate?;
        let f = self.fraction(elapsed);
        if f < 0.08 {
            return Some((e.seconds - elapsed).max(1.0));
        }
        let projected = elapsed / f;
        // Lean on the projection as the build advances.
        let blended = projected * f + e.seconds * (1.0 - f);
        Some((blended - elapsed).max(1.0))
    }
}

fn stage_for(kind: &str) -> Option<Stage> {
    Some(match kind {
        k if k.starts_with("Compile")
            && (k.contains("Asset") || k.contains("Storyboard") || k.contains("XIB")) =>
        {
            Stage::Resources
        }
        k if k.starts_with("Compile")
            || k.starts_with("Swift")
            || k.starts_with("Precompile")
            || k.starts_with("ScanDependencies")
            || k.starts_with("GenerateDSYM")
            || k == "MetalLink"
            || k == "EmitSwiftModule" =>
        {
            Stage::Compiling
        }
        "Ld" | "Libtool" | "CreateUniversalBinary" | "LinkStoryboards" => Stage::Linking,
        "CodeSign" | "ProcessProductPackaging" | "ProcessProductPackagingDER" => Stage::Signing,
        "PhaseScriptExecution" => Stage::Script,
        k if k.starts_with("Cp")
            || k.starts_with("Copy")
            || k.starts_with("Process")
            || k.starts_with("Generate")
            || k.starts_with("Write")
            || k == "Touch"
            || k == "Validate"
            || k == "RegisterExecutionPolicyException" =>
        {
            Stage::Resources
        }
        _ => return None,
    })
}

fn unescape(s: &str) -> String {
    s.replace("\\ ", " ")
}

fn detail_for(kind: &str, args: &str) -> Option<String> {
    if kind == "PhaseScriptExecution" {
        // PhaseScriptExecution [CP]\ Embed\ Pods\ Frameworks /path/Script.sh
        let name = args.split(" /").next()?.trim();
        return Some(unescape(name));
    }
    if let Some(rest) = args.split("Compiling\\ ").nth(1) {
        // Batch: "Compiling\ A.swift,\ B.swift /path/A.swift …"
        let first = rest.split(",\\ ").next()?.split(' ').next()?;
        let more = rest.split(" /").next()?.matches(",\\ ").count();
        return Some(if more > 0 {
            format!("{} +{more}", unescape(first))
        } else {
            unescape(first)
        });
    }
    // The first source-looking path in the arguments.
    let mut words = vec![];
    let mut word = String::new();
    let mut escaped = false;
    for c in args.chars() {
        if escaped {
            word.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == ' ' {
            words.push(std::mem::take(&mut word));
        } else {
            word.push(c);
        }
    }
    words.push(word);
    let source = [
        ".swift",
        ".m",
        ".mm",
        ".c",
        ".cpp",
        ".cc",
        ".metal",
        ".xcassets",
        ".storyboard",
        ".xib",
        ".plist",
        ".strings",
        ".xcstrings",
    ];
    if let Some(path) = words
        .iter()
        .filter(|w| w.starts_with('/'))
        .find(|w| source.iter().any(|s| w.ends_with(s)))
    {
        return path.rsplit('/').next().map(str::to_owned);
    }
    if matches!(kind, "Ld" | "CodeSign") {
        return words
            .iter()
            .find(|w| w.starts_with('/'))
            .and_then(|p| p.rsplit('/').next())
            .map(str::to_owned);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_graph_tasks_and_details() {
        let mut p = Progress::new(None);
        p.line("Resolve Package Graph");
        assert_eq!(p.stage, Stage::Resolving);
        p.line("note: Target dependency graph (3 targets)");
        p.line("Target dependency graph (3 targets)");
        assert_eq!(p.total_targets, 3);
        p.line("SwiftCompile normal arm64 /src/App/Views/Home\\ View.swift (in target 'App' from project 'App')");
        assert_eq!(p.stage, Stage::Compiling);
        assert_eq!(p.detail, "Home View.swift");
        assert_eq!(p.target, "App");
        p.line("SwiftCompile normal arm64 Compiling\\ A.swift,\\ B.swift,\\ C.swift /src/A.swift /src/B.swift (in target 'Kit' from project 'Pods')");
        assert_eq!(p.detail, "A.swift +2");
        p.line("PhaseScriptExecution [CP]\\ Embed\\ Pods\\ Frameworks /DD/Script-1.sh (in target 'App' from project 'App')");
        assert_eq!(p.stage, Stage::Script);
        assert_eq!(p.detail, "[CP] Embed Pods Frameworks");
        p.line("    cd /src");
        p.line("Ld /DD/Build/Products/Debug-iphoneos/App.app/App normal (in target 'App' from project 'App')");
        assert_eq!(p.stage, Stage::Linking);
        assert_eq!(p.detail, "App");
        p.line("CodeSign /DD/App.app (in target 'App' from project 'App')");
        assert_eq!(p.stage, Stage::Signing);
        assert_eq!(p.tasks, 5);
        assert_eq!(p.targets_started(), 2);
    }

    #[test]
    fn fraction_is_monotonic_and_capped() {
        let mut p = Progress::new(None);
        let a = p.fraction(10.0);
        assert!(a > 0.0 && a < 0.04);
        p.line("Target dependency graph (2 targets)");
        p.line("CompileC /o/a.o /src/a.m normal arm64 objective-c x (in target 'Pod' from project 'Pods')");
        let b = p.fraction(11.0);
        assert!(b > a);
        for _ in 0..10_000 {
            p.line("CompileC /o/a.o /src/a.m normal arm64 objective-c x (in target 'App' from project 'App')");
        }
        let c = p.fraction(12.0);
        assert!(c > b && c <= 0.99, "{c}");
        // A target switch never moves the bar back.
        p.line("Ld /x/Pod normal (in target 'Pod' from project 'Pods')");
        assert!(p.fraction(13.0) >= c);
    }

    #[test]
    fn estimate_drives_fraction_and_eta() {
        let mut p = Progress::new(Some(Estimate {
            tasks: 100,
            seconds: 100.0,
        }));
        assert!(p.has_estimate());
        assert_eq!(p.remaining(0.0), Some(100.0));
        for _ in 0..50 {
            p.line("SwiftCompile normal arm64 /s/a.swift (in target 'App' from project 'App')");
        }
        let f = p.fraction(50.0);
        assert!((f - 0.5).abs() < 0.01, "{f}");
        let left = p.remaining(50.0).unwrap();
        assert!((40.0..=60.0).contains(&left), "{left}");
        // Never 100% before the tool says so.
        for _ in 0..500 {
            p.line("SwiftCompile normal arm64 /s/a.swift (in target 'App' from project 'App')");
        }
        assert!(p.fraction(500.0) <= 0.99);
    }

    #[test]
    fn counts_tests_and_install_percent() {
        let mut p = Progress::new(None);
        p.line("Test Case '-[AppTests testA]' passed (0.001 seconds).");
        p.line("Test Case '-[AppTests testB]' failed (0.002 seconds).");
        p.line("Test case 'AppTests.testC()' passed on 'My Mac - App (1)' (0.000 seconds)");
        assert_eq!((p.tests_passed, p.tests_failed), (2, 1));
        assert_eq!(p.stage, Stage::Testing);
        p.set_stage(Stage::Installing);
        p.line("13:01:02  Installing app (45%)");
        assert_eq!(p.install, Some(0.45));
    }
}
