use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use crew_ledger::{
    Error, Kind, Ledger, Refusal, RunKind, Status, Use, Violation, check, events, show, summarize,
};

const PLAN: &str = r#"
[[step]]
id = "a"
title = "asked one"
kind = "asked"
owns = ["crates/a/**"]
budget = { builds = 2, test_minutes = 10, harnesses = 1 }

[[step]]
id = "b"
title = "asked two"
kind = "asked"
deps = ["a"]
owns = ["crates/a/src/b.rs"]

[[step]]
id = "x"
title = "extra thing"
kind = "extra"
owns = ["crates/x/**"]

[[step]]
id = "c"
title = "core design"
kind = "core-design"
owns = ["crates/c/**"]

[[step]]
id = "c2"
title = "core design, agent first"
kind = "core-design"
review = "owner-after"
owns = ["crates/c2/**"]
"#;

fn project(plan: &str) -> (tempfile::TempDir, Ledger) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("crew.toml"),
        "[plan]\nrequest = \"build the thing\"\n\n[queue]\nbase = \"main\"\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("plan.toml"), plan).unwrap();
    let ledger = Ledger::open(dir.path()).unwrap();
    (dir, ledger)
}

fn set(l: &Ledger, id: &str, s: Status) {
    l.record(events::status(id, s, "t", "")).unwrap();
}

fn ids(l: &Ledger) -> Vec<String> {
    l.ready_steps()
        .unwrap()
        .iter()
        .map(|s| s.id.clone())
        .collect()
}

#[test]
fn fold_takes_the_last_status_and_sums_budget_use() {
    let (_d, l) = project(PLAN);
    assert_eq!(l.state("a").unwrap().status, Status::Planned);
    set(&l, "a", Status::Running);
    l.record(events::budget_use(
        "a",
        "t",
        Use {
            builds: 1,
            test_ms: 5000,
            ..Use::default()
        },
    ))
    .unwrap();
    l.record(events::budget_use(
        "a",
        "t",
        Use {
            builds: 1,
            test_ms: 7000,
            harnesses: 1,
            games: 0,
        },
    ))
    .unwrap();
    set(&l, "a", Status::Built);
    l.record(events::landed("a", "abc123", "batch-1")).unwrap();
    let st = l.state("a").unwrap();
    assert_eq!(st.status, Status::Landed);
    assert_eq!(st.landed.unwrap().sha, "abc123");
    assert_eq!(
        st.used,
        Use {
            builds: 2,
            test_ms: 12_000,
            harnesses: 1,
            games: 0
        }
    );
    assert!(st.at_ms > 0);
}

#[test]
fn ready_steps_wait_for_deps_to_land_and_keep_plan_order() {
    let (_d, l) = project(PLAN);
    assert_eq!(ids(&l), ["a", "c2"]);
    set(&l, "a", Status::Built);
    assert_eq!(ids(&l), ["c2"]);
    l.record(events::landed("a", "s", "")).unwrap();
    assert_eq!(ids(&l), ["b", "c2"]);
    set(&l, "b", Status::Failed);
    assert_eq!(ids(&l), ["c2"]);
}

#[test]
fn an_unapproved_extra_cannot_leave_planned() {
    let (_d, l) = project(PLAN);
    for s in [
        Status::Approved,
        Status::Running,
        Status::Built,
        Status::Landed,
    ] {
        assert!(matches!(
            l.record(events::status("x", s, "t", "")),
            Err(Error::NeedsApproval(id)) if id == "x"
        ));
    }
    assert!(matches!(
        l.record(events::landed("x", "s", "")),
        Err(Error::NeedsApproval(_))
    ));
    assert!(!ids(&l).contains(&"x".to_string()));
    set(&l, "x", Status::Held);
    l.record(events::approve_step("x", "owner", "yes")).unwrap();
    assert_eq!(l.state("x").unwrap().status, Status::Held);
    set(&l, "x", Status::Planned);
    assert_eq!(l.state("x").unwrap().status, Status::Planned);
    assert!(ids(&l).contains(&"x".to_string()));
    set(&l, "x", Status::Running);
}

#[test]
fn core_design_needs_approval_unless_owner_reviews_after() {
    let (_d, l) = project(PLAN);
    assert!(matches!(
        l.record(events::status("c", Status::Running, "t", "")),
        Err(Error::NeedsApproval(_))
    ));
    set(&l, "c2", Status::Running);
    l.record(events::approve_scope("owner", "all extras"))
        .unwrap();
    assert!(
        l.record(events::status("c", Status::Running, "t", ""))
            .is_err(),
        "a scope approval covers extras, not core design"
    );
    l.record(events::approve_step("c", "owner", "")).unwrap();
    assert_eq!(l.state("c").unwrap().status, Status::Approved);
    set(&l, "c", Status::Running);
}

