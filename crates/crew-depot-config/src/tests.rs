use std::os::unix::fs::PermissionsExt;

use super::*;

fn parse(text: &str) -> Result<DepotTable, ConfigError> {
    DepotTable::parse(text, "test.toml")
}

fn exe(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[test]
fn the_public_defaults_name_no_machine_and_change_nothing() {
    let d = Depot::default();
    assert_eq!(d.toolchain, None);
    assert_eq!(d.mbx, MbxMode::Auto);
    assert!(!d.mbx_reserve);
    assert!(d.profiles.is_empty() && d.profile_env().is_empty());
    assert_eq!(d.store_budget_gib, 60);
    assert!(d.store.is_absolute() || std::env::var_os("HOME").is_none());
    assert!(d.store.ends_with("crew"));
}

#[test]
fn a_file_without_the_table_means_no_settings_and_other_tables_are_ignored() {
    assert_eq!(parse("").unwrap(), DepotTable::default());
    let t = parse("[queue]\nbase = \"main\"\nanything = 1\n[depot]\ntoolchain = \"1.99.1\"\n");
    assert_eq!(t.unwrap().toolchain.as_deref(), Some("1.99.1"));
}

#[test]
fn unknown_keys_are_errors_inside_depot_only() {
    assert!(parse("[depot]\ntoolchane = \"x\"\n").is_err());
    assert!(parse("[depot.profiles.fast]\nlto = \"off\"\nopt-level = 3\n").is_err());
    assert!(parse("[depot]\nmbx = \"sometimes\"\n").is_err());
    assert!(parse("[depot.profiles.fast]\nlto = \"maybe\"\n").is_err());
    assert!(parse("[other]\nwhatever = true\n").is_ok());
}

#[test]
fn a_profile_name_must_be_something_cargo_can_read_from_the_environment() {
    assert!(parse("[depot.profiles.\"a b\"]\nlto = \"off\"\n").is_err());
    assert!(parse("[depot.profiles.agent-fast]\nlto = \"off\"\n").is_ok());
}

#[test]
fn profile_overrides_become_cargo_profile_variables() {
    let t = parse(
        "[depot.profiles.fast]\nlto = \"off\"\ncodegen-units = 256\n[depot.profiles.agent-dev]\nlto = \"thin\"\n",
    )
    .unwrap();
    let env = t.resolve().profile_env();
    assert_eq!(
        env,
        [
            (
                "CARGO_PROFILE_AGENT_DEV_LTO".to_string(),
                "thin".to_string()
            ),
            ("CARGO_PROFILE_FAST_LTO".to_string(), "off".to_string()),
            (
                "CARGO_PROFILE_FAST_CODEGEN_UNITS".to_string(),
                "256".to_string()
            ),
        ]
    );
}

#[test]
fn the_project_overrides_the_user_key_by_key() {
    let user = parse(
        "[depot]\ntoolchain = \"1.99.1\"\nmbx_reserve = true\nreserve_cpus = 6\n[depot.profiles.fast]\nlto = \"off\"\ncodegen-units = 256\n",
    )
    .unwrap();
    let project = parse(
        "[depot]\ntoolchain = \"stable\"\n[depot.profiles.fast]\ncodegen-units = 64\n[depot.profiles.dev]\nlto = \"thin\"\n",
    )
    .unwrap();
    let d = user.layered(project).resolve();
    assert_eq!(d.toolchain.as_deref(), Some("stable"));
    assert!(d.mbx_reserve);
    assert_eq!(d.reserve_cpus, 6);
    assert_eq!(d.profiles["fast"].lto, Some(Lto::Off));
    assert_eq!(d.profiles["fast"].codegen_units, Some(64));
    assert_eq!(d.profiles["dev"].lto, Some(Lto::Thin));
}

#[test]
fn files_are_read_from_the_config_dir_and_the_nearest_crew_toml() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        DepotTable::user_in(dir.path()).unwrap(),
        DepotTable::default()
    );
    fs::write(
        dir.path().join("depot.toml"),
        "[depot]\nmbx_reserve = true\n",
    )
    .unwrap();
    assert_eq!(
        DepotTable::user_in(dir.path()).unwrap().mbx_reserve,
        Some(true)
    );
    let repo = dir.path().join("repo");
    fs::create_dir_all(repo.join("a/b")).unwrap();
    assert_eq!(
        DepotTable::project(&repo.join("a/b")).unwrap(),
        DepotTable::default()
    );
    fs::write(repo.join("crew.toml"), "[depot]\nmbx = \"off\"\n").unwrap();
    assert_eq!(
        DepotTable::project(&repo.join("a/b")).unwrap().mbx,
        Some(MbxMode::Off)
    );
    fs::write(repo.join("crew.toml"), "[depot]\nbogus = 1\n").unwrap();
    assert!(DepotTable::project(&repo).is_err());
}

