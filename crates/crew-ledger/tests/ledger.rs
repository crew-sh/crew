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

fn owner_context() -> crew_proto::ledger::ApproverContext {
    crew_proto::ledger::ApproverContext {
        interactive: true,
        typed_confirmation: true,
        user: "owner".into(),
        ..Default::default()
    }
}

fn step<'a>(l: &'a Ledger, id: &str) -> &'a crew_ledger::Step {
    l.plan().step(id).unwrap()
}

#[test]
fn a_step_cannot_be_landed_by_a_status_nor_leave_landed_nor_come_back_from_dropped() {
    let (_d, l) = project(PLAN);
    set(&l, "a", Status::Running);
    assert!(matches!(
        l.record(events::status("a", Status::Landed, "agent", "")),
        Err(Error::BadTransition { .. })
    ));
    assert_ne!(l.state("a").unwrap().status, Status::Landed);
    assert!(
        !ids(&l).contains(&"b".to_string()),
        "b waits for a real landing"
    );
    // A landing needs the merge sha, and only a step that was started.
    assert!(matches!(
        l.record(events::landed("b", "abc", "")),
        Err(Error::Invalid(_) | Error::BadTransition { .. })
    ));
    l.record(events::landed("a", "abc123", "batch")).unwrap();
    assert_eq!(l.state("a").unwrap().status, Status::Landed);
    assert_eq!(l.state("a").unwrap().detail, "landed abc123");
    for back in [
        Status::Planned,
        Status::Running,
        Status::Built,
        Status::Dropped,
    ] {
        assert!(
            matches!(
                l.record(events::status("a", back, "t", "")),
                Err(Error::BadTransition { .. })
            ),
            "landed -> {back:?}"
        );
    }

    set(&l, "c2", Status::Dropped);
    assert!(matches!(
        l.record(events::status("c2", Status::Built, "t", "")),
        Err(Error::BadTransition { .. })
    ));
    set(&l, "c2", Status::Planned);
}

#[test]
fn a_step_whose_dependency_has_not_landed_cannot_land() {
    let (_d, l) = project(PLAN);
    set(&l, "b", Status::Built);
    let err = l.record(events::landed("b", "abc", "")).unwrap_err();
    assert!(err.to_string().contains("dependency `a`"), "{err}");
}

#[test]
fn an_approval_binds_to_the_step_as_the_owner_saw_it() {
    let (d, l) = project(PLAN);
    l.record(events::approve_step_as(
        step(&l, "x"),
        "owner",
        "",
        owner_context(),
    ))
    .unwrap();
    assert!(l.state("x").unwrap().allowed);
    // The agent edits plan.toml afterwards: a bigger budget, same id. The approval no longer matches.
    let edited = PLAN.replace(
        "owns = [\"crates/x/**\"]",
        "owns = [\"crates/x/**\"]\nbudget = { builds = 999 }",
    );
    std::fs::write(d.path().join("plan.toml"), edited).unwrap();
    let l = Ledger::open(d.path()).unwrap();
    let st = l.state("x").unwrap();
    assert!(!st.allowed && st.approval_stale);
    assert!(matches!(
        l.record(events::status("x", Status::Running, "t", "")),
        Err(Error::NeedsApproval(_))
    ));
    l.record(events::approve_step_as(
        step(&l, "x"),
        "owner",
        "",
        owner_context(),
    ))
    .unwrap();
    assert!(l.state("x").unwrap().allowed);
}

#[test]
fn a_scope_approval_covers_the_extras_it_listed_and_not_ones_added_later() {
    let (d, l) = project(PLAN);
    let extras: Vec<&crew_ledger::Step> = l
        .plan()
        .steps
        .iter()
        .filter(|s| s.covered_by_scope())
        .collect();
    assert_eq!(extras.len(), 1);
    l.record(events::approve_scope_as(
        &extras,
        "owner",
        "",
        owner_context(),
    ))
    .unwrap();
    assert!(l.state("x").unwrap().allowed);
    let later = format!(
        "{PLAN}\n[[step]]\nid = \"y\"\ntitle = \"added after\"\nkind = \"extra\"\nowns = [\"crates/y/**\"]\n"
    );
    std::fs::write(d.path().join("plan.toml"), later).unwrap();
    let l = Ledger::open(d.path()).unwrap();
    assert!(l.state("x").unwrap().allowed);
    assert!(
        !l.state("y").unwrap().allowed,
        "an extra added after the scope approval needs its own"
    );
}

#[test]
fn an_approval_from_an_agent_context_is_ignored_and_reported() {
    let (_d, l) = project(PLAN);
    let agent = crew_proto::ledger::ApproverContext {
        agent: "worker-1".into(),
        interactive: true,
        typed_confirmation: true,
        ..Default::default()
    };
    l.record(events::approve_step_as(
        step(&l, "x"),
        "worker-1",
        "",
        agent,
    ))
    .unwrap();
    assert!(!l.state("x").unwrap().allowed);
    assert!(
        l.snapshot()
            .unwrap()
            .warnings
            .iter()
            .any(|w| w.contains("not given by the owner")),
        "{:?}",
        l.snapshot().unwrap().warnings
    );
    let untyped = crew_proto::ledger::ApproverContext {
        interactive: true,
        ..Default::default()
    };
    l.record(events::approve_step_as(step(&l, "x"), "owner", "", untyped))
        .unwrap();
    assert!(!l.state("x").unwrap().allowed, "no typed confirmation");
    let token = crew_proto::ledger::ApproverContext {
        via_owner_token: true,
        ..Default::default()
    };
    l.record(events::approve_step_as(step(&l, "x"), "owner", "", token))
        .unwrap();
    assert!(l.state("x").unwrap().allowed);
}

