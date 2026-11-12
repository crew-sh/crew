use super::*;

struct U {
    id: &'static str,
    features: &'static str,
    debuginfo: &'static str,
    deps: &'static [usize],
    mode: &'static str,
}

const fn u(id: &'static str, deps: &'static [usize]) -> U {
    U {
        id,
        features: "",
        debuginfo: "2",
        deps,
        mode: "build",
    }
}

fn graph(units: &[U]) -> UnitGraph {
    let items: Vec<String> = units
        .iter()
        .map(|u| {
            let deps: Vec<String> = u
                .deps
                .iter()
                .map(|d| format!(r#"{{"index":{d},"extern_crate_name":"d{d}","public":false,"noprelude":false}}"#))
                .collect();
            format!(
                r#"{{"pkg_id":"{}","target":{{"kind":["lib"],"crate_types":["lib"],"name":"x"}},
                "profile":{{"name":"dev","opt_level":"0","lto":"false","codegen_backend":null,"codegen_units":null,
                "debuginfo":{},"split_debuginfo":null,"debug_assertions":true,"overflow_checks":true,"rpath":false,
                "incremental":false,"panic":"unwind","strip":{{"deferred":"None"}}}},
                "platform":null,"mode":"{}","features":[{}],"dependencies":[{}]}}"#,
                u.id,
                u.debuginfo,
                u.mode,
                u.features,
                deps.join(",")
            )
        })
        .collect();
    serde_json::from_str(&format!(r#"{{"version":1,"units":[{}]}}"#, items.join(","))).unwrap()
}

const REG: &str = "registry+https://github.com/rust-lang/crates.io-index";

fn id(name: &str, version: &str) -> &'static str {
    Box::leak(format!("{REG}#{name}@{version}").into_boxed_str())
}

fn verdicts(here: &UnitGraph, there: &UnitGraph) -> Vec<(String, Verdict)> {
    compare(here, there, "1.99.1", "1.99.1")
        .units
        .into_iter()
        .map(|u| (u.package, u.verdict))
        .collect()
}

#[test]
fn package_ids_of_every_source_kind_are_read() {
    assert_eq!(
        package(&format!("{REG}#serde@1.0.228")),
        ("serde".into(), "1.0.228".into(), false)
    );
    assert_eq!(
        package("git+https://github.com/o/r?rev=abc#r@0.2.0"),
        ("r".into(), "0.2.0".into(), false)
    );
    assert_eq!(
        package("path+file:///w/crates/app#app@0.1.0"),
        ("app".into(), "0.1.0".into(), true)
    );
    assert_eq!(
        package("path+file:///w/crates/app#0.1.0"),
        ("app".into(), "0.1.0".into(), true)
    );
}

#[test]
fn a_unit_with_the_same_everything_is_shared_and_workspace_units_are_left_out() {
    let here = graph(&[
        u("path+file:///w/a#a@0.1.0", &[1]),
        u(id("serde", "1.0.0"), &[]),
    ]);
    let there = graph(&[u(id("serde", "1.0.0"), &[])]);
    assert_eq!(
        verdicts(&here, &there),
        [("serde".to_string(), Verdict::Shared)]
    );
}

#[test]
fn a_different_compiler_decides_every_unit_first() {
    let g = graph(&[u(id("serde", "1.0.0"), &[])]);
    let r = compare(&g, &g, "1.99.1 (abc)", "1.97.1 (def)");
    assert_eq!(
        r.units[0].verdict,
        Verdict::Rustc {
            here: "1.99.1 (abc)".into(),
            there: "1.97.1 (def)".into()
        }
    );
    assert_eq!(r.summary["rustc"], 1);
}

#[test]
fn absent_packages_other_versions_and_other_modes_are_told_apart() {
    let here = graph(&[
        u(id("only-here", "1.0.0"), &[]),
        u(id("serde", "1.0.0"), &[]),
        U {
            mode: "check",
            ..u(id("syn", "2.0.0"), &[])
        },
    ]);
    let there = graph(&[
        u(id("serde", "1.0.5"), &[]),
        u(id("serde", "1.0.1"), &[]),
        u(id("syn", "2.0.0"), &[]),
    ]);
    let v = verdicts(&here, &there);
    assert_eq!(v[0].1, Verdict::NotUsed);
    assert_eq!(
        v[1].1,
        Verdict::LockVersion {
            there: vec!["1.0.1".into(), "1.0.5".into()]
        }
    );
    assert_eq!(v[2].1, Verdict::OtherMode);
}

#[test]
fn features_are_compared_as_sets_and_the_difference_is_named() {
    let here = graph(&[U {
        features: r#""std","derive""#,
        ..u(id("serde", "1.0.0"), &[])
    }]);
    let same = graph(&[U {
        features: r#""derive","std""#,
        ..u(id("serde", "1.0.0"), &[])
    }]);
    assert_eq!(verdicts(&here, &same)[0].1, Verdict::Shared);
    let other = graph(&[U {
        features: r#""std","alloc""#,
        ..u(id("serde", "1.0.0"), &[])
    }]);
    assert_eq!(
        verdicts(&here, &other)[0].1,
        Verdict::Features {
            only_here: vec!["derive".into()],
            only_there: vec!["alloc".into()]
        }
    );
}

#[test]
fn a_profile_field_that_differs_is_named_with_both_values() {
    let here = graph(&[u(id("serde", "1.0.0"), &[])]);
    let there = graph(&[U {
        debuginfo: "0",
        ..u(id("serde", "1.0.0"), &[])
    }]);
    assert_eq!(
        verdicts(&here, &there)[0].1,
        Verdict::ProfileField {
            field: "debuginfo".into(),
            here: "2".into(),
            there: "0".into()
        }
    );
}

#[test]
fn a_unit_whose_dependency_is_not_shared_points_at_it() {
    let here = graph(&[
        u(id("serde", "1.0.0"), &[1]),
        u(id("serde_derive", "1.0.0"), &[]),
    ]);
    let there = graph(&[
        u(id("serde", "1.0.0"), &[1]),
        U {
            debuginfo: "0",
            ..u(id("serde_derive", "1.0.0"), &[])
        },
    ]);
    let v = verdicts(&here, &there);
    assert_eq!(
        v[0].1,
        Verdict::Dependency {
            name: "serde_derive".into()
        }
    );
    assert!(matches!(v[1].1, Verdict::ProfileField { .. }));
}

#[test]
fn an_extra_dependency_over_there_is_a_different_dependency_set() {
    let here = graph(&[u(id("a", "1.0.0"), &[1]), u(id("b", "1.0.0"), &[])]);
    let there = graph(&[
        u(id("a", "1.0.0"), &[1, 2]),
        u(id("b", "1.0.0"), &[]),
        u(id("c", "1.0.0"), &[]),
    ]);
    assert_eq!(verdicts(&here, &there)[0].1, Verdict::DependencySet);
}

#[test]
fn the_text_report_counts_reasons_and_lists_every_unit() {
    let here = graph(&[u(id("serde", "1.0.0"), &[]), u(id("syn", "2.0.0"), &[])]);
    let there = graph(&[u(id("serde", "1.0.0"), &[])]);
    let text = render(&compare(&here, &there, "1.99.1", "1.99.1"), false);
    assert!(
        text.contains("1 of 2 dependency units are shared"),
        "{text}"
    );
    assert!(text.contains("   1  not used there"), "{text}");
    assert!(text.contains("serde 1.0.0 (lib build): shared"), "{text}");
    assert!(
        text.contains("syn 2.0.0 (lib build): not used by the other project"),
        "{text}"
    );
    assert!(!render(&compare(&here, &there, "a", "a"), true).contains("syn 2.0.0 (lib"));
}

#[test]
fn rustc_versions_use_the_commit_when_there_is_one() {
    let v = "rustc 1.99.1 (3a4b5c6d7 2026-10-01)\nbinary: rustc\ncommit-hash: 3a4b5c6d7e8f\nhost: x86_64-unknown-linux-gnu\nrelease: 1.99.1\n";
    assert_eq!(
        parse_rustc_version(v).unwrap(),
        "1.99.1 (3a4b5c6d7) x86_64-unknown-linux-gnu"
    );
    assert!(parse_rustc_version("nonsense").is_none());
}

#[test]
fn a_real_unit_graph_line_from_cargo_parses() {
    let text = r#"{"version":1,"units":[{"pkg_id":"path+file:///t#probe@0.1.0","target":{"kind":["bin"],"crate_types":["bin"],"name":"probe","src_path":"/t/src/main.rs","edition":"2021","doc":true,"doctest":false,"test":true},"profile":{"name":"dev","opt_level":"0","lto":"false","codegen_backend":null,"codegen_units":null,"debuginfo":2,"split_debuginfo":null,"debug_assertions":true,"overflow_checks":true,"rpath":false,"incremental":false,"panic":"unwind","strip":{"deferred":"None"}},"platform":null,"mode":"build","features":[],"dependencies":[]}],"roots":[0]}"#;
    let g: UnitGraph = serde_json::from_str(text).unwrap();
    assert_eq!(g.units.len(), 1);
    assert_eq!(g.units[0].profile.debuginfo, Some(Setting::Number(2)));
    assert!(!g.units[0].is_dependency());
}
