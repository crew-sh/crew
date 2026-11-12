# crew

Tooling for parallel AI coding agents. This workspace holds the MVP of `usher`: a machine-wide admission daemon and its CLI.

- `crew usher` runs the daemon (`crew-usher`) in the foreground; config is `~/.config/crew/usher.toml` (see `crates/crew-usher/src/config.rs` for every key).
- `crew run [--kind K] [--mem 8G] [--cpus N] [--display] -- CMD...` asks the daemon for admission, then runs CMD in its own systemd user scope.
- `crew top [--watch] [--json]` shows running and queued jobs, what each queued job waits on, and the agent sessions (claude, codex) and projects the daemon observes.
- `crew wait queue-empty|owner-away|job <id> [--timeout 30s] [--json]` blocks until the daemon reports the condition: exit 0, 124 on timeout, 2 if the job never ran.
- `--json` (run, top, owner, wait) prints typed records: top, owner and wait on stdout, run's progress events on stderr.
- `crew owner here|away|auto` overrides the owner-presence detection.
- Agent adoption is observe-only: `[adopt]` in `usher.toml` (`enabled`, `move`, `freeze`, `protect`, `freeze_psi_cpu`); `move` and `freeze` only log what they would do. Only named (`CREW_AGENT`), terminal-less, unprotected sessions are ever candidates.
- `CREW_STRICT=1` makes `crew run` fail when the daemon is down instead of running the command unmanaged.

Layers (checked by `crates/crew/tests/layers.rs`): `crew-proto`, `crew-sys` (0) < `crew-usher-client`, `crew-usher` (1) < `crew` (4).
Build and test through `hz cargo`; `CREW_REQUIRE_SCOPES=1` turns the real-scope tests' skip into a failure.
