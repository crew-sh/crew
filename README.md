# crew

Tools for running many AI coding agents on one Linux machine.

- **usher** is a small daemon that decides when each heavy job may start. `crew run`, `crew jobs`, `crew cancel`, `crew top`, `crew owner` and `crew wait` talk to it.
- **crew queue** is a merge queue. Agents submit branches. It lands them in batches and bisects a red batch.
- **crew plan** is a typed plan ledger. It records what was asked, what was added on top, who approved it, and how much each step used.
- **crew cargo** runs cargo through the [mbx](https://github.com/jdx/mr-boxington) build cache under an usher grant, and `crew build` shows what the cache did.

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
- Not on crates.io. Install a release binary or build from git.

## Install

The quick way is a prebuilt release (Linux, x86_64 or aarch64, statically linked):

```sh
curl -fsSL https://crew.umceko.dev/install.sh | sh
```

It downloads the latest release from GitHub, checks its sha256 against the release's `SHA256SUMS`, and installs the five programs into `~/.local/bin` (`CREW_INSTALL_DIR` picks another directory, `CREW_VERSION=v0.1.0` pins a release). Nothing is installed if the sum does not match. It also leaves a ready `crew-usher.service` in `~/.local/share/crew/` and prints the commands that enable it as a user service. Read the script first if you like: it is [site/install.sh](site/install.sh). Release files are on the [releases page](https://github.com/crew-sh/crew/releases).

Or build it yourself with `cargo install`. You need Rust (edition 2024, 1.97 or newer) and `protoc`, the protobuf compiler. The build compiles `.proto` files and does not bundle `protoc`.

```sh
sudo apt install protobuf-compiler          # Debian, Ubuntu
sudo dnf install protobuf-compiler          # Fedora
sudo pacman -S protobuf                     # Arch
```

If `protoc` is not on `PATH`, set `PROTOC=/path/to/protoc`.

```sh
cargo install --locked --git https://github.com/crew-sh/crew crew crew-usher crew-queue crew-ledger crew-depot
```

This installs five programs into `~/.cargo/bin`:

| Program | From crate | What it is |
|---|---|---|
| `crew` | `crew` | The command. It also runs `crew-<name>` for any other subcommand. |
| `crew-usher` | `crew-usher` | The admission daemon. |
| `crew-queue` | `crew-queue` | `crew queue ...` |
| `crew-plan` | `crew-ledger` | `crew plan ...` |
| `crew-depot` | `crew-depot` | `crew cargo ...` and `crew build ...` |

From a clone, install each crate (`cargo install` takes one `--path`):

```sh
for c in crew crew-usher crew-queue crew-ledger crew-depot; do cargo install --locked --path crates/$c; done
```

Make sure `~/.cargo/bin` (or the directory you installed into) is on `PATH`. `crew` looks for plugins next to itself, then on `PATH`.

## Quickstart

### 1. Start the usher

The usher is a user service. Copy the unit from this repo and start it:

```sh
mkdir -p ~/.config/systemd/user
cp contrib/crew-usher.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now crew-usher
```

The unit runs `%h/.cargo/bin/crew-usher`. Edit `ExecStart` if you installed elsewhere. (`install.sh` leaves a copy with `ExecStart` already pointing at its install directory, in `~/.local/share/crew/`.)

To tell when you are at the machine, the usher asks the Wayland compositor about idle time. That needs `WAYLAND_DISPLAY` in the user manager. Most desktop sessions import it already. If yours does not, run this once per login:

```sh
systemctl --user import-environment WAYLAND_DISPLAY
systemctl --user restart crew-usher
```

A user service stops with your last login session. To keep the usher, and the queue it keeps, running across a logout or a reboot nobody logs in after, run `loginctl enable-linger $USER` once.

Without it the usher still works. It assumes you are present until you say otherwise with `crew owner here|away`.

The usher never trusts a silent watcher. If the compositor connection ends, or never worked, the owner counts as present until an idle event arrives again, and the usher resubscribes with a growing delay (1 s up to 60 s). `crew top` shows `idle watcher lost, retrying` or `idle watcher unavailable, retrying` next to the owner line. A manual `crew owner away` ends after `manual_away_max_secs` (8 hours), or at the first input after an idle spell, so a forgotten one cannot admit benches forever. The usher logs which process set it. A manual `here` or `away` survives a usher restart: the usher keeps it, with the time it was set, in `usher-owner.pb` in its state directory, and an `away` that outlived its maximum while the usher was down is dropped.

One state directory has one usher: the usher takes an exclusive lock on `usher.lock` there and a second one refuses to start.

For a quick try, run `crew usher` in a terminal instead. It stays in the foreground.

### 2. Run a heavy command through it

```sh
crew run -- cargo build
```

The usher checks free memory, CPU and disk. It starts the command in its own scope at once, or queues it and says what it waits on. `crew run` exits with the command's exit code. It exits 75 when the usher refuses the job (for example, low disk) and 125 on a crew error.

It guesses the job kind from the command: `cargo build` is a build, `cargo test` a test, `make` a build, `pytest` a test. Force a kind with `--kind`, and set a size with `--mem 8G` or `--cpus 4`:

```sh
crew run --kind bench --mem 8G -- ./run-benchmarks.sh
crew run --kind bench --exclusive -- ./time-it.sh
```

A `bench` is heavy work that waits until you are away; it runs beside other jobs when memory, cores and disk allow. `--exclusive` asks for the machine to itself, for timing benchmarks: nothing else runs beside the job, and it too waits until you are away. Memory measurements and builds never need it.

"Waits until you are away" is about when a job starts. A bench or exclusive job that is already running when you come back is not paused: it gets the lower CPU and I/O weights for a present owner, and the usher logs `OWNER returned while away-only jobs run`, naming them. Cancel one with `crew cancel` if it must stop.

If the usher is not running when `crew run` starts, it prints a note and runs the command unmanaged. Set `CREW_STRICT=1` to make that an error.

A job whose memory can never fit is refused at once (exit 75) with the numbers: its expected size plus `min_free_mem_gib` must not exceed the machine's total memory. Lower `--mem` or `min_free_mem_gib`.

A job that is already queued never does that. If the usher restarts while `crew run` waits in its queue, `crew run` keeps the job id and its original queue time, retries the connection with a growing delay for up to five minutes, and queues the job again. It never runs the command on its own, so a bench that waits for you to be away still waits. Only when the usher stays gone for the whole five minutes does it fail (exit 125).

The queued message names its cause. A job that waits on memory prints the rule and the numbers, for example `needs 16.0 GiB + 16.0 GiB kept free = 32.0 GiB; 28.1 GiB available (31.0 GiB free - 2.9 GiB reserved by other jobs); queue position is not the cause`.

Every wait is on a resource: memory, cores, the game cap, disk, your presence, or a running exclusive job. Queue position alone never holds a job back, with two exceptions that say so:

- A job that has waited `starve_after_secs` (10 minutes) on memory or cores holds what it lacks. A later job still starts if it should end before the held job can start (by its learned duration), or if it fits in what the held job leaves spare once everything now running has ended. Otherwise it waits with the reason, for example `memory is held for test 1a2b3c4d (myrepo: cargo test), which has waited 11m on memory: ...`.
- An exclusive job lets others start beside the running ones while it cannot start anyway. Once it has waited `exclusive_drain_after_secs` (10 minutes) while you were away, the usher drains: no new job starts, the running ones finish, and it runs alone. Those jobs wait on `drain`, and `crew top` shows `draining for <job>`. An exclusive job therefore waits at most that long plus the longest job running when the drain began.

#### Detached jobs

A queued `crew run` lives only as long as its client. For a job that waits hours, or must outlive the agent that submitted it, hand it to the usher:

```sh
crew run --detach --kind bench -- ./run-benchmarks.sh
crew run --detach --log /tmp/bench.log -- ./run-benchmarks.sh
```

The usher takes the command, its working directory, the environment of the client and the limits. It then owns the job. It prints the job id on stdout, writes where the output goes on stderr, and exits 0 at once. `--json` also prints a `detached` event on stderr. The usher queues the job like any other, starts it in its own scope when it is granted, and appends its stdout and stderr to the log. The default log is `<state dir>/jobs/<id>.log` (the state dir is `$XDG_STATE_HOME/crew`). The client may exit, or its agent may end; nothing stops the job but `crew cancel`. Without a usher, `--detach` fails with exit 125, since there is nobody to own the job.

```sh
crew jobs                  # queued, running and recently finished jobs, with exit codes and log paths
crew jobs --json
crew wait job <id>         # blocks until a detached job finishes, then exits with its exit code
crew cancel <id>           # remove a queued job, or SIGTERM a running one (SIGKILL after 10 s)
```

`crew jobs` shows detached jobs and the jobs that `crew run` clients hold; it keeps the last 100 detached jobs that finished in the past 7 days. Their records, and the default logs the usher made for them, go after that. A log you chose with `--log` is never removed.

The usher writes its queued and running jobs, and the finished detached ones it keeps, to `$XDG_STATE_HOME/crew/usher-jobs.pb`, replacing the file in one step (synced to disk). A file that is cut off, damaged or from a newer usher is never read as an empty queue: it is moved to `usher-jobs.pb.corrupt-<time>` and the log says so. The same holds for `usher-sizes.pb`. A restarted usher reads the state back:

- A `crew run` job that was running keeps its place: its live scope is counted as a running job (reservations, exclusivity) until the scope ends, and the usher stops that scope if its `crew run` process has died meanwhile. A `crew run` client that was queued keeps its queue time when it comes back; a client's own claim of a queue time is not believed.

- Queued jobs return with their original queue time, so the order, and the 10-minute starvation rule, carry on as if nothing happened. The drain clock of an exclusive job starts again. A job queued before `--exclusive` existed is not exclusive.
- A running job whose scope is still alive is watched again, and its exit status comes from a file the job's wrapper writes (`<state dir>/jobs/<id>.status`).
- A running job whose scope is gone is marked finished if the status file is there, and failed otherwise. `crew jobs` shows the reason.

A job ends when its command ends. Anything the command left running in the job's scope (a server, a daemonised helper) is stopped with it, so nothing outlives a job unaccounted. If `systemd-run` itself fails (the unit name is taken, the user manager is down), the job is recorded as failed and never run, with the reason; it is not recorded as the command's exit 1.

The job's environment is the client's, minus `CREW_JOB`, and the usher applies the same rules as for any granted job: it removes your display variables, sets `TMPDIR` to a private directory and sets `CREW_JOB`. The usher keeps the environment in `usher-jobs.pb` (mode 0600, since environments hold secrets) until the job finishes, and drops it then.

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
KIND     AGENT      PROJECT       WAITED WAITING ON      STARTS    MEM  COMMAND
test     agent-b    myrepo           12s cpu            ~3m10s    2.0  cargo test
```

`STARTS` is when the usher expects the job to start: it replays its own rules as the running jobs end at their learned durations. It shows `-` while a job it waits on has never been timed, or while it waits for you to leave. `crew jobs` shows the same estimate and the full reason.

`crew top` also lists the agent sessions it sees (processes named `claude` or `codex`) and the projects they work in, with their CPU, memory and I/O.

### 4. Wait for the machine

```sh
crew wait queue-empty --timeout 10m
crew wait owner-away
crew wait job <id>
```

`wait` blocks until the usher reports the condition. It exits 0 when the condition holds, 124 on timeout, and 2 if the job never ran. The usher notifies. Nothing polls.

`crew wait job <id>` on a detached job waits until it finishes and exits with the job's exit code (a code outside 0 to 255 is a crew error, 125, never masked into another value). It exits 2 if the job never ran (cancelled while queued, refused, could not start) and 125 if it ran and was lost with no exit status. A job's own exit code can be 2, 124 or 125 too; `--json` has an `outcome` and an `exit_code` to tell them apart. For a job that a `crew run` client holds, `crew wait job <id>` returns as soon as the job is running or has finished, because that client has the exit code. `crew wait job-granted <id>` means that for any job, a detached one included.

Set the owner by hand with `crew owner here`, `crew owner away` or `crew owner auto`.

`crew run` guesses the kind of a command through `env`, `timeout`, `nice`, `time`, `mbx` and `sh -c "..."`, and past options before a cargo subcommand: `cargo --locked build`, `timeout 600 cargo test` and `env X=1 cargo bench` are what they say. `cargo bench` and `make bench` are benches. In a linked git worktree the `[usher]` rules come from the main checkout's `crew.toml`, so a branch cannot reclassify its own jobs.

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
crew plan approve docs                # owner only; `extra` steps cannot start without this
crew plan status flag running
crew plan compact                     # drop superseded status records, sum budget use; the old file is kept

crew queue worktree new flag          # a worktree and branch crew/flag; prints its path
# ... the agent works and commits in that worktree ...
crew queue submit crew/flag --step flag
crew queue run --once                 # land ready branches; omit --once to keep running
crew queue status
```

The queue lands branches in batches onto `integration_branch`. It runs your `check` commands once per batch, through `crew run` when a `crew` binary is found. When a batch is red, it bisects to the bad branch and lands the rest. A branch that touches a file another branch in the batch already changed waits for the next batch. A step lands only after its `deps` landed.

A new worktree starts warm: `crew queue worktree new` clones the `[queue] warm` folders (`target/`, `node_modules/`, `.venv/` by default) of the integration worktree into it as copy-on-write reflinks, so the agent's first build does not start cold. It needs a filesystem with reflinks (btrfs, XFS with `reflink=1`, bcachefs); on any other it clones nothing, says so in one line on stderr, and never falls back to a full copy. A worktree that already exists is not warmed again, a folder the source lacks is skipped, and `--no-warm` skips the step. The integration worktree is where the queue's checks run, so it has the build folders once the queue has run a batch.

`crew queue worktree done <step>` closes a worktree and drops its `target/`; only its owner may close a live one (`--owner-pid` acts for an owner). `crew queue gc` saves the work of a worktree whose owner process died and removes it, unless a process still works in it or the step was taken over meanwhile (checked again under the state lock, just before the removal); files `.gitignore` hides are not saved, and a failed save keeps the worktree. The owner of a new worktree is the agent session (`claude`, `codex`) above the caller, else the session leader; pass `--owner-pid` to say otherwise. A branch with no commits beyond `base` fails its step instead of landing. A check that is killed by a signal, or that `crew run` refuses or fails, is run once more and then stops the queue without blaming a step. A looping `crew queue run` that hits an error (a usher refusal, a git hiccup, such a check) logs it, waits (3 s, doubling up to a minute) and tries again; `--once` still stops with the error. A check's output goes to a file and only its last lines are kept, so a chatty check costs no memory. A runner started while `crew queue status` looks at the runner lock waits up to 2 s for it. `crew queue final` runs the `final` commands on the integration tip.

Approvals come from the owner, not from an agent. `crew plan approve` shows the step (or the steps `scope` covers) and asks you to type its id back; it works only from a terminal, and exits 1 with `approvals come from the owner` when `CREW_AGENT` is set, when it runs inside a crew job, or when stdin or stdout is not a terminal. An approval records who gave it and a hash of the step, so a step that is edited afterwards (a bigger budget, another kind of review) needs a new approval, and `crew plan check` reports a step that runs without one. Approvals recorded by an earlier version keep counting.

A script that approves for you uses `--owner-confirmed` together with the environment variable `CREW_OWNER_TOKEN`, which must equal the token in `~/.config/crew/owner-token` (mode 0600; `crew plan owner-token` writes it, from a terminal only). Put the token in your own shell, never in an agent's environment, and tell agents not to read the file. This guards against mistakes; it is not a security boundary, because agents run as the same user and could read the file.

Statuses follow rules: a step becomes `landed` only through a landing record with the merge sha (the queue writes it, or `crew plan landed <id> <sha>`), only from approved, running or built, and only after its `deps` landed; a landed step stays landed; a dropped step can only go back to `planned`. Step ids use letters, digits, `.`, `_` and `-`, not starting with `.` or `-`. A ledger record whose length is damaged in the middle of the file is an error (nothing after it is dropped); a cut-off last record is ignored, and a copy of the file is kept before the next write cuts it off.

### 6. Build through the cache

`crew cargo` is `cargo` for a machine with many agents. It asks the usher for a build grant, then runs `mbx <cargo arguments>` when [mbx](https://github.com/jdx/mr-boxington) is installed and plain `cargo` when it is not:

```sh
crew cargo build --workspace
crew cargo nextest run
crew build stats                      # what this project's builds got from the cache
crew build why serde_derive           # why a crate missed in the last build
crew build report --shared ../other   # which dependency units ../other would share, or why not
```

What the grant changes: the build runs with at most the granted cores (`CARGO_BUILD_JOBS`, unless you pass `-j`), and with mbx's `low` scheduler priority while you are at the machine (`MBX_SCHEDULER_PRIORITY`). Nothing you set yourself is overridden. A leftover `RUSTC_WRAPPER` is dropped for mbx, with a note, because mbx caches nothing when another wrapper holds that slot.

The per-project record behind `crew build stats` is written through a synced file. One that cannot be read is moved to `<name>.corrupt-<time>` with a message, never silently replaced, and run reports that a killed `crew cargo` left behind are removed after a day.

`crew build report` runs `cargo build --unit-graph` in both projects (with `RUSTC_BOOTSTRAP=1` on that one planning command, since the flag is unstable; no compiler runs). For every registry or git unit of this project it prints `shared`, or the first reason it is not: the compiler, the lock file's version, features, a profile field, or a dependency. It reports and edits nothing.

## Tell your agents

Paste this into `AGENTS.md`, `CLAUDE.md` or your agent's system prompt:

```markdown
## Shared machine

Other agents run on this machine. Use crew so we do not starve each other.

- Run heavy commands as `crew run -- <command>`: builds, test suites, benchmarks, anything that uses many cores or much memory. Build Rust with `crew cargo <args>`; it goes through the same admission.
- A job that will wait for hours, or that must keep waiting after you are gone, goes in as `crew run --detach -- <command>`. The usher keeps it, even across its own restarts. Follow it with `crew wait job <id>`, look at `crew jobs`, read the log it names.
- Run `crew top` to see what is running and what you are waiting on. Do not start a second copy of a job that is already queued.
- Pass `--exclusive` only when a measurement needs the machine to itself (timings). Memory measurements, builds and tests never do; `--kind bench` alone already waits until the owner is away.
- Never run `crew plan approve`, and never read `~/.config/crew/owner-token`. Approvals come from the owner. If a step is gated, report it and wait.
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
exclusive_drain_after_secs = 600
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
| `min_free_disk_gib` | Path to GiB. Build, test and harness jobs are refused while a path has less free space than listed, or cannot be measured. The path must also be in `watched_paths`; a written threshold for another path is an error. Default: `/` at 20. |
| `tmp_root` | Each job gets `TMPDIR=<tmp_root>/<job id>`, a directory only you can enter. Default: `$XDG_RUNTIME_DIR/crew/tmp`, else `<system temp dir>/crew`. A root owned by another user is refused (the job runs without a private `TMPDIR`). At start the usher removes job directories (UUID-named, older than 5 minutes) that no scope or known job holds. |
| `owner_present_weights` | While you are present, running jobs get at most this cgroup `cpu.weight` and `io.weight` (the normal weight is 100). |
| `idle_timeout_secs` | Seconds without input before the compositor calls you idle. |
| `starve_after_secs` | A job that waited this long on memory or cores holds what it lacks: later jobs start only where they would not delay it. |
| `exclusive_drain_after_secs` | An exclusive job that waited this long while you were away stops new jobs from starting, so the running ones finish and it can run. |
| `manual_away_max_secs` | A manual `crew owner away` ends after this long. Default 8 hours. |
| `learn_min_wall_secs` | Runs shorter than this, and runs that failed, teach the usher nothing about a command's size. Default 5. |
| `max_connections` | Open client connections beyond this are turned away. Every queued `crew run` and every waiter holds one. Default 512. |
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
| `exclusive` | Every job of the kind runs alone, and only while you are away, as with `--exclusive`. |
| `away_only` | Jobs of the kind run only while you are away. |

Built-in values: every kind expects 2 GiB, 1 core and weight 100. Exceptions: `build` expects 4 cores. `game` is capped at 6 GiB and expects 6. `bench` runs only while you are away (it is not exclusive; `--exclusive` asks for that). `lsp` has `cpu_weight` 20.

Once the usher has seen a command run, it reserves the median of its last 20 successful runs instead of these numbers (and estimates start times from their median duration), held between half and eight times the kind's expectation. A size you give with `--mem` always wins. The figure is the job's working set (anonymous, shared and kernel memory), sampled while it runs: page cache a build writes is not counted. Commands unseen for 28 days are forgotten. It keeps them in `$XDG_STATE_HOME/crew/usher-sizes.pb`. The log is `usher.log` in the same directory. Detached jobs keep `usher-jobs.pb` and a `jobs/` folder there too.

### `depot.toml` and `[depot]`

Path: `$XDG_CONFIG_HOME/crew/depot.toml`, normally `~/.config/crew/depot.toml`. It holds one `[depot]` table. The same table in a project's `crew.toml` overrides it key by key, and profiles merge field by field, except `mbx_path` and `store`: a project comes with a checked-out branch, and a branch must not choose which program `crew cargo` runs or where it writes, so `crew cargo` ignores those two keys there and says so. Every key is optional. Unknown keys inside `[depot]` are an error. `crew cargo` reads both files on each run. The usher reads only `depot.toml`, at start, so the pool keys (`mbx`, `mbx_path`, `mbx_reserve`, `reserve_cpus`, `reserve_memory_gib`) are user-level for it: when a project's `crew.toml` changes one, `crew cargo` obeys it and says that the usher does not. `crew cargo` also says when an inherited `RUSTUP_TOOLCHAIN` or `CARGO_BUILD_JOBS` overrides a setting or the grant, and when `RUSTFLAGS` or `RUSTC_BOOTSTRAP` is set while mbx is used (they change the cache keys).

<!-- test:depot-example -->
```toml
[depot]
toolchain = "1.99.1"
mbx_reserve = true
reserve_cpus = 4
reserve_memory_gib = 8

[depot.profiles.fast]
lto = "off"
codegen-units = 256
```

| Key | Default | Meaning |
|---|---|---|
| `toolchain` | none | A rustup toolchain every `crew cargo` build uses (`RUSTUP_TOOLCHAIN`). A `+toolchain` argument or an existing `RUSTUP_TOOLCHAIN` wins. One toolchain per machine is what lets projects share compiled dependencies. |
| `mbx` | `"auto"` | `"auto"` uses mbx when it is installed and says so when it is not. `"off"` never does. `"required"` fails when mbx is missing. |
| `mbx_path` | none | The mbx program. Default: `PATH`, then `$CARGO_HOME/bin`. Only `depot.toml` may set it. |
| `mbx_reserve` | `false` | While the owner is present the usher holds `reserve_cpus` cores and `reserve_memory_gib` GiB of mbx's compile pool (`mbx reserve`), and `crew run` wraps games and harnesses in `mbx reserve` for what the usher expects them to use. Compilers then leave room. |
| `reserve_cpus` | `4` | Cores the usher holds while the owner is present. |
| `reserve_memory_gib` | `8` | Memory the usher holds while the owner is present. |
| `store` | `$XDG_CACHE_HOME/crew` | Where `crew build` keeps its per-project statistics. Only `depot.toml` may set it. |
| `store_budget_gib` | `60` | Reserved for the snapshot store of a later milestone. Not enforced yet. |
| `profiles.<name>` | none | `lto` (`"off"`, `"thin"`, `"fat"`) and `codegen-units` for one cargo profile, set as `CARGO_PROFILE_<NAME>_LTO` and `_CODEGEN_UNITS` for agent builds. Off unless you write it: it changes the generated code, so measure the binary first. With `lto = "off"` and many codegen units, mbx can keep incremental state for an optimized profile. |

The compile budget in a grant is the kind's `expected_cores` (see `[kinds.build]` above), never lower, and the priority is `low` while the owner is present.

### `crew.toml`

One file at the repo root. Each tool reads its own table and ignores the others.

| Table | Read by | Unknown keys |
|---|---|---|
| `[plan]` | `crew plan` | ignored |
| `[queue]` | `crew queue` | error |
| `[usher]` | `crew run` | error |
| `[depot]` | `crew cargo`, `crew build` | error |

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

`extra` and `owner-before` steps cannot start until `crew plan approve <id>`. `crew plan approve scope` approves the `extra` steps that need no review before, as listed when you approve; an extra added later needs its own approval. Budgets are advisory: `crew plan may-run` and `crew plan check` report an overspend, and nothing stops a process.

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
| `warm` | `["target", "node_modules", ".venv"]` | Folders of the integration worktree that `crew queue worktree new` clones into a new worktree. Relative names only. |

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
| `CREW_STRICT` | `crew run`, `crew cargo` | `1`: fail when the usher is down instead of running unmanaged. (A job already queued never runs unmanaged, with or without it.) |
| `CREW_JOB` | set by `crew run` | Set inside a granted job. A nested `crew run` runs in that job instead of queueing behind it, but only when the usher confirms that the caller's own process is inside that running job's scope. A leaked or exported value is ignored and the job is admitted as usual. |
| `CREW_OWNER_TOKEN` | `crew plan approve --owner-confirmed` | The owner's token, for scripts. Keep it out of agents' environments. |
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
| 1 | `crew-depot-config` | The `[depot]` table: toolchain pin, mbx use, profile overrides, pool reserve. |
| 2 | `crew-queue` | The merge queue. Program: `crew-queue`. |
| 2 | `crew-depot` | `crew cargo` and `crew build`: cargo through mbx under a build grant, statistics, miss explanations, the sharing report. Program: `crew-depot`. |
| 4 | `crew` | The command line. |

`crew-queue` also runs the layer-4 `crew` binary at run time to start its checks under admission (`CREW_BIN`, else a `crew` beside it, else one on `PATH`; none means the checks run directly). That is a process it starts, not a dependency, so the layer test cannot see it. Only normal dependencies are pinned per crate; dev dependencies obey the layers only.

Why separate programs:

- `crew <name>` runs `crew-<name>`, as `git` and `cargo` do. Install only what you use. `crew plan` and `crew queue` never need the usher.
- The usher is a long-lived daemon with a Wayland client. The command line does not link it.
- The usher and its clients speak length-prefixed protobuf over a Unix socket. Any program can be a client. `crew queue` is one.

## Safety

- crew never opens a window or takes over a display. The usher reads idle time from the Wayland compositor and does nothing else with it. It removes `DISPLAY`, `WAYLAND_DISPLAY` and `WAYLAND_SOCKET` from job environments, so a test cannot open a window on your desktop. A private X display such as `:99` is kept. `--display` jobs are refused while you are present. `CREW_DESKTOP=1` is the one opt-out.
- The usher kills only its own scopes: a job's scope when its `crew run` goes away or receives SIGTERM, and a detached job's scope on `crew cancel`. It never signals other processes.
- Agent sessions (processes named `claude` or `codex`) are observed, not managed. `[adopt]` can move a session into a crew cgroup and freeze the busiest one under CPU pressure. Neither is implemented yet. `move` and `freeze` default to `false`, and when set they only write "would do" lines to the log. Sessions with a terminal, without `CREW_AGENT`, or listed in `protect` are never candidates.
- The usher runs as you and listens on a socket in a `0700` directory. There is no authentication beyond that.
- `crew queue` runs your `check` commands with your permissions, in worktrees under `.crew/`.
- The usher answers anyone who can open its socket. `JobDone` is accepted only from the connection the job was granted to, a queued job is not taken over by another process, and ids are plain (1 to 64 letters, digits, dashes). The state directory lock, the connection cap and the owner rules above keep a mistake from doing damage; none of this stops a hostile process of the same user.

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