#[test]
fn scope_approval_covers_every_extra() {
    let (_d, l) = project(PLAN);
    assert!(!l.state("x").unwrap().allowed);
    l.record(events::approve_scope("owner", "")).unwrap();
    assert!(l.state("x").unwrap().allowed);
    set(&l, "x", Status::Running);
}

#[test]
fn unknown_steps_and_bad_events_are_rejected() {
    let (_d, l) = project(PLAN);
    assert!(matches!(
        l.record(events::status("nope", Status::Running, "t", "")),
        Err(Error::UnknownStep(_))
    ));
    assert!(matches!(l.state("nope"), Err(Error::UnknownStep(_))));
    assert!(
        l.record(events::status("a", Status::Unspecified, "t", ""))
            .is_err()
    );
    assert!(l.record(events::landed("a", "", "")).is_err());
}

#[test]
fn check_reports_cycles_dupes_unknown_deps_and_overlaps() {
    let plan = r#"
[[step]]
id = "p"
title = "p"
kind = "asked"
deps = ["q"]
owns = ["src/**"]
[[step]]
id = "q"
title = "q"
kind = "asked"
deps = ["p", "ghost"]
[[step]]
id = "r"
title = "r"
kind = "asked"
owns = ["src/lib.rs"]
[[step]]
id = "r"
title = "again"
kind = "asked"
"#;
    let (_d, l) = project(plan);
    let v = check(&l).unwrap();
    assert!(v.contains(&Violation::DuplicateId("r".into())), "{v:?}");
    assert!(v.contains(&Violation::UnknownDep {
        step: "q".into(),
        dep: "ghost".into()
    }));
    assert!(v.iter().any(|x| matches!(x, Violation::Cycle(c) if c.contains(&"p".to_string()) && c.contains(&"q".to_string()))));
    assert!(
        v.iter()
            .any(|x| matches!(x, Violation::Overlap { a, b, .. } if a == "p" && b == "r"))
    );
}

#[test]
fn overlap_is_fine_when_a_dependency_path_orders_the_steps() {
    let (_d, l) = project(PLAN);
    assert_eq!(check(&l).unwrap(), Vec::<Violation>::new());
    let plan = PLAN.replace(
        "deps = [\"a\"]\nowns = [\"crates/a/src/b.rs\"]",
        "owns = [\"crates/a/src/b.rs\"]",
    );
    let (_d2, l2) = project(&plan);
    let v = check(&l2).unwrap();
    assert_eq!(v.len(), 1, "{v:?}");
    assert!(matches!(&v[0], Violation::Overlap { a, b, glob_a, glob_b }
        if a == "a" && b == "b" && glob_a == "crates/a/**" && glob_b == "crates/a/src/b.rs"));
}

#[test]
fn check_flags_a_ledger_that_skipped_the_approval_gate() {
    let (d, l) = project(PLAN);
    let path = l.path().to_path_buf();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // appended by hand, bypassing record()
    let ev = events::status("x", Status::Running, "rogue", "");
    let mut bytes = Vec::new();
    crew_proto_encode(&ev, &mut bytes);
    std::fs::write(&path, bytes).unwrap();
    let v = check(&Ledger::open(d.path()).unwrap()).unwrap();
    assert!(
        v.contains(&Violation::Unapproved {
            step: "x".into(),
            status: Status::Running
        }),
        "{v:?}"
    );
}

fn crew_proto_encode(ev: &crew_ledger::Event, out: &mut Vec<u8>) {
    use crew_proto::prost::Message;
    ev.encode_length_delimited(out).unwrap();
}

#[test]
fn report_counts_per_kind_not_one_percentage() {
    let (_d, l) = project(PLAN);
    set(&l, "a", Status::Built);
    l.record(events::landed("a", "s", "")).unwrap();
    set(&l, "c2", Status::Failed);
    l.record(events::status("c2", Status::Failed, "t", "boom\nmore"))
        .unwrap();
    set(&l, "b", Status::Dropped);
    let s = summarize(&l).unwrap();
    let get = |k: Kind| s.kinds.iter().find(|c| c.kind == k).unwrap().clone();
    let asked = get(Kind::Asked);
    assert_eq!((asked.done, asked.total, asked.dropped), (1, 1, 1));
    let extra = get(Kind::Extra);
    assert_eq!((extra.done, extra.total, extra.waiting_approval), (0, 1, 1));
    let core = get(Kind::CoreDesign);
    assert_eq!((core.done, core.total, core.waiting_approval), (0, 2, 1));
    assert_eq!(s.failed.len(), 1);
    assert_eq!(s.failed[0].step, "c2");
    assert_eq!(
        s.counts_line(),
        "asked 1/1 · extra 0/1 (1 waiting approval) · core 0/2 (1 waiting approval)"
    );
    let full = show(&l).unwrap();
    let text = s.render_report(&full);
    assert!(
        text.contains("waiting on you:") && text.contains("failed:") && text.contains("c2: boom")
    );
}

