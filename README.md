# crew

Tooling for parallel AI coding agents. This workspace holds the MVP of `usher`: a machine-wide admission daemon and its CLI.

- `crew usher` runs the daemon (`crew-usher`) in the foreground; config is `~/.config/crew/usher.toml` (see `crates/crew-usher/src/config.rs` for every key).
- `crew run [--kind K] [--mem 8G] [--cpus N] [--display] -- CMD...` asks the daemon for admission, then runs CMD in its own systemd user scope.
- `crew top [--watch]` shows running and queued jobs and what each queued job waits on.
- `crew owner here|away|auto` overrides the owner-presence detection.
- `CREW_STRICT=1` makes `crew run` fail when the daemon is down instead of running the command unmanaged.

Layers (checked by `crates/crew/tests/layers.rs`): `crew-proto`, `crew-sys` (0) < `crew-usher-client`, `crew-usher` (1) < `crew` (4).
Build and test through `hz cargo`; `CREW_REQUIRE_SCOPES=1` turns the real-scope tests' skip into a failure.
