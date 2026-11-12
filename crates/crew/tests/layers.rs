use std::collections::BTreeMap;
use std::process::Command;

use serde::Deserialize;

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    dependencies: Vec<Dependency>,
}

#[derive(Deserialize)]
struct Dependency {
    name: String,
}

struct Crate {
    layer: u8,
    /// When set, the crate may depend on exactly these workspace crates and no others.
    only: Option<&'static [&'static str]>,
}

fn layers() -> BTreeMap<&'static str, Crate> {
    BTreeMap::from([
        (
            "crew-proto",
            Crate {
                layer: 0,
                only: Some(&[]),
            },
        ),
        (
            "crew-sys",
            Crate {
                layer: 0,
                only: Some(&[]),
            },
        ),
        (
            "crew-usher-client",
            Crate {
                layer: 1,
                only: Some(&["crew-proto", "crew-sys"]),
            },
        ),
        (
            "crew-usher",
            Crate {
                layer: 1,
                only: None,
            },
        ),
        (
            "crew-git",
            Crate {
                layer: 0,
                only: Some(&["crew-proto"]),
            },
        ),
        (
            "crew-ledger",
            Crate {
                layer: 1,
                only: Some(&["crew-proto"]),
            },
        ),
        (
            "crew-depot-config",
            Crate {
                layer: 1,
                only: Some(&["crew-sys"]),
            },
        ),
        (
            "crew-queue",
            Crate {
                layer: 2,
                only: Some(&["crew-proto", "crew-git", "crew-ledger", "crew-usher-client"]),
            },
        ),
        (
            "crew",
            Crate {
                layer: 4,
                only: Some(&["crew-proto", "crew-usher-client"]),
            },
        ),
    ])
}

fn violations(packages: &[Package], table: &BTreeMap<&str, Crate>) -> Vec<String> {
    let mut out = Vec::new();
    for p in packages {
        let Some(me) = table.get(p.name.as_str()) else {
            out.push(format!("{} has no layer; add it to the table", p.name));
            continue;
        };
        for d in p
            .dependencies
            .iter()
            .filter(|d| table.contains_key(d.name.as_str()))
        {
            let dep = &table[d.name.as_str()];
            if dep.layer > me.layer {
                out.push(format!(
                    "{} (layer {}) depends on {} (layer {})",
                    p.name, me.layer, d.name, dep.layer
                ));
            }
            if me.only.is_some_and(|only| !only.contains(&d.name.as_str())) {
                out.push(format!("{} may not depend on {}", p.name, d.name));
            }
        }
    }
    out
}

fn package(name: &str, deps: &[&str]) -> Package {
    Package {
        name: name.into(),
        dependencies: deps
            .iter()
            .map(|d| Dependency { name: (*d).into() })
            .collect(),
    }
}

#[test]
fn the_workspace_respects_the_layers() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo metadata");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta: Metadata = serde_json::from_slice(&out.stdout).expect("metadata json");
    assert_eq!(violations(&meta.packages, &layers()), Vec::<String>::new());
}

#[test]
fn the_check_catches_upward_and_unlisted_dependencies() {
    let table = layers();
    let bad = [
        package("crew-proto", &["crew-usher"]),
        package("crew", &["crew-usher"]),
        package("crew-newcomer", &[]),
    ];
    let v = violations(&bad, &table);
    assert_eq!(v.len(), 4, "{v:?}");
    assert!(
        v.iter()
            .any(|m| m.contains("crew-proto") && m.contains("layer 1"))
    );
    assert!(v.iter().any(|m| m == "crew may not depend on crew-usher"));
    assert!(v.iter().any(|m| m.contains("crew-newcomer has no layer")));
    assert!(
        violations(
            &[package(
                "crew-usher",
                &["crew-sys", "crew-proto", "crew-usher-client"]
            )],
            &table
        )
        .is_empty()
    );
}