#[test]
fn budget_use_over_the_limit_is_reported_and_refused() {
    let (_d, l) = project(PLAN);
    assert_eq!(l.may_run("a", RunKind::Build).unwrap(), None);
    l.record(events::budget_use(
        "a",
        "t",
        Use {
            builds: 2,
            ..Use::default()
        },
    ))
    .unwrap();
    assert_eq!(
        l.may_run("a", RunKind::Build).unwrap(),
        Some(Refusal::Exhausted { used: 2, limit: 2 })
    );
    assert_eq!(
        check(&l).unwrap(),
        Vec::<Violation>::new(),
        "at the limit is not over it"
    );
    assert_eq!(l.may_run("a", RunKind::Test).unwrap(), None);
    assert_eq!(
        l.may_run("a", RunKind::Game).unwrap(),
        None,
        "no limit stated means unlimited"
    );
    l.record(events::budget_use(
        "a",
        "t",
        Use {
            builds: 1,
            test_ms: 10 * 60_000,
            ..Use::default()
        },
    ))
    .unwrap();
    let v = check(&l).unwrap();
    assert_eq!(
        v,
        [Violation::OverBudget {
            step: "a".into(),
            kinds: vec![RunKind::Build]
        }]
    );
    assert_eq!(
        l.may_run("a", RunKind::Test).unwrap(),
        Some(Refusal::Exhausted {
            used: 600_000,
            limit: 600_000
        })
    );
    assert_eq!(
        l.may_run("x", RunKind::Build).unwrap(),
        Some(Refusal::NeedsApproval)
    );
    let s = summarize(&l).unwrap();
    assert_eq!(s.over_budget.len(), 1);
}

#[test]
fn a_truncated_last_record_is_ignored_with_a_warning_and_repaired_by_the_next_write() {
    let (_d, l) = project(PLAN);
    set(&l, "a", Status::Running);
    l.record(events::landed("a", "deadbeef", "b")).unwrap();
    let path = l.path().to_path_buf();
    let whole = std::fs::read(&path).unwrap();
    std::fs::write(&path, &whole[..whole.len() - 3]).unwrap();
    let snap = l.snapshot().unwrap();
    assert_eq!(snap.warnings.len(), 1, "{:?}", snap.warnings);
    assert_eq!(l.state("a").unwrap().status, Status::Running);
    l.record(events::status("a", Status::Built, "t", ""))
        .unwrap();
    let snap = l.snapshot().unwrap();
    assert!(snap.warnings.is_empty());
    assert_eq!(l.state("a").unwrap().status, Status::Built);
    // a length prefix with no body at all
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&[0x80])
        .unwrap();
    assert_eq!(l.snapshot().unwrap().warnings.len(), 1);
    assert_eq!(l.state("a").unwrap().status, Status::Built);
}

#[test]
fn a_corrupt_record_before_the_tail_is_an_error() {
    let (_d, l) = project(PLAN);
    set(&l, "a", Status::Running);
    set(&l, "a", Status::Built);
    let path = l.path().to_path_buf();
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[1] = 0xff; // field tag 31 with a bad wire type
    std::fs::write(&path, bytes).unwrap();
    assert!(matches!(l.state("a"), Err(Error::Corrupt { .. })));
}

#[test]
fn concurrent_writers_lose_nothing() {
    let (d, _l) = project(PLAN);
    let root = d.path().to_path_buf();
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let root = root.clone();
            std::thread::spawn(move || {
                let l = Ledger::open(&root).unwrap();
                for _ in 0..25 {
                    l.record(events::budget_use(
                        "a",
                        "t",
                        Use {
                            builds: 1,
                            ..Use::default()
                        },
                    ))
                    .unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(
        Ledger::open(Path::new(&root))
            .unwrap()
            .state("a")
            .unwrap()
            .used
            .builds,
        150
    );
}

#[test]
fn the_plan_file_rejects_unknown_keys_and_kinds() {
    for bad in [
        "[[step]]\nid=\"a\"\ntitle=\"t\"\nkind=\"asked\"\ntypo=1\n",
        "[[step]]\nid=\"a\"\ntitle=\"t\"\nkind=\"maybe\"\n",
        "[[step]]\nid=\"a\"\ntitle=\"t\"\nkind=\"asked\"\nmodel=\"gpt\"\n",
        "[[step]]\nid=\"a\"\ntitle=\"t\"\nkind=\"asked\"\nbudget={ cakes = 1 }\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("crew.toml"), "[plan]\nrequest=\"r\"\n").unwrap();
        std::fs::write(dir.path().join("plan.toml"), bad).unwrap();
        assert!(
            matches!(Ledger::open(dir.path()), Err(Error::Toml { .. })),
            "{bad}"
        );
    }
}
