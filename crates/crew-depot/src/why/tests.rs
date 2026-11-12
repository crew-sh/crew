use super::*;

/// Shaped like `mbx explain --last` output (mbx 1.30 `explain.rs`, `display_last` and `diff_lines`).
const SAMPLE: &str = "\
cache explanation: last recorded build
  workspace: /work/proj
  command: cargo build -p app
  results: 3 hit, 4 miss

missed crates

serde_derive
  key details changed since the last recording:
    - compiler rustc version
    - argument -C metadata
  inputs changed since the last recording:
    - input ${workspace}/src/lib.rs

serde
  nothing in this compilation changed; it missed because the artifact of serde_derive differs, so explain that crate first

foo
  the action key did not change; its cached result was unavailable, evicted, or absent from the configured remote

bar
  no earlier build recorded key details for this crate; the cache may be cold, this action may use another adapter, or its history may have expired

2 compilations bypassed the cache, and the results above give only the cause of each; rerun the command above under `mbx explain`
";

#[test]
fn the_header_and_every_missed_crate_are_read() {
    let e = parse(SAMPLE);
    assert_eq!(e.workspace, "/work/proj");
    assert_eq!(e.command, "cargo build -p app");
    assert_eq!(e.results, "3 hit, 4 miss");
    let crates: Vec<_> = e.missed.iter().map(|m| m.krate.as_str()).collect();
    assert_eq!(crates, ["serde_derive", "serde", "foo", "bar"]);
}

#[test]
fn changed_keys_and_inputs_keep_their_items() {
    let e = parse(SAMPLE);
    assert_eq!(
        find(&e, "serde_derive").unwrap().reasons,
        [
            Reason::KeyDetails(vec![
                "compiler rustc version".into(),
                "argument -C metadata".into()
            ]),
            Reason::Inputs(vec!["input ${workspace}/src/lib.rs".into()]),
        ]
    );
}

#[test]
fn a_miss_caused_by_a_dependency_names_it() {
    let e = parse(SAMPLE);
    assert_eq!(
        find(&e, "serde").unwrap().reasons,
        [Reason::DependencyChanged(vec!["serde_derive".into()])]
    );
    let two = parse(
        "missed crates\n\nx\n  nothing in this compilation changed; it missed because the artifacts of a, b and c differ, so explain those crates first\n",
    );
    assert_eq!(
        find(&two, "x").unwrap().reasons,
        [Reason::DependencyChanged(vec![
            "a".into(),
            "b".into(),
            "c".into()
        ])]
    );
}

#[test]
fn the_unchanged_key_and_cold_cache_cases_are_told_apart() {
    let e = parse(SAMPLE);
    assert_eq!(find(&e, "foo").unwrap().reasons, [Reason::KeyUnchanged]);
    assert_eq!(
        find(&e, "bar").unwrap().reasons,
        [Reason::NoEarlierRecording]
    );
}

#[test]
fn the_section_after_the_crate_list_is_not_a_crate() {
    let e = parse(SAMPLE);
    assert!(find(&e, "2").is_none() && e.missed.len() == 4);
    let warned = parse(
        "missed crates\n\nx\n  the action key did not change; its cached result was unavailable\n\ncacheability warning\nCC is set\n",
    );
    assert_eq!(warned.missed.len(), 1);
}

#[test]
fn dashes_and_underscores_name_the_same_crate() {
    let e = parse(SAMPLE);
    assert!(find(&e, "serde-derive").is_some());
}

#[test]
fn unknown_lines_are_kept_as_notes() {
    let e = parse(
        "missed crates\n\nx\n  hint: OUT_DIR differs between the checkouts; set it\n  something new\n",
    );
    assert_eq!(
        find(&e, "x").unwrap().reasons,
        [
            Reason::Note("hint: OUT_DIR differs between the checkouts; set it".into()),
            Reason::Note("something new".into())
        ]
    );
}

#[test]
fn a_build_with_no_misses_says_so_and_a_hit_crate_is_not_blamed() {
    let none = parse(
        "cache explanation: last recorded build\n  workspace: /w\n  command: cargo build\n  results: \n\nno cache misses were recorded\n",
    );
    assert!(none.missed.is_empty());
    assert!(render(&none, "serde").contains("recorded no cache misses"));
    let text = render(&parse(SAMPLE), "tokio");
    assert!(
        text.contains("tokio did not miss") && text.contains("serde_derive, serde, foo, bar"),
        "{text}"
    );
}

#[test]
fn the_text_for_a_miss_lists_each_reason() {
    let text = render(&parse(SAMPLE), "serde_derive");
    assert!(text.contains("serde_derive missed the cache:"), "{text}");
    assert!(
        text.contains("  - compiler rustc version")
            && text.contains("input ${workspace}/src/lib.rs"),
        "{text}"
    );
}
