use std::path::PathBuf;

use crew_depot_config::{DepotTable, Lto, ProfileOverride};

use super::*;

fn words(cmd: &str) -> Vec<String> {
    cmd.split_whitespace().map(str::to_string).collect()
}

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn grant(cpus: u32, priority: BuildPriority) -> BuildGrant {
    BuildGrant {
        priority: priority as i32,
        compile_cpus: cpus,
    }
}

fn fast_depot() -> Depot {
    let mut t = DepotTable {
        toolchain: Some("1.99.1".into()),
        ..DepotTable::default()
    };
    t.profiles.insert(
        "fast".into(),
        ProfileOverride {
            lto: Some(Lto::Off),
            codegen_units: Some(256),
        },
    );
    t.resolve()
}

fn plan(
    depot: &Depot,
    args: &str,
    env: &BTreeMap<String, String>,
    mbx: bool,
    g: Option<&BuildGrant>,
) -> Plan {
    let mbx_path = PathBuf::from("/opt/bin/mbx");
    let report = PathBuf::from("/s/report.json");
    assemble(&Inputs {
        depot,
        args: &words(args),
        env,
        mbx: mbx.then_some(mbx_path.as_path()),
        grant: g,
        report: Some(&report),
    })
}

#[test]
fn with_mbx_the_cargo_arguments_follow_mbx_unchanged() {
    let p = plan(
        &Depot::default(),
        "build -p x --profile fast",
        &env(&[]),
        true,
        None,
    );
    assert_eq!(p.argv, words("/opt/bin/mbx build -p x --profile fast"));
    assert_eq!(p.env_set["MBX_STATS_REPORT"], "/s/report.json");
}

#[test]
fn without_mbx_it_is_plain_cargo_and_asks_nothing_of_mbx() {
    let g = grant(6, BuildPriority::Low);
    let p = plan(
        &Depot::default(),
        "check --workspace",
        &env(&[]),
        false,
        Some(&g),
    );
    assert_eq!(p.argv, words("cargo check --workspace"));
    assert!(!p.env_set.contains_key("MBX_STATS_REPORT"));
    assert!(!p.env_set.contains_key("MBX_SCHEDULER_PRIORITY"));
    assert_eq!(p.env_set["CARGO_BUILD_JOBS"], "6");
}

#[test]
fn the_grant_sets_the_job_count_and_the_priority() {
    let low = plan(
        &Depot::default(),
        "build",
        &env(&[]),
        true,
        Some(&grant(8, BuildPriority::Low)),
    );
    assert_eq!(low.env_set["CARGO_BUILD_JOBS"], "8");
    assert_eq!(low.env_set["MBX_SCHEDULER_PRIORITY"], "low");
    let normal = plan(
        &Depot::default(),
        "build",
        &env(&[]),
        true,
        Some(&grant(8, BuildPriority::Normal)),
    );
    assert!(!normal.env_set.contains_key("MBX_SCHEDULER_PRIORITY"));
    let none = plan(&Depot::default(), "build", &env(&[]), true, None);
    assert!(!none.env_set.contains_key("CARGO_BUILD_JOBS"));
}

#[test]
fn what_the_caller_set_or_typed_is_never_overridden() {
    let g = grant(8, BuildPriority::Low);
    let e = env(&[
        ("CARGO_BUILD_JOBS", "2"),
        ("MBX_SCHEDULER_PRIORITY", "normal"),
        ("RUSTUP_TOOLCHAIN", "nightly"),
        ("CARGO_PROFILE_FAST_LTO", "thin"),
    ]);
    let p = plan(&fast_depot(), "build", &e, true, Some(&g));
    assert!(!p.env_set.contains_key("CARGO_BUILD_JOBS"));
    assert!(!p.env_set.contains_key("MBX_SCHEDULER_PRIORITY"));
    assert!(!p.env_set.contains_key("RUSTUP_TOOLCHAIN"));
    assert!(!p.env_set.contains_key("CARGO_PROFILE_FAST_LTO"));
    assert_eq!(p.env_set["CARGO_PROFILE_FAST_CODEGEN_UNITS"], "256");
    for typed in [
        "build -j4",
        "build -j 4",
        "build --jobs 4",
        "build --jobs=4",
    ] {
        let p = plan(&Depot::default(), typed, &env(&[]), true, Some(&g));
        assert!(!p.env_set.contains_key("CARGO_BUILD_JOBS"), "{typed}");
    }
    let after_dashes = plan(&Depot::default(), "test -- -j4", &env(&[]), true, Some(&g));
    assert_eq!(after_dashes.env_set["CARGO_BUILD_JOBS"], "8");
}

#[test]
fn the_toolchain_pin_and_profile_overrides_apply_when_configured() {
    let p = plan(&fast_depot(), "build --profile fast", &env(&[]), true, None);
    assert_eq!(p.env_set["RUSTUP_TOOLCHAIN"], "1.99.1");
    assert_eq!(p.env_set["CARGO_PROFILE_FAST_LTO"], "off");
    assert_eq!(p.env_set["CARGO_PROFILE_FAST_CODEGEN_UNITS"], "256");
    let plain = plan(&Depot::default(), "build", &env(&[]), true, None);
    assert!(
        plain.env_set.keys().all(|k| k == "MBX_STATS_REPORT"),
        "{:?}",
        plain.env_set
    );
}

#[test]
fn an_explicit_plus_toolchain_beats_the_pin() {
    let p = plan(&fast_depot(), "+nightly build", &env(&[]), true, None);
    assert!(!p.env_set.contains_key("RUSTUP_TOOLCHAIN"));
    assert_eq!(p.argv, words("/opt/bin/mbx +nightly build"));
}

#[test]
fn a_leftover_rustc_wrapper_is_dropped_for_mbx_only_and_said_so() {
    let e = env(&[("RUSTC_WRAPPER", "/usr/bin/sccache")]);
    let with = plan(&Depot::default(), "build", &e, true, None);
    assert_eq!(with.env_unset, ["RUSTC_WRAPPER"]);
    assert_eq!(with.notes.len(), 1);
    assert!(with.notes[0].contains("sccache"));
    let without = plan(&Depot::default(), "build", &e, false, None);
    assert!(without.env_unset.is_empty() && without.notes.is_empty());
    let empty = env(&[("RUSTC_WRAPPER", "")]);
    assert!(
        plan(&Depot::default(), "build", &empty, true, None)
            .env_unset
            .is_empty()
    );
}
