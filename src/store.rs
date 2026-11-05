use crate::{
    job::{Job, Phase},
    xcode::Project,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::{collections::BTreeMap, fs, path::PathBuf};
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Prefs {
    pub scheme: String,
    pub simulator: String,
}
fn config() -> Option<PathBuf> {
    // HOME is deliberately respected so isolated terminal sessions never touch real history.
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support/xctui"))
}
fn history_dir(p: &Project) -> Option<PathBuf> {
    let digest = Sha1::digest(p.name.to_string_lossy().as_bytes());
    let tag = digest[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    Some(
        config()?
            .join("runs")
            .join(format!("{}-{tag}", p.name.file_stem()?.to_string_lossy())),
    )
}
pub fn load_prefs(p: &Project) -> Prefs {
    let prefs: Option<BTreeMap<String, Prefs>> = config()
        .and_then(|c| fs::read(c.join("prefs.json")).ok())
        .and_then(|b| serde_json::from_slice(&b).ok());
    prefs
        .and_then(|mut m| m.remove(&p.name.to_string_lossy().into_owned()))
        .unwrap_or_default()
}
pub fn save_prefs(p: &Project, prefs: &Prefs) -> Result<()> {
    let Some(c) = config() else { return Ok(()) };
    fs::create_dir_all(&c)?;
    let path = c.join("prefs.json");
    let mut all: BTreeMap<String, Prefs> = fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    all.insert(p.name.to_string_lossy().into_owned(), prefs.clone());
    atomic_write(&path, &serde_json::to_vec_pretty(&all)?)
}
fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temp, bytes)?;
    fs::rename(&temp, path)?;
    Ok(())
}
pub fn save_job(p: &Project, j: &Job) -> Result<()> {
    let Some(root) = history_dir(p) else {
        return Ok(());
    };
    let dir = root.join(format!(
        "{}-{}",
        j.started.format("%Y-%m-%dT%H-%M-%S.%9f"),
        j.kind
    ));
    fs::create_dir_all(&dir)?;
    let phases: Vec<_> = j
        .phases
        .iter()
        .map(|p| {
            let (e, w) = p.counts();
            serde_json::json!({"Name":p.name,"Errors":e,"Warnings":w})
        })
        .collect();
    let meta = serde_json::json!({"Kind":j.kind,"Status":j.status,"Outcome":j.outcome,"BuildOutcome":j.build_outcome,"App":j.app,"Started":j.started.to_rfc3339(),"Attached":j.attached.map(|d|d.to_rfc3339()).unwrap_or_else(||"0001-01-01T00:00:00Z".into()),"Elapsed":(j.elapsed*1e9) as u64,"BuildElapsed":(j.build_elapsed*1e9) as u64,"Phases":phases});
    for phase in &j.phases {
        let mut phase = phase.clone();
        phase.ensure_loaded();
        fs::write(
            dir.join(format!("{}.log", phase.name)),
            format!("{}\n", phase.lines.join("\n")),
        )?;
    }
    atomic_write(&dir.join("meta.json"), &serde_json::to_vec_pretty(&meta)?)?;
    let mut entries = fs::read_dir(root)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.path())
        .collect::<Vec<_>>();
    entries.sort();
    entries.reverse();
    for old in entries.into_iter().skip(50) {
        fs::remove_dir_all(old)?;
    }
    Ok(())
}
pub fn load_history(p: &Project) -> Vec<Job> {
    let Some(root) = history_dir(p) else {
        return vec![];
    };
    let Ok(entries) = fs::read_dir(root) else {
        return vec![];
    };
    let mut dirs = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect::<Vec<_>>();
    dirs.sort();
    dirs.reverse();
    dirs.into_iter()
        .filter_map(|d| load_job(d).ok())
        .take(50)
        .collect()
}
fn load_job(dir: PathBuf) -> Result<Job> {
    let v: serde_json::Value = serde_json::from_slice(&fs::read(dir.join("meta.json"))?)?;
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    let mut phases = vec![];
    for p in v["Phases"].as_array().context("no phases")? {
        let name = p["Name"].as_str().context("missing phase name")?;
        anyhow::ensure!(
            ["build", "test", "clean", "resolve", "console"].contains(&name),
            "invalid phase name"
        );
        phases.push(Phase::from_history(
            name,
            dir.join(format!("{name}.log")),
            p["Errors"].as_u64().unwrap_or(0) as usize,
            p["Warnings"].as_u64().unwrap_or(0) as usize,
        ));
    }
    anyhow::ensure!(!phases.is_empty(), "no phases");
    Ok(Job {
        kind: s("Kind"),
        status: s("Status"),
        outcome: s("Outcome"),
        build_outcome: s("BuildOutcome"),
        app: s("App"),
        started: chrono::DateTime::parse_from_rfc3339(&s("Started"))?.with_timezone(&chrono::Local),
        attached: chrono::DateTime::parse_from_rfc3339(&s("Attached"))
            .ok()
            .filter(|d| d.timestamp() > 0)
            .map(|d| d.with_timezone(&chrono::Local)),
        elapsed: v["Elapsed"].as_u64().unwrap_or(0) as f64 / 1e9,
        build_elapsed: v["BuildElapsed"].as_u64().unwrap_or(0) as f64 / 1e9,
        phases,
    })
}
pub fn delete_history(p: &Project) -> Result<()> {
    if let Some(dir) = history_dir(p) {
        match fs::remove_dir_all(dir) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

type Estimates = BTreeMap<String, BTreeMap<String, crate::progress::Estimate>>;
fn load_estimates(c: &std::path::Path) -> Estimates {
    fs::read(c.join("estimates.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}
/// Duration and task count of the last successful build for `key` (scheme + destination + action).
pub fn load_estimate(p: &Project, key: &str) -> Option<crate::progress::Estimate> {
    let c = config()?;
    load_estimates(&c)
        .remove(&p.name.to_string_lossy().into_owned())?
        .remove(key)
}
pub fn save_estimate(p: &Project, key: &str, estimate: crate::progress::Estimate) -> Result<()> {
    let Some(c) = config() else { return Ok(()) };
    fs::create_dir_all(&c)?;
    let mut all = load_estimates(&c);
    all.entry(p.name.to_string_lossy().into_owned())
        .or_default()
        .insert(key.to_owned(), estimate);
    atomic_write(&c.join("estimates.json"), &serde_json::to_vec_pretty(&all)?)
}