#[test]
fn mbx_is_found_on_the_path_then_in_cargos_bin_dir() {
    let dir = tempfile::tempdir().unwrap();
    let (on_path, cargo_bin) = (dir.path().join("p"), dir.path().join("c"));
    fs::create_dir_all(&on_path).unwrap();
    fs::create_dir_all(&cargo_bin).unwrap();
    let d = Depot::default();
    assert_eq!(d.find_mbx_in(Some(on_path.as_os_str()), &cargo_bin), None);
    let in_cargo = exe(&cargo_bin, "mbx");
    assert_eq!(
        d.find_mbx_in(Some(on_path.as_os_str()), &cargo_bin),
        Some(in_cargo)
    );
    let first = exe(&on_path, "mbx");
    assert_eq!(
        d.find_mbx_in(Some(on_path.as_os_str()), &cargo_bin),
        Some(first)
    );
}

#[test]
fn a_configured_path_wins_and_off_hides_mbx() {
    let dir = tempfile::tempdir().unwrap();
    let mine = exe(dir.path(), "my-mbx");
    let on_path = exe(dir.path(), "mbx");
    let mut d = Depot {
        mbx_path: Some(mine.clone()),
        ..Depot::default()
    };
    assert_eq!(
        d.find_mbx_in(Some(dir.path().as_os_str()), dir.path()),
        Some(mine)
    );
    d.mbx_path = Some(dir.path().join("missing"));
    assert_eq!(
        d.find_mbx_in(Some(dir.path().as_os_str()), dir.path()),
        None
    );
    d.mbx_path = None;
    assert_eq!(
        d.find_mbx_in(Some(dir.path().as_os_str()), dir.path()),
        Some(on_path)
    );
    d.mbx = MbxMode::Off;
    assert_eq!(
        d.find_mbx_in(Some(dir.path().as_os_str()), dir.path()),
        None
    );
}

/// The first ```toml fence after `<!-- test:NAME -->` in the README.
fn readme_block(name: &str) -> String {
    let readme = include_str!("../../../README.md");
    let after = readme
        .split(&format!("<!-- test:{name} -->"))
        .nth(1)
        .unwrap();
    let body = after.split("```toml\n").nth(1).unwrap();
    body.split("```").next().unwrap().to_string()
}

#[test]
fn the_readme_example_loads_and_means_what_it_says() {
    let d = parse(&readme_block("depot-example")).unwrap().resolve();
    assert_eq!(d.toolchain.as_deref(), Some("1.99.1"));
    assert!(d.mbx_reserve);
    assert_eq!((d.reserve_cpus, d.reserve_memory_gib), (4, 8));
    assert_eq!(
        d.profile_env(),
        [
            ("CARGO_PROFILE_FAST_LTO".to_string(), "off".to_string()),
            (
                "CARGO_PROFILE_FAST_CODEGEN_UNITS".to_string(),
                "256".to_string()
            )
        ]
    );
}
