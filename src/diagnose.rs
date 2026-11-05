//! Turns a failed log into the one thing to look at first, plus a likely fix.
use regex::Regex;
use std::sync::OnceLock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// `File.swift:585` when the error names a source location.
    pub place: String,
    pub message: String,
    pub hint: Option<&'static str>,
}

fn location() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?:.*/)?([^/:\s][^/:]*):(\d+)(?::\d+)?:\s*(?:fatal )?error:\s*(.*)$").unwrap()
    })
}

const HINTS: &[(&str, &str)] = &[
    (
        "unable to type-check this expression in reasonable time",
        "Split that expression into smaller `let`s or add explicit types.",
    ),
    (
        "does not contain a scheme named",
        "The scheme isn't shared in this container. Press s to pick another, or share it in the project.",
    ),
    (
        "does not exist",
        "The project moved or was deleted. xctuirs looks for it again on the next build.",
    ),
    (
        "No profiles for",
        "Signing needs a profile. Sign in to your Apple account in Xcode › Settings › Accounts once.",
    ),
    (
        "requires a development team",
        "Set a development team for the target once (Signing & Capabilities).",
    ),
    (
        "No Account for Team",
        "Add the team's Apple account in Xcode › Settings › Accounts once.",
    ),
    (
        "Developer Mode",
        "Enable Developer Mode on the device: Settings › Privacy & Security › Developer Mode.",
    ),
    (
        "is locked",
        "Unlock the device and keep it awake, then run again.",
    ),
    (
        "passcode",
        "Unlock the device and keep it awake, then run again.",
    ),
    (
        "Unable to find a destination matching",
        "The device or simulator isn't available. Press d to choose another one.",
    ),
    (
        "not paired",
        "Connect the device by cable once and tap Trust.",
    ),
    (
        "Library '",
        "A dependency library is missing. With CocoaPods, run `pod install` and build the .xcworkspace.",
    ),
    (
        "No such module",
        "A dependency module is missing. Resolve packages (p) or run `pod install`.",
    ),
    (
        "Sandbox: rsync",
        "Turn off User Script Sandboxing (ENABLE_USER_SCRIPT_SANDBOXING) for the target.",
    ),
    (
        "free; xctuirs requires",
        "Free up disk space; clearing ~/Library/Developer/Xcode/DerivedData usually helps.",
    ),
    (
        "another xctuirs job is already using",
        "Another xctuirs window is building this project. Stop it there first.",
    ),
    (
        "Command SwiftCompile failed with a nonzero exit code",
        "The Swift compiler crashed or ran out of memory. Run again; if it repeats, close other apps.",
    ),
    (
        "Command PhaseScriptExecution failed",
        "A build script failed. Its output is just above that line in the log.",
    ),
];

fn hint_for(text: &str) -> Option<&'static str> {
    HINTS
        .iter()
        .find(|(needle, _)| text.contains(needle))
        .map(|(_, hint)| *hint)
}

/// The most useful problems first: source errors, then tool errors.
pub fn problems(lines: &[String], limit: usize) -> Vec<Problem> {
    let mut out: Vec<Problem> = vec![];
    let mut tool: Vec<Problem> = vec![];
    let mut seen = std::collections::HashSet::new();
    for line in lines {
        if out.len() >= limit {
            break; // enough source errors; tool errors would sort after them
        }
        let line = line.trim();
        if let Some(c) = location().captures(line) {
            let p = Problem {
                place: format!("{}:{}", &c[1], &c[2]),
                message: c[3].trim().to_owned(),
                hint: hint_for(&c[3]),
            };
            if seen.insert((p.place.clone(), p.message.clone())) {
                out.push(p);
            }
        } else if let Some(i) = line.find("error: ") {
            let message = line[i + 7..].trim();
            if message.starts_with("exit status") || message.is_empty() {
                continue;
            }
            let p = Problem {
                place: String::new(),
                message: message.to_owned(),
                hint: hint_for(message),
            };
            if tool.len() < limit && seen.insert((String::new(), p.message.clone())) {
                tool.push(p);
            }
        }
    }
    // Linker and signing failures print no source location but carry hints.
    if out.is_empty()
        && let Some(hint) = lines.iter().rev().find_map(|l| hint_for(l))
        && tool.iter().all(|p| p.hint.is_none())
        && let Some(first) = tool.first_mut()
    {
        first.hint = Some(hint);
    }
    out.extend(tool);
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn v(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn source_errors_lead_with_place_and_hint() {
        let p = problems(
            &v(&[
                "warning: something",
                "Puzzle/Views/BoardView.swift:585:16: error: the compiler is unable to type-check this expression in reasonable time; try breaking up the expression into distinct sub-expressions",
                "error: exit status 65",
            ]),
            3,
        );
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].place, "BoardView.swift:585");
        assert!(p[0].message.starts_with("the compiler is unable"));
        assert!(p[0].hint.unwrap().contains("Split"));
    }

    #[test]
    fn tool_errors_get_hints() {
        let p = problems(
            &v(&[
                "xcodebuild: error: The workspace named \"Puzzle\" does not contain a scheme named \"Puzzle\". The \"-list\" option can be used to find the names of the schemes in the workspace.",
                "error: exit status 65",
            ]),
            3,
        );
        assert_eq!(p.len(), 1);
        assert!(p[0].place.is_empty());
        assert!(p[0].hint.unwrap().contains("scheme"));
    }

    #[test]
    fn linker_hint_comes_from_nearby_line() {
        let p = problems(
            &v(&[
                "ld: warning: Library 'GTMSessionFetcher' not found",
                "clang: error: linker command failed with exit code 1 (use -v to see invocation)",
            ]),
            3,
        );
        assert!(p[0].hint.unwrap().contains("pod install"));
    }
}
