//! Runs the upstream discovery, scheme, destination and settings fixtures.
use serde_yaml::Value as Y;
use std::{
    fs,
    path::{Path, PathBuf},
};
use xctuirs::xcode::*;
fn text(v: &Y) -> &str {
    v.as_str().unwrap_or("")
}
fn list(v: &Y) -> Vec<String> {
    v.as_sequence()
        .unwrap()
        .iter()
        .map(|x| text(x).into())
        .collect()
}
fn fixture(v: &Y) -> String {
    fs::read_to_string(Path::new("spec").join(text(v))).unwrap()
}
fn tree(root: &Path, v: &Y) {
    if let Some(seq) = v.as_sequence() {
        for p in seq {
            let path = root.join(text(p));
            if text(p).ends_with('/') {
                fs::create_dir_all(path).unwrap()
            } else {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, "").unwrap()
            }
        }
    }
    if let Some(map) = v.as_mapping() {
        for (p, content) in map {
            let path = root.join(text(p));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text(content)).unwrap()
        }
    }
}
fn project(path: &str) -> Project {
    Project {
        name: path.into(),
        root: PathBuf::new(),
    }
}
fn dest(v: &Y) -> Destination {
    let mut d: Destination = serde_yaml::from_value(v.clone()).unwrap();
    if v["mac"].as_bool() == Some(true) {
        d.udid = "mac".into();
        d.runtime = "macOS".into()
    }
    d
}
fn dests(v: &Y) -> Vec<Destination> {
    v.as_sequence().unwrap().iter().map(dest).collect()
}
#[test]
fn upstream_xcode_spec_56_cases() {
    let spec: Y = serde_yaml::from_str(include_str!("../spec/tests.yaml")).unwrap();
    let mut count = 0;
    for c in spec["container"].as_sequence().unwrap() {
        let tmp = tempfile::tempdir().unwrap();
        tree(tmp.path(), &c["tree"]);
        let got = Project::find(tmp.path());
        if c["error"].is_string() {
            assert_eq!(
                got.unwrap_err().to_string(),
                text(&c["error"]).replace(
                    "<dir>",
                    &tmp.path().canonicalize().unwrap().to_string_lossy()
                )
            )
        } else {
            assert_eq!(
                got.unwrap().name,
                tmp.path().canonicalize().unwrap().join(text(&c["want"]))
            )
        }
        count += 1;
    }
    for c in spec["container_args"].as_sequence().unwrap() {
        assert_eq!(
            project(text(&c["container"])).container_args(),
            list(&c["want"])
        );
        count += 1
    }
    let s = &spec["schemes"];
    let tmp = tempfile::tempdir().unwrap();
    tree(tmp.path(), &s["tree"]);
    for c in s["files_on_disk"].as_sequence().unwrap() {
        let p = Project {
            name: tmp.path().join(text(&c["container"])),
            root: tmp.path().into(),
        };
        assert_eq!(scheme_files(&p).unwrap(), list(&c["want"]));
        count += 1
    }
    assert_eq!(
        package_products(tmp.path()).unwrap(),
        list(&s["package_products"]["want"])
    );
    count += 1;
    for c in s["list_fallback"].as_sequence().unwrap() {
        assert_eq!(
            parse_scheme_list(&fixture(&c["fixture"])).unwrap(),
            list(&c["want"])
        );
        count += 1
    }
    for c in s["merge"].as_sequence().unwrap() {
        let got = merge_schemes(list(&c["listed"]), list(&c["products"]));
        let want: Vec<Scheme> = serde_yaml::from_value(c["want"].clone()).unwrap();
        assert_eq!(got, want);
        count += 1
    }
    for c in s["default"].as_sequence().unwrap() {
        let schemes = list(&c["schemes"])
            .into_iter()
            .map(|name| Scheme {
                name,
                package: false,
            })
            .collect::<Vec<_>>();
        let got = default_scheme(&project(text(&c["container"])), &schemes)
            .map(|i| schemes[i].name.as_str());
        assert_eq!(got, c["want"].as_str());
        count += 1
    }
    for c in s["display"].as_sequence().unwrap() {
        let scheme = Scheme {
            name: text(&c["name"]).into(),
            package: c["package"].as_bool().unwrap(),
        };
        assert_eq!(scheme.to_string(), text(&c["want"]));
        count += 1
    }
    let d = &spec["destinations"];
    for c in d["runtime_name"].as_sequence().unwrap() {
        assert_eq!(runtime_name(text(&c["id"])), text(&c["want"]));
        count += 1
    }
    for c in d["platform"].as_sequence().unwrap() {
        assert_eq!(dest(c).platform(), text(&c["want"]));
        count += 1
    }
    for c in d["destination_and_platform_name"].as_sequence().unwrap() {
        let got = dest(c);
        assert_eq!(got.destination(), text(&c["destination"]));
        assert_eq!(got.platform_name(), text(&c["platform_name"]));
        count += 1
    }
    for c in d["sort"].as_sequence().unwrap() {
        let mut got = dests(&c["input"]);
        sort_simulators(&mut got);
        assert_eq!(
            got.iter().map(|d| d.name.clone()).collect::<Vec<_>>(),
            list(&c["want"])
        );
        count += 1
    }
    for c in d["default"].as_sequence().unwrap() {
        let ds = dests(&c["input"]);
        let got = default_destination(&ds).map(|i| ds[i].name.as_str());
        assert_eq!(got, c["want"].as_str());
        count += 1
    }
    let c = &d["parse"];
    let mut got = vec![Destination {
        name: "My Mac".into(),
        runtime: "macOS".into(),
        udid: "mac".into(),
        ..Default::default()
    }];
    got.extend(parse_devices(&fixture(&c["devices"])).unwrap());
    got.extend(parse_simulators(&fixture(&c["fixture"])).unwrap());
    assert_eq!(got, dests(&c["want"]));
    count += 1;
    for c in d["labels"].as_sequence().unwrap() {
        let got = dest(c);
        assert_eq!(got.label(), text(&c["all"]));
        assert_eq!(got.label_for_platform(true), text(&c["one_platform"]));
        count += 1
    }
    for c in spec["build_settings"].as_sequence().unwrap() {
        let json = if c["fixture"].is_string() {
            fixture(&c["fixture"])
        } else {
            text(&c["json"]).into()
        };
        let got = parse_build_settings(&json, text(&c["platform"]));
        if c["error"].is_string() {
            assert_eq!(got.unwrap_err().to_string(), text(&c["error"]))
        } else {
            let got = got.unwrap();
            assert_eq!(got.app_path, PathBuf::from(text(&c["want"]["app"])));
            assert_eq!(got.bundle_id, text(&c["want"]["bundle"]));
            assert_eq!(
                got.executable,
                PathBuf::from(text(&c["want"]["executable"]))
            )
        }
        count += 1
    }
    assert_eq!(count, 56);
    println!("upstream xcode spec: {count}/56 cases passed");
}
#[test]
fn upstream_xcode_command_arguments() {
    let spec: Y = serde_yaml::from_str(include_str!("../spec/tests.yaml")).unwrap();
    let scheme = Scheme {
        name: "Puzzle".into(),
        package: false,
    };
    for c in spec["commands"]["xcodebuild_args"].as_sequence().unwrap() {
        assert_eq!(
            destination_args(&project(text(&c["container"])), &scheme, &dest(c)),
            list(&c["want"])
        )
    }
    let p = project("/src/Puzzle.xcodeproj");
    let d = Destination {
        udid: "A".into(),
        ..Default::default()
    };
    for c in spec["commands"]["actions"].as_sequence().unwrap() {
        let action = text(&c["job"]);
        let mut got = vec!["xcodebuild".into()];
        got.extend(if action == "resolve" {
            resolve_args(&p)
        } else {
            action_args(action, &p, &scheme, &d)
        });
        assert_eq!(got, list(&c["argv"]))
    }
}
