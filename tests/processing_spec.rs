use serde_yaml::Value;
use xctuirs::job::*;
fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}
fn job(v: &Value) -> Job {
    let mut j = Job::new(if text(v, "kind").is_empty() {
        "run"
    } else {
        text(v, "kind")
    });
    for (k, slot) in [
        ("outcome", &mut j.outcome),
        ("status", &mut j.status),
        ("build_outcome", &mut j.build_outcome),
        ("app", &mut j.app),
    ] {
        if v[k].is_string() {
            *slot = text(v, k).into()
        }
    }
    j.elapsed = v["elapsed_ms"].as_f64().unwrap_or(0.) / 1000.;
    j.build_elapsed = v["build_elapsed_ms"].as_f64().unwrap_or(0.) / 1000.;
    for n in 0..v["errors"].as_u64().unwrap_or(0) {
        j.line(format!("error: {n}"))
    }
    for n in 0..v["warnings"].as_u64().unwrap_or(0) {
        j.line(format!("warning: {n}"))
    }
    if v["attached"].as_bool() == Some(true) {
        j.attached = Some(chrono::Local::now())
    }
    j
}
#[test]
fn upstream_processing_43_cases() {
    let spec: Value = serde_yaml::from_str(include_str!("../spec/tests.yaml")).unwrap();
    let p = &spec["processing"];
    let mut n = 0;
    for c in p["kind"].as_sequence().unwrap() {
        assert_eq!(kind(text(c, "line")), text(c, "want"));
        n += 1
    }
    let c = &p["issues"];
    let mut phase = Phase::new("build");
    phase.lines = c["lines"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().into())
        .collect();
    assert_eq!(
        phase.issues(),
        c["want_issues"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(phase.counts(), (1, 1));
    assert_eq!(phase.lines.len(), 5);
    n += 1;
    for c in p["strip_paths"].as_sequence().unwrap() {
        assert_eq!(
            strip_paths(text(c, "line"), std::path::Path::new(text(c, "container"))),
            text(c, "want")
        );
        n += 1
    }
    for c in p["short"].as_sequence().unwrap() {
        assert_eq!(short(c["ms"].as_f64().unwrap() / 1000.), text(c, "want"));
        n += 1
    }
    for c in p["when"].as_sequence().unwrap() {
        let parse = |s: &str| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap();
        assert_eq!(
            when_at(parse(text(c, "started")), parse(text(c, "now"))),
            text(c, "want")
        );
        n += 1
    }
    for c in p["count"].as_sequence().unwrap() {
        assert_eq!(
            count(c["n"].as_u64().unwrap() as usize, text(c, "noun")),
            text(c, "want")
        );
        n += 1
    }
    for c in p["marks"].as_sequence().unwrap() {
        assert_eq!(
            mark(text(c, "outcome"), c["attached"].as_bool().unwrap_or(false)),
            text(c, "want")
        );
        n += 1
    }
    for group in [
        "summaries",
        "build_summaries",
        "console_summaries",
        "runs_rows",
    ] {
        for c in p[group].as_sequence().unwrap() {
            let mut j = job(c);
            let got = match group {
                "summaries" => j.summary(),
                "build_summaries" => j.build_summary(),
                "console_summaries" => {
                    if j.outcome == "running" {
                        j.attached = Some(chrono::Local::now())
                    }
                    j.console_summary()
                }
                _ => {
                    use chrono::TimeZone;
                    j.started = chrono::Local
                        .with_ymd_and_hms(2026, 9, 26, 10, 0, 0)
                        .unwrap();
                    j.row(c["console_row"].as_bool().unwrap_or(false))
                }
            };
            assert_eq!(got, text(c, "want"), "{group}: {c:?}");
            n += 1
        }
    }
    assert_eq!(n, 43);
    eprintln!("processing: {n}/43 upstream cases passed");
}
