use super::*;

const MBX_REPORT: &str = r#"{
  "version": 5, "session_duration_ns": 2500000000, "hits": 31, "misses": 2, "unconsulted": 23,
  "incremental_compilations": 1, "estimated_compiler_duration_avoided_ns": 14100000000,
  "bypasses": {"compiler-query": 2, "standard-input": 1, "cc-unsupported-environment": 4, "link": 3},
  "compiler": {}, "slow_compilations": []
}"#;

fn report() -> MbxReport {
    serde_json::from_str(MBX_REPORT).unwrap()
}

#[test]
fn a_build_report_becomes_counts_without_routine_probe_bypasses() {
    let c = report().counts();
    assert_eq!(
        (c.hits, c.misses, c.unconsulted, c.incremental, c.bypasses),
        (31, 2, 23, 1, 3)
    );
    assert_eq!(
        (c.compiler_ns_avoided, c.wall_ns),
        (14_100_000_000, 2_500_000_000)
    );
}

#[test]
fn a_report_from_a_newer_mbx_with_missing_fields_still_reads() {
    let r: MbxReport =
        serde_json::from_str(r#"{"version": 9, "hits": 4, "brand_new": true}"#).unwrap();
    assert_eq!(r.counts().hits, 4);
}

#[test]
fn lifetime_totals_come_from_mbx_stats_json() {
    let l = parse_lifetime(
        r#"{"version":1,"savings":{"compilations":{"hits":10,"misses":2,"unconsulted":3,"bypasses":4,"verifications":9}}}"#,
    )
    .unwrap();
    assert_eq!(
        l,
        Lifetime {
            hits: 10,
            misses: 2,
            unconsulted: 3,
            bypasses: 4
        }
    );
    assert_eq!(
        parse_lifetime(r#"{"savings":{}}"#).unwrap(),
        Lifetime::default()
    );
    assert!(parse_lifetime("not json").is_err());
}

#[test]
fn builds_accumulate_per_repository_and_the_last_one_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let root = Path::new("/work/proj");
    let build = |hits, at| Build {
        root,
        command: "build -p x",
        counts: BuildCounts {
            hits,
            misses: 1,
            ..BuildCounts::default()
        },
        at_secs: at,
    };
    record(dir.path(), &build(10, 100)).unwrap();
    record(dir.path(), &build(5, 200)).unwrap();
    let other = Build {
        root: Path::new("/work/other"),
        ..build(99, 300)
    };
    record(dir.path(), &other).unwrap();
    let s = load(&record_path(dir.path(), root)).unwrap();
    assert_eq!(
        (s.builds, s.last_at_secs, s.project.as_str()),
        (2, 200, "proj")
    );
    assert_eq!(s.last.unwrap().hits, 5);
    let t = s.total.unwrap();
    assert_eq!((t.hits, t.misses), (15, 2));
    let o = load(&record_path(dir.path(), Path::new("/work/other"))).unwrap();
    assert_eq!(o.builds, 1);
}

#[test]
fn two_directories_with_one_name_get_two_records_and_a_broken_record_starts_over() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (Path::new("/x/proj"), Path::new("/y/proj"));
    assert_ne!(record_path(dir.path(), a), record_path(dir.path(), b));
    let p = record_path(dir.path(), a);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, b"\xff\xff garbage").unwrap();
    assert!(load(&p).is_none());
    let build = Build {
        root: a,
        command: "check",
        counts: BuildCounts::default(),
        at_secs: 1,
    };
    record(dir.path(), &build).unwrap();
    assert_eq!(load(&p).unwrap().builds, 1);
}

#[test]
fn the_text_report_says_what_is_missing_and_what_is_known() {
    assert!(render(None, None, 0).contains("no build of this project"));
    let s = ProjectStats {
        version: 1,
        project: "proj".into(),
        root: "/work/proj".into(),
        builds: 3,
        last: Some(report().counts()),
        last_at_secs: 90,
        last_command: "build".into(),
        total: Some(report().counts()),
    };
    let text = render(
        Some(&s),
        Some(&Lifetime {
            hits: 7,
            ..Lifetime::default()
        }),
        100,
    );
    assert!(text.contains("builds    3 through crew cargo"), "{text}");
    assert!(text.contains("`cargo build`, 10s ago"), "{text}");
    assert!(
        text.contains("hits 31  misses 2  not looked up 23"),
        "{text}"
    );
    assert!(text.contains("compiler time avoided 14.1s"), "{text}");
    assert!(
        text.contains("machine   all projects") && text.contains("hits 7"),
        "{text}"
    );
}
