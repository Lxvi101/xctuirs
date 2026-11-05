use xctuirs::{
    job::Job,
    store::{self, Prefs},
    xcode::Project,
};
#[test]
fn history_and_preferences_roundtrip_retention_and_delete() {
    let dir = tempfile::tempdir().unwrap();
    // This integration-test executable has one test and never starts workers.
    unsafe { std::env::set_var("HOME", dir.path()) };
    let p = Project {
        name: "/src/Puzzle.xcodeproj".into(),
        root: "/src".into(),
    };
    let prefs = Prefs {
        scheme: "PuzzleKit".into(),
        simulator: "B".into(),
    };
    store::save_prefs(&p, &prefs).unwrap();
    assert_eq!(store::load_prefs(&p).scheme, "PuzzleKit");
    assert_eq!(store::load_prefs(&p).simulator, "B");
    let second = Project {
        name: "/src/Other.xcodeproj".into(),
        root: "/src".into(),
    };
    store::save_prefs(
        &second,
        &Prefs {
            scheme: "Other".into(),
            simulator: "mac".into(),
        },
    )
    .unwrap();
    assert_eq!(store::load_prefs(&p).scheme, "PuzzleKit");
    let mut j = Job::new("run");
    j.line("warning: unique".into());
    j.line("warning: unique".into());
    j.build_finished();
    j.attach("com.example.Puzzle".into());
    j.line("10:00:03.001 hello from the app".into());
    j.finish(0, false);
    store::save_job(&p, &j).unwrap();
    let mut got = store::load_history(&p);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].phases[0].counts(), (0, 1));
    assert!(!got[0].phases[1].is_loaded());
    got[0].phases[1].ensure_loaded();
    assert_eq!(got[0].phases[1].lines, j.phases[1].lines);
    assert_eq!(got[0].outcome, "exited");
    for i in 0..55 {
        let mut j = Job::new("build");
        j.started += chrono::Duration::seconds(i + 1);
        j.finish(0, false);
        store::save_job(&p, &j).unwrap()
    }
    assert_eq!(store::load_history(&p).len(), 50);
    let base = dir.path().join("Library/Application Support/xctui/runs");
    let project_dir = std::fs::read_dir(base)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::create_dir(project_dir.join("zzzz-corrupt")).unwrap();
    std::fs::write(project_dir.join("zzzz-corrupt/meta.json"), "{bad json").unwrap();
    assert_eq!(store::load_history(&p).len(), 50);
    store::delete_history(&p).unwrap();
    assert!(store::load_history(&p).is_empty());
    assert!(!project_dir.exists());
    assert_eq!(store::load_prefs(&p).scheme, "PuzzleKit");
}
