use anyhow::{Result, bail};
use std::{
    io::{self, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use xctuirs::{
    job::Job,
    runtime::{self, Event},
    xcode::{self, Destination, Project, Scheme},
};
fn usage() -> i32 {
    eprintln!(
        "Usage:\n  xctuirs                                 open the TUI for the project in the current folder\n  xctuirs <scheme>                        same, with that scheme selected\n  xctuirs project                         print the .xcworkspace or .xcodeproj\n  xctuirs schemes                         list schemes and local package products\n  xctuirs destinations                    list run destinations\n  xctuirs build <scheme> [destination]\n  xctuirs test <scheme> [destination]\n  xctuirs clean <scheme> [destination]\n  xctuirs resolve\n  xctuirs settings <scheme> [destination]\n  xctuirs run <scheme> [destination]"
    );
    2
}
fn main() {
    let code = match run() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("xctui: {e:#}");
            1
        }
    };
    std::process::exit(code)
}
fn run() -> Result<i32> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|a| a == "--help" || a == "-h") {
        usage();
        return Ok(0);
    }
    if args.first().is_some_and(|a| a == "--version" || a == "-V") {
        println!("xctuirs {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    let root = std::env::current_dir()?;
    let Some(cmd) = args.first().map(String::as_str) else {
        xctuirs::tui::run(&root, None)?;
        return Ok(0);
    };
    let commands = [
        "project",
        "schemes",
        "destinations",
        "build",
        "test",
        "clean",
        "resolve",
        "settings",
        "run",
        "info",
    ];
    if !commands.contains(&cmd) {
        if args.len() == 1 {
            xctuirs::tui::run(&root, Some(cmd))?;
            return Ok(0);
        }
        return Ok(usage());
    }
    let action = ["build", "test", "clean", "run", "settings"].contains(&cmd);
    if (action && !(2..=3).contains(&args.len())) || (!action && args.len() != 1) {
        return Ok(usage());
    }
    if cmd == "destinations" {
        let list = xcode::destinations()?;
        let default = xcode::default_destination(&list);
        for (i, d) in list.iter().enumerate() {
            println!(
                "{}{}{}  {}",
                if Some(i) == default { "* " } else { "  " },
                d.label(),
                if d.device { " (device)" } else { "" },
                d.udid
            )
        }
        return Ok(0);
    }
    let p = Project::find(&root)?;
    if cmd == "project" {
        println!("{}", p.name.display());
        return Ok(0);
    }
    if cmd == "schemes" || cmd == "info" {
        if cmd == "info" {
            println!("{}", p.name.display())
        }
        let list = xcode::schemes(&p)?;
        let default = xcode::default_scheme(&p, &list);
        for (i, s) in list.iter().enumerate() {
            println!(
                "{}{}{}",
                if Some(i) == default { "* " } else { "  " },
                s.name,
                if s.package { " (package)" } else { "" }
            )
        }
        return Ok(0);
    }
    let (scheme, dest) = if cmd == "resolve" {
        (
            Scheme {
                name: String::new(),
                package: false,
            },
            Destination {
                name: String::new(),
                runtime: String::new(),
                state: String::new(),
                udid: String::new(),
                device: false,
            },
        )
    } else {
        let schemes = xcode::schemes(&p)?;
        let Some(s) = schemes.into_iter().find(|s| s.name == args[1]) else {
            bail!("scheme {:?} not found", args[1])
        };
        let list = xcode::destinations()?;
        let index = if let Some(name) = args.get(2) {
            list.iter()
                .position(|d| d.name == *name)
                .or_else(|| list.iter().position(|d| d.udid == *name))
        } else {
            xcode::default_destination(&list)
        };
        let Some(index) = index else {
            bail!(
                "destination {:?} not found",
                args.get(2).map(String::as_str).unwrap_or("default")
            )
        };
        (s, list[index].clone())
    };
    if cmd == "settings" {
        let s = xcode::build_settings(&p, &scheme, &dest)?;
        println!(
            "app: {}\nbundle: {}\nexecutable: {}",
            s.app_path.display(),
            s.bundle_id,
            s.executable.display()
        );
        return Ok(0);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let sig = stop.clone();
    ctrlc::set_handler(move || sig.store(true, Ordering::SeqCst))?;
    let mut job = Job::new(cmd);
    let task = runtime::start(cmd, p.clone(), scheme, dest);
    let prefix = format!("{}/", p.name.parent().unwrap_or(&root).display());
    let mut cancelled = false;
    let code = loop {
        if stop.load(Ordering::SeqCst) && !cancelled {
            task.cancel();
            cancelled = true;
        }
        match task.events.recv_timeout(Duration::from_millis(30)) {
            Ok(Event::Line(l)) | Ok(Event::Console(l)) => {
                let l = l.replace(&prefix, "");
                println!("{l}");
                io::stdout().flush()?;
                job.line(l);
            }
            Ok(Event::State(s)) => {
                if s == "installing" && job.build_outcome.is_empty() {
                    job.build_finished();
                    eprintln!("{}", job.build_summary());
                }
                job.status = s;
            }
            Ok(Event::Attached(app)) => {
                println!("▸ Launched {app}");
                io::stdout().flush()?;
                job.attach(app);
            }
            Ok(Event::Done { code, stopped }) => {
                job.finish(code, stopped || cancelled);
                break if stopped || cancelled {
                    130
                } else if job.attached.is_some() {
                    0
                } else {
                    code
                };
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => (),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                job.finish(1, cancelled);
                break if cancelled { 130 } else { 1 };
            }
        }
    };
    task.wait();
    eprintln!("{}", job.summary());
    Ok(code)
}
