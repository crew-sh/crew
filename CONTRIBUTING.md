# Contributing

The README covers building and testing. These are the conventions it does not.

- **Layers.** A crate may depend only on crates in its own layer or below. A new crate needs a row in `crates/crew/tests/layers.rs` and in the README table. The test fails without it.
- **Comments.** One line, and only for a non-obvious reason. If it needs more, it belongs in the commit message or a doc.
- **Strict types.** Use enums for closed sets and typed `serde` structs for files and messages. No `serde_json::Value`, and no string lists standing in for an enum.
- **Config files.** Each tool reads only its own table of `crew.toml` and ignores the rest. Reject unknown keys inside your own table.
- **Tests.** Test behavior, not wording. Take expected values from outside the code under test. Tests that need `systemd-run --user --scope` must skip with a note when it is missing, and `CREW_REQUIRE_SCOPES=1` turns that skip into a failure. Put scratch files in the system temp directory, or in `CREW_TEST_TMP` when set.
- **Checks before a PR.** `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- **Licence.** Contributions are under `MIT OR Apache-2.0`, the same as the project.
