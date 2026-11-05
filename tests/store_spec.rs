//! Storage expectations from upstream persistence fixtures; UI steps are tested in tui.rs.
use serde_json::Value;
use std::{collections::BTreeSet, fs};
use xctuirs::{
    job::Job,
    store::{self, Prefs},
    xcode::Project,
};
fn keys(v: &Value) -> BTreeSet<String> {
    v.as_object().unwrap().keys().cloned().collect()
}
fn expected(v: &serde_yaml::Value) -> BTreeSet<String> {
    v.as_sequence()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}
#[test]
fn upstream_persistence_storage_fixtures() {
    use chrono::TimeZone;
    let spec: serde_yaml::Value = serde_yaml::from_str(include_str!("../spec/tests.yaml")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("HOME", dir.path()) };
    let project = Project {
        name: "/src/Puzzle.xcodeproj".into(),
        root: "/src".into(),
    };
    store::save_prefs(
        &project,
        &Prefs {
            scheme: "PuzzleKit".into(),
            simulator: "B".into(),
        },
    )
    .unwrap();
    let base = dir.path().join("Library/Application Support/xctui");
    assert_eq!(
        fs::read_to_string(base.join("prefs.json")).unwrap(),
        spec["persistence"]["prefs"]["file"].as_str().unwrap()
    );
    let prefs = store::load_prefs(&project);
    assert_eq!(
        prefs.scheme,
        spec["persistence"]["prefs"]["restart_restores"]["scheme"]
            .as_str()
            .unwrap()
    );
    assert_eq!(prefs.simulator, "B");
    let h = &spec["persistence"]["history"];
    let start = chrono::Local
        .with_ymd_and_hms(2026, 9, 26, 10, 0, 0)
        .unwrap();
    let mut build = Job::new("build");
    build.started = start;
    build.line("CompileSwift".into());
    build.line("Puzzle/A.swift:1:1: warning: unused".into());
    build.line("** BUILD SUCCEEDED **".into());
    build.finish(0, false);
    build.elapsed = 2.;
    store::save_job(&project, &build).unwrap();
    let root = fs::read_dir(base.join("runs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let build_dir = fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
    assert!(
        build_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("2026-09-26T10-00-00.000000000-build")
    );
    assert_eq!(
        fs::read_dir(&build_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<BTreeSet<_>>(),
        expected(&h["files"])
    );
    let meta: Value =
        serde_json::from_slice(&fs::read(build_dir.join("meta.json")).unwrap()).unwrap();
    assert_eq!(keys(&meta), expected(&h["meta_keys"]));
    assert_eq!(keys(&meta["Phases"][0]), expected(&h["phase_keys"]));
    assert_eq!(
        fs::read_to_string(build_dir.join("build.log")).unwrap(),
        format!("{}\n", build.phases[0].lines.join("\n"))
    );
    let mut run = Job::new("run");
    run.started = start + chrono::Duration::seconds(3);
    run.line("build two".into());
    run.build_finished();
    run.attach("com.example.Puzzle".into());
    run.line("10:00:05.000 hello from the app".into());
    run.finish(0, false);
    run.elapsed = 2.;
    store::save_job(&project, &run).unwrap();
    let mut jobs = store::load_history(&project);
    assert_eq!(
        jobs.iter().map(|j| j.kind.clone()).collect::<Vec<_>>(),
        h["reload"]["after_restart"]["jobs"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|j| j.as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        format!("last time: {}", jobs[0].summary()),
        h["reload"]["after_restart"]["status_row"].as_str().unwrap()
    );
    assert_eq!(jobs[1].phases[0].counts().1, 1);
    assert!(!jobs[1].phases[0].is_loaded());
    jobs[0].phases[1].ensure_loaded();
    assert!(
        jobs[0].phases[1].lines[0].ends_with(
            h["reload"]["after_restart"]["console_log_line_suffix"]
                .as_str()
                .unwrap()
        )
    );
    fs::remove_file(build_dir.join("build.log")).unwrap();
    jobs[1].phases[0].ensure_loaded();
    assert!(jobs[1].phases[0].lines[0].starts_with("(log missing: "));
    assert_eq!(jobs[1].phases[0].counts().1, 1);
    assert_eq!(h["keep"].as_u64(), Some(50)); // Retention exercised with 55 saves in store_roundtrip.rs.
    println!(
        "upstream persistence: prefs/history storage expectations passed; UI steps tested separately"
    );
}
