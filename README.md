# crew

Tools for running many AI coding agents on one Linux machine.

- **usher** is a small daemon that decides when each heavy job may start. `crew run`, `crew top`, `crew owner` and `crew wait` talk to it.
- **crew queue** is a merge queue. Agents submit branches. It lands them in batches and bisects a red batch.
- **crew plan** is a typed plan ledger. It records what was asked, what was added on top, who approved it, and how much each step used.

## Why

Run five agents on one box and the same things go wrong:

- Every agent starts `cargo build` at once. The CPU is gone, memory runs out, the disk fills up, and your own desktop stalls.
- A test or a game opens a window on the desktop you are sitting at.
- Agents merge one at a time, each rebuilding the world, and a red merge blocks the rest.
- A plan starts with three steps and quietly grows to ten. Nobody approved the extra seven.

crew gives each problem one small, separate tool.

## Status

Early. It works and has tests, but the formats may still change.

- Linux with a systemd user session only. Jobs run in `systemd-run --user --scope` scopes, so cgroup v2 is needed.
- Agent accounting is observe-only. See [Safety](#safety).
- Not on crates.io. Install from git.

## Install

You need Rust (edition 2024, 1.97 or newer) and `protoc`, the protobuf compiler. The build compiles `.proto` files and does not bundle `protoc`.

```sh
sudo apt install protobuf-compiler          # Debian, Ubuntu
sudo dnf install protobuf-compiler          # Fedora
sudo pacman -S protobuf                     # Arch
```

If `protoc` is not on `PATH`, set `PROTOC=/path/to/protoc`.

```sh
cargo install --locked --git https://github.com/crew-sh/crew crew crew-usher crew-queue crew-ledger
```

This installs four programs into `~/.cargo/bin`:

| Program | From crate | What it is |
|---|---|---|
| `crew` | `crew` | The command. It also runs `crew-<name>` for any other subcommand. |
| `crew-usher` | `crew-usher` | The admission daemon. |
| `crew-queue` | `crew-queue` | `crew queue ...` |
| `crew-plan` | `crew-ledger` | `crew plan ...` |

From a clone, install each crate (`cargo install` takes one `--path`):

```sh
for c in crew crew-usher crew-queue crew-ledger; do cargo install --locked --path crates/$c; done
```

Make sure `~/.cargo/bin` is on `PATH`. `crew` looks for plugins next to itself, then on `PATH`.

## Quickstart

### 1. Start the usher

The usher is a user service. Copy the unit from this repo and start it:

```sh
mkdir -p ~/.config/systemd/user
cp contrib/crew-usher.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now crew-usher
```

The unit runs `%h/.cargo/bin/crew-usher`. Edit `ExecStart` if you installed elsewhere.

To tell when you are at the machine, the usher asks the Wayland compositor about idle time. That needs `WAYLAND_DISPLAY` in the user manager. Most desktop sessions import it already. If yours does not, run this once per login:

```sh
systemctl --user import-environment WAYLAND_DISPLAY
systemctl --user restart crew-usher
```

Without it the usher still works. It assumes you are present until you say otherwise with `crew owner here|away`.

For a quick try, run `crew usher` in a terminal instead. It stays in the foreground.

### 2. Run a heavy command through it

```sh
crew run -- cargo build
```

The usher checks free memory, CPU and disk. It starts the command in its own scope at once, or queues it and says what it waits on. `crew run` exits with the command's exit code. It exits 75 when the usher refuses the job (for example, low disk) and 125 on a crew error.

It guesses the job kind from the command: `cargo build` is a build, `cargo test` a test, `make` a build, `pytest` a test. Force a kind with `--kind`, and set a size with `--mem 8G` or `--cpus 4`:

```sh
crew run --kind bench --mem 8G -- ./run-benchmarks.sh
```

If the usher is not running, `crew run` prints a note and runs the command unmanaged. Set `CREW_STRICT=1` to make that an error.

### 3. Look at the machine

```sh
crew top            # once
crew top --watch    # refresh every 2 seconds
crew top --json     # typed JSON, for scripts and agents
```

```
memory   27.1 of 61.9 GiB available
cpu      24 cores, load 17.66, unmanaged 0%
psi      cpu 2.3  mem 0.0  io 28.2
disk     / 166.5 GiB free (min 20.0)
owner    away (manual)

RUNNING (1)
KIND     AGENT      PROJECT          AGE CPU act/exp MEM act/exp GiB       IO  COMMAND
build    agent-a    myrepo           41s  3.8/4.0     1.9/2.0        120M  cargo build

QUEUED (1)
KIND     AGENT      PROJECT       WAITED WAITING ON        MEM  COMMAND
test     agent-b    myrepo           12s cpu               2.0  cargo test
```

`crew top` also lists the agent sessions it sees (processes named `claude` or `codex`) and the projects they work in, with their CPU, memory and I/O.

### 4. Wait for the machine

```sh
crew wait queue-empty --timeout 10m
crew wait owner-away
crew wait job <id>
```

`wait` blocks until the usher reports the condition. It exits 0 when the condition holds, 124 on timeout, and 2 if the job never ran. The usher notifies. Nothing polls.

Set the owner by hand with `crew owner here`, `crew owner away` or `crew owner auto`.

### 5. Plan work and merge it

`crew plan` and `crew queue` read `crew.toml` at the root of your repo (they search upward from the current directory). Here is a minimal one:

<!-- test:crew-toml -->
```toml
[plan]
request = "Add a --json flag to the export command"

[queue]
integration_branch = "crew/integration"
base = "main"
check = ["cargo test"]
```

The steps live in `plan.toml`, next to it:

<!-- test:plan-toml -->
```toml
[[step]]
id = "flag"
title = "Parse and apply --json"
kind = "asked"
owns = ["src/export/**"]
budget = { builds = 5, test_minutes = 10 }

[[step]]
id = "docs"
title = "Document the flag"
kind = "extra"
deps = ["flag"]
owns = ["docs/**"]
```

Add `.crew/` to your `.gitignore`. The ledger and queue state live there.

```sh
crew plan check                       # rules; exit 1 on a violation
crew plan show                        # counts per kind, then the steps
crew plan next                        # steps that are ready, in plan order
crew plan approve docs                # `extra` steps cannot start without this
crew plan status flag running

crew queue worktree new flag          # a worktree and branch crew/flag; prints its path
# ... the agent works and commits in that worktree ...
crew queue submit crew/flag --step flag
crew queue run --once                 # land ready branches; omit --once to keep running
crew queue status
```

The queue lands branches in batches onto `integration_branch`. It runs your `check` commands once per batch, through `crew run` when a `crew` binary is found. When a batch is red, it bisects to the bad branch and lands the rest. A branch that touches a file another branch in the batch already changed waits for the next batch. A step lands only after its `deps` landed.

`crew queue gc` saves the work of a worktree whose owner process died and removes it. `crew queue final` runs the `final` commands on the integration tip.

## Tell your agents

Paste this into `AGENTS.md`, `CLAUDE.md` or your agent's system prompt:

```markdown
## Shared machine

Other agents run on this machine. Use crew so we do not starve each other.

- Run heavy commands as `crew run -- <command>`: builds, test suites, benchmarks, anything that uses many cores or much memory.
- Run `crew top` to see what is running and what you are waiting on. Do not start a second copy of a job that is already queued.
- `crew run` exits 75 when it refuses a job (low disk, no display while the owner is here). Do not retry in a loop. Report it.
- Do not open windows. crew removes DISPLAY and WAYLAND_DISPLAY from your jobs on purpose.
- Set `CREW_AGENT=<your name>` so crew can tell sessions apart.
- Merge with `crew queue submit <branch> --step <id>`. Never push to the integration branch yourself.
- Before a step runs more builds, `crew plan may-run <id> build` must exit 0. Record usage with `crew plan use <id> --builds 1`.
```

## Configuration

### `usher.toml`

Path: `$XDG_CONFIG_HOME/crew/usher.toml`, normally `~/.config/crew/usher.toml`. It is optional. Every key is optional. Unknown keys are an error. Restart the usher after a change.

These are the defaults:

<!-- test:usher-defaults -->
```toml
min_free_mem_gib = 16
max_games = 4
cpu_oversub = 1.5
idle_timeout_secs = 120
starve_after_secs = 600
min_free_disk_gib = { "/" = 20 }

[owner_present_weights]
cpu = 20
io = 20

[adopt]
enabled = true
move = false
freeze = false
protect = []
freeze_psi_cpu = 40.0
```

| Key | Meaning |
|---|---|
| `min_free_mem_gib` | Memory kept free. A job waits unless available memory, minus what running jobs have reserved, covers this plus the job's expected size. |
| `max_games` | How many `game` jobs may run at once. `0` refuses them. |
| `cpu_oversub` | Granted cores may add up to `cpu count × this`, minus cores used by processes outside crew. |
| `watched_paths` | Paths whose free disk space the usher measures and `crew top` shows. Default: `/`, plus `$HOME` when it is on another filesystem. |
| `min_free_disk_gib` | Path to GiB. Build, test and harness jobs are refused while a path has less free space than listed. The path must also be in `watched_paths`. Default: `/` at 20. |
| `tmp_root` | Each job gets `TMPDIR=<tmp_root>/<job id>`. Default: `$XDG_RUNTIME_DIR/crew/tmp`, else `<system temp dir>/crew`. |
| `owner_present_weights` | While you are present, running jobs get at most this cgroup `cpu.weight` and `io.weight` (the normal weight is 100). |
| `idle_timeout_secs` | Seconds without input before the compositor calls you idle. |
| `starve_after_secs` | A job that waited this long stops later jobs from jumping ahead of it. |
| `adopt.*` | Agent sessions. See [Safety](#safety). |

`$XDG_RUNTIME_DIR` is usually a small RAM-backed filesystem. If your builds write large temporary files, point `tmp_root` at a big disk, and list that disk in `watched_paths` and `min_free_disk_gib`.

Per-kind settings go under `[kinds.<kind>]`. The kinds are `build`, `test`, `harness`, `game`, `bench`, `lsp` and `other`. Any key you leave out keeps the built-in value.

<!-- test:usher-overrides -->
```toml
watched_paths = ["/", "/data"]
tmp_root = "/data/crew-tmp"
min_free_disk_gib = { "/" = 20, "/data" = 100 }

[kinds.build]
expected_cores = 8.0
expected_mem_gib = 4

[kinds.game]
mem_cap_gib = 8
```

| Key | Meaning |
|---|---|
| `mem_cap_gib` | Hard memory limit for the scope. |
| `expected_mem_gib` | Memory the usher reserves for the job. |
| `expected_cores` | Cores the usher reserves for the job. |
| `cpu_weight`, `io_weight` | cgroup weights. |
| `exclusive` | The job runs alone, and only while you are away. |

Built-in values: every kind expects 2 GiB, 1 core and weight 100. Exceptions: `build` expects 4 cores. `game` is capped at 6 GiB and expects 6. `bench` is exclusive. `lsp` has `cpu_weight` 20.

Once the usher has seen a command run, it reserves the median of its last 20 runs instead of these numbers. It keeps them in `$XDG_STATE_HOME/crew/usher-sizes.pb`. The log is `usher.log` in the same directory.

### `crew.toml`

One file at the repo root. Each tool reads its own table and ignores the others.

| Table | Read by | Unknown keys |
|---|---|---|
| `[plan]` | `crew plan` | ignored |
| `[queue]` | `crew queue` | error |
| `[usher]` | `crew run` | error |

`[plan]`

| Key | Default | Meaning |
|---|---|---|
| `request` | none | The request the plan serves, in a sentence. `crew plan check` fails without it. |
| `file` | `plan.toml` | The steps file, relative to the root. |
| `ledger` | `.crew/ledger.pb` | The append-only ledger. |

`plan.toml` holds `[[step]]` tables. Unknown keys are an error.

| Key | Meaning |
|---|---|
| `id`, `title` | Required. `scope` is reserved. |
| `kind` | Required. `asked`, `extra` or `core-design`. |
| `deps` | Step ids that must land first. |
| `model` | `sonnet` (default), `opus`, `haiku` or `fable`. A label for who should do it. |
| `owns` | Globs of the files the step may change. Two steps with overlapping globs need a dependency path between them. |
| `verify` | Notes on how to verify the step. Recorded, not run. |
| `budget` | `builds`, `test_minutes`, `harnesses`, `games`. A missing limit is unlimited. |
| `review` | `owner-before`, `owner-after` or `none`. Default: `owner-before` for `core-design`, else `none`. |
| `issue` | An issue number. |

`extra` and `owner-before` steps cannot start until `crew plan approve <id>`. `crew plan approve scope` approves all `extra` steps that need no review before. Budgets are advisory: `crew plan may-run` and `crew plan check` report an overspend, and nothing stops a process.

`[queue]`

| Key | Default | Meaning |
|---|---|---|
| `integration_branch` | required | Where branches land. Do not keep it checked out. |
| `base` | required | The branch step branches start from. |
| `check` | `[]` | Shell commands run for every batch. |
| `final` | `[]` | Shell commands for `crew queue final`. |
| `push` | none | `{ remote = "origin", branch = "main" }`: push green batches. |
| `max_batch` | `8` | Most branches in one batch. |
| `branch_prefix` | `crew/` | Step branches are `<prefix><step id>`. |
| `worktree_root` | `.crew/wt` | Where `crew queue worktree new` puts worktrees. |

`[usher]` holds project-specific job kinds for `crew run`. The built-in guesses cover `cargo`, `go`, `npm`/`pnpm test`, `pytest` and `make`. Your rules are checked first.

<!-- test:usher-table -->
```toml
[usher]
wrappers = ["time"]

[usher.kinds]
game = ["my-game", "my-ctl launch"]
harness = ["python3 tools/e2e.py"]
```

Each entry is a command prefix. `"my-ctl launch"` matches `my-ctl launch --world W`, not `my-ctl state`. The first word is compared with the program's file name, and the other words must match the arguments exactly. The longest matching prefix wins. `wrappers` are programs that run the rest of the command line (`time`, `nice`, your own script). `crew run` skips them before matching. Kinds: `build`, `test`, `harness`, `game`, `bench`, `lsp`, `other`.

A broken `[usher]` table, a `crew.toml` that is not valid TOML, or an unreadable `crew.toml` makes `crew run` stop with exit 125. Passing `--kind` skips the file.

### Environment

| Variable | Used by | Meaning |
|---|---|---|
| `CREW_AGENT` | all | Your agent name. `crew plan` falls back to `$USER`. |
| `CREW_SOCKET` | `crew`, `crew-usher` | The usher socket. Default `$XDG_RUNTIME_DIR/crew/usher.sock`. |
| `CREW_STRICT` | `crew run` | `1`: fail when the usher is down instead of running unmanaged. |
| `CREW_JOB` | set by `crew run` | Set inside a granted job. A nested `crew run` runs in that job instead of queueing behind it. |
| `CREW_DESKTOP` | `crew run` | `1`: keep the owner's display variables in the job (never for `bench`). |
| `CREW_ROOT` | `crew plan` | The project root, instead of searching upward. |
| `CREW_BIN` | `crew queue` | The `crew` binary that runs check commands. |

## Architecture

Each crate depends only on crates in its own layer or below. `crates/crew/tests/layers.rs` checks this.

| Layer | Crate | Role |
|---|---|---|
| 0 | `crew-proto` | Protobuf messages and framing, shared by every program. |
| 0 | `crew-sys` | Linux access: `/proc`, cgroups, `systemd-run`, display variables, Wayland idle. |
| 0 | `crew-git` | Worktrees, merges, ancestry, salvage. Runs the `git` binary. |
| 1 | `crew-usher-client` | Job requests, kind guessing, the scope runner, `crew top` text and JSON. |
| 1 | `crew-usher` | The daemon: admission, queue, owner presence, agent accounting. |
| 1 | `crew-ledger` | `plan.toml`, the ledger, the rules. Program: `crew-plan`. |
| 2 | `crew-queue` | The merge queue. Program: `crew-queue`. |
| 4 | `crew` | The command line. |

Why separate programs:

- `crew <name>` runs `crew-<name>`, as `git` and `cargo` do. Install only what you use. `crew plan` and `crew queue` never need the usher.
- The usher is a long-lived daemon with a Wayland client. The command line does not link it.
- The usher and its clients speak length-prefixed protobuf over a Unix socket. Any program can be a client. `crew queue` is one.

## Safety

- crew never opens a window or takes over a display. The usher reads idle time from the Wayland compositor and does nothing else with it. It removes `DISPLAY`, `WAYLAND_DISPLAY` and `WAYLAND_SOCKET` from job environments, so a test cannot open a window on your desktop. A private X display such as `:99` is kept. `--display` jobs are refused while you are present. `CREW_DESKTOP=1` is the one opt-out.
- The usher kills only its own scopes: a job's scope when its `crew run` goes away or receives SIGTERM. It never signals other processes.
- Agent sessions (processes named `claude` or `codex`) are observed, not managed. `[adopt]` can move a session into a crew cgroup and freeze the busiest one under CPU pressure. Neither is implemented yet. `move` and `freeze` default to `false`, and when set they only write "would do" lines to the log. Sessions with a terminal, without `CREW_AGENT`, or listed in `protect` are never candidates.
- The usher runs as you and listens on a socket in a `0700` directory. There is no authentication beyond that.
- `crew queue` runs your `check` commands with your permissions, in worktrees under `.crew/`.

## Development

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace        # or: cargo nextest run --workspace
```

Tests that need `systemd-run --user --scope` skip with a note when it is not available. Set `CREW_REQUIRE_SCOPES=1` to make a skip a failure. Tests create scratch files in the system temp directory. Set `CREW_TEST_TMP` to use another one. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Copyright 2026 crew contributors.