fn varint(mut n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

#[test]
fn a_damaged_length_in_the_middle_is_corruption_not_a_cut_off_tail() {
    let (_d, l) = project(PLAN);
    set(&l, "a", Status::Running);
    set(&l, "a", Status::Built);
    set(&l, "x", Status::Held);
    let path = l.path().to_path_buf();
    let good = std::fs::read(&path).unwrap();
    // The first record's length now claims more bytes than the file has, as a damaged byte could make it.
    let first_len = good[0] as usize;
    let mut bad = varint(good.len() + 50);
    bad.extend_from_slice(&good[1..]);
    std::fs::write(&path, &bad).unwrap();
    assert!(
        matches!(l.state("a"), Err(Error::Corrupt { .. })),
        "records behind it were swallowed as a tail"
    );
    assert!(
        l.record(events::status("a", Status::Failed, "t", ""))
            .is_err(),
        "a write must not truncate the file here"
    );
    assert_eq!(std::fs::read(&path).unwrap(), bad, "the file is untouched");
    // An absurd claim is damage too.
    let mut huge = varint(1 << 30);
    huge.extend_from_slice(&good[1..first_len + 1]);
    std::fs::write(&path, &huge).unwrap();
    assert!(matches!(l.state("a"), Err(Error::Corrupt { .. })));
}

#[test]
fn cutting_off_a_partial_tail_keeps_a_copy_of_the_file_first() {
    let (_d, l) = project(PLAN);
    set(&l, "a", Status::Running);
    set(&l, "a", Status::Built);
    let path = l.path().to_path_buf();
    let whole = std::fs::read(&path).unwrap();
    let cut = &whole[..whole.len() - 2];
    std::fs::write(&path, cut).unwrap();
    l.record(events::status("a", Status::Failed, "t", ""))
        .unwrap();
    let copies: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("truncated-"))
        .collect();
    assert_eq!(copies.len(), 1);
    assert_eq!(std::fs::read(copies[0].path()).unwrap(), cut);
}

#[test]
fn check_rejects_step_ids_that_git_or_the_filesystem_would_misread() {
    for bad in [
        "..",
        "../x",
        "/abs",
        ".integration",
        "-rf",
        "a b",
        "a/b",
        "x..y",
    ] {
        let plan =
            format!("[[step]]\nid = {bad:?}\ntitle = \"t\"\nkind = \"asked\"\nowns = [\"a/**\"]\n");
        let (_d, l) = project(&plan);
        let found = check(&l).unwrap();
        assert!(
            found
                .iter()
                .any(|v| matches!(v, Violation::BadId(id) if id == bad)),
            "{bad}: {found:?}"
        );
    }
    let (_d, l) = project(
        "[[step]]\nid = \"s1.fix_2-b\"\ntitle = \"t\"\nkind = \"asked\"\nowns = [\"a/**\"]\n",
    );
    assert!(
        !check(&l)
            .unwrap()
            .iter()
            .any(|v| matches!(v, Violation::BadId(_)))
    );
}

fn facts(l: &Ledger) -> Vec<(String, crew_ledger::StepState)> {
    l.plan()
        .steps
        .iter()
        .map(|s| (s.id.clone(), l.state(&s.id).unwrap()))
        .collect()
}

#[test]
fn compaction_keeps_every_fact_and_shrinks_the_file() {
    let (_d, l) = project(PLAN);
    l.record(events::approve_step("x", "owner", "ok")).unwrap();
    for status in [Status::Approved, Status::Running, Status::Built] {
        set(&l, "a", status);
    }
    for _ in 0..30 {
        l.record(events::budget_use(
            "a",
            "t",
            Use {
                builds: 1,
                test_ms: 1000,
                ..Use::default()
            },
        ))
        .unwrap();
    }
    set(&l, "x", Status::Running);
    set(&l, "x", Status::Failed);
    set(&l, "x", Status::Running);
    l.record(events::landed("a", &"c".repeat(40), "b1"))
        .unwrap();
    let before = facts(&l);
    let before_bytes = std::fs::read(l.path()).unwrap();

    let c = l.compact().unwrap();

    assert_eq!(facts(&l), before, "compaction changed what the ledger says");
    assert!(c.events_after < c.events_before / 2, "{c:?}");
    assert!(c.bytes_after < c.bytes_before);
    assert_eq!(std::fs::read(l.path()).unwrap().len() as u64, c.bytes_after);
    assert_eq!(std::fs::read(c.backup.unwrap()).unwrap(), before_bytes);
    assert_eq!(l.state("a").unwrap().used.builds, 30);
    assert!(l.state("x").unwrap().allowed, "the approval is still there");

    // The ledger is still appendable, and a second compaction has nothing to drop.
    set(&l, "x", Status::Built);
    assert_eq!(l.state("x").unwrap().status, Status::Built);
    let again = l.compact().unwrap();
    assert!(again.backup.is_none() || again.events_after <= again.events_before);
    assert_eq!(l.state("a").unwrap().used.builds, 30);
}

#[test]
fn compacting_a_ledger_with_nothing_to_drop_changes_nothing() {
    let (_d, l) = project(PLAN);
    assert_eq!(l.compact().unwrap().events_before, 0, "no file yet");
    set(&l, "a", Status::Approved);
    let bytes = std::fs::read(l.path()).unwrap();
    let c = l.compact().unwrap();
    assert!(c.backup.is_none());
    assert_eq!(std::fs::read(l.path()).unwrap(), bytes);
}
