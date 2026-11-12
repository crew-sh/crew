use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crew_proto::{AccountingSource, AgentKind, AgentView, ProjectView};
use crew_sys::procs::{self, ProcStat};
use crew_sys::{paths, scope};

use crate::config::Adopt;

const MAX_DEPTH: usize = 64;

#[derive(Clone, Debug, PartialEq)]
pub struct Agent {
    pub name: String,
    pub kind: AgentKind,
    pub pid: u32,
    pub project: String,
    pub toplevel: String,
    pub cwd: String,
    pub processes: u32,
    pub cpu_usec: u64,
    pub cpu_milli: u32,
    pub mem_bytes: u64,
    pub io_read: u64,
    pub io_write: u64,
    pub cgroup: String,
    pub source: AccountingSource,
    pub age_ms: u64,
    pub named: bool,
    pub has_tty: bool,
    pub actionable: bool,
}

impl Agent {
    pub fn view(&self) -> AgentView {
        AgentView {
            name: self.name.clone(),
            kind: self.kind as i32,
            pid: self.pid,
            project: self.project.clone(),
            cwd: self.cwd.clone(),
            processes: self.processes,
            cpu_usec: self.cpu_usec,
            cpu_milli: self.cpu_milli,
            mem_bytes: self.mem_bytes,
            io_read_bytes: self.io_read,
            io_write_bytes: self.io_write,
            cgroup: self.cgroup.clone(),
            source: self.source as i32,
            age_ms: self.age_ms,
            named: self.named,
            has_tty: self.has_tty,
            actionable: self.actionable,
        }
    }
}

/// Kernel command names: `claude`, `codex`, and the `.claude-wrapped` Nix wrappers make.
fn agent_kind(comm: &str) -> Option<AgentKind> {
    match comm.trim_start_matches('.').trim_end_matches("-wrapped") {
        "claude" => Some(AgentKind::Claude),
        "codex" => Some(AgentKind::Codex),
        _ => None,
    }
}

fn kind_label(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Codex => "codex",
        AgentKind::Claude | AgentKind::Unspecified => "claude",
    }
}

#[derive(Default)]
struct Totals {
    cpu_usec: u64,
    mem_bytes: u64,
    io_read: u64,
    io_write: u64,
}

pub struct Collector {
    proc_root: PathBuf,
    cgroup_root: PathBuf,
    protect: Vec<String>,
    prev: HashMap<u32, (u64, Instant, u64)>,
}

impl Collector {
    pub fn new(proc_root: &Path, cgroup_root: &Path, protect: Vec<String>) -> Self {
        Self {
            proc_root: proc_root.to_path_buf(),
            cgroup_root: cgroup_root.to_path_buf(),
            protect,
            prev: HashMap::new(),
        }
    }

    /// Scans /proc once; sessions are the outermost claude/codex processes with all their descendants.
    pub fn collect(&mut self, now: Instant) -> Vec<Agent> {
        let stats = procs::read_all_stats(&self.proc_root);
        let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for s in stats.values() {
            children.entry(s.ppid).or_default().push(s.pid);
        }
        let uptime_ms = self.uptime_ms();
        let hz = procs::clock_ticks_per_sec();
        let mut out = Vec::new();
        let mut live = HashMap::new();
        for s in stats.values() {
            let Some(kind) = agent_kind(&s.comm) else {
                continue;
            };
            if is_nested(s, &stats) {
                continue;
            }
            let members = tree(s.pid, &children);
            let mut agent = self.account(s, kind, &members, &stats, hz);
            agent.age_ms = uptime_ms.saturating_sub(s.start_ticks * 1000 / hz);
            agent.cpu_milli = self.rate(s, agent.cpu_usec, now);
            live.insert(s.pid, (s.start_ticks, now, agent.cpu_usec));
            out.push(agent);
        }
        self.prev = live;
        out.sort_by(|a, b| a.name.cmp(&b.name).then(a.pid.cmp(&b.pid)));
        out
    }

    fn uptime_ms(&self) -> u64 {
        std::fs::read_to_string(self.proc_root.join("uptime"))
            .ok()
            .and_then(|t| t.split_whitespace().next()?.parse::<f64>().ok())
            .map_or(0, |s| (s * 1000.0) as u64)
    }

    fn rate(&self, root: &ProcStat, cpu_usec: u64, now: Instant) -> u32 {
        let Some(&(start, at, before)) = self.prev.get(&root.pid) else {
            return 0;
        };
        let dt = now.duration_since(at).as_micros() as u64;
        if start != root.start_ticks || dt == 0 {
            return 0;
        }
        u32::try_from(cpu_usec.saturating_sub(before) * 1000 / dt).unwrap_or(u32::MAX)
    }

    fn account(
        &self,
        root: &ProcStat,
        kind: AgentKind,
        members: &[u32],
        stats: &BTreeMap<u32, ProcStat>,
        hz: u64,
    ) -> Agent {
        let pid = root.pid;
        let env_name =
            procs::read_env_var(&self.proc_root, pid, "CREW_AGENT").filter(|n| !n.is_empty());
        let named = env_name.is_some();
        let name = env_name.unwrap_or_else(|| format!("{}-{pid}", kind_label(kind)));
        let cwd = procs::read_cwd(&self.proc_root, pid);
        let top = cwd.as_deref().and_then(paths::repo_root);
        let project = cwd.as_deref().map(paths::project_of).unwrap_or_default();
        let has_tty = root.tty_nr != 0;
        let protected = self
            .protect
            .iter()
            .any(|p| *p == name || *p == pid.to_string());
        let cgroup = procs::read_cgroup_path(&self.proc_root, pid).unwrap_or_default();
        let (totals, source) = self.cgroup_totals(&cgroup, members).map_or_else(
            || (self.proc_totals(members, stats, hz), AccountingSource::Proc),
            |t| (t, AccountingSource::Cgroup),
        );
        Agent {
            name,
            kind,
            pid,
            project,
            toplevel: top
                .or(cwd.clone())
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            cwd: cwd.map(|p| p.display().to_string()).unwrap_or_default(),
            processes: u32::try_from(members.len()).unwrap_or(u32::MAX),
            cpu_usec: totals.cpu_usec,
            cpu_milli: 0,
            mem_bytes: totals.mem_bytes,
            io_read: totals.io_read,
            io_write: totals.io_write,
            cgroup,
            source,
            age_ms: 0,
            named,
            has_tty,
            actionable: named && !has_tty && !protected,
        }
    }

    /// The cgroup's own counters, when it holds nothing but this session (no other process, no child cgroup).
    fn cgroup_totals(&self, cgroup: &str, members: &[u32]) -> Option<Totals> {
        if cgroup.is_empty() {
            return None;
        }
        let dir = self.cgroup_root.join(cgroup.trim_start_matches('/'));
        let pids = procs::cgroup_pids(&dir)?;
        let own: BTreeSet<u32> = members.iter().copied().collect();
        if pids.is_empty()
            || pids.iter().any(|p| !own.contains(p))
            || procs::has_child_cgroups(&dir)
        {
            return None;
        }
        let s = scope::stats_at(&dir)?;
        Some(Totals {
            cpu_usec: s.cpu_usec,
            mem_bytes: s.mem_current,
            io_read: s.io_read,
            io_write: s.io_write,
        })
    }

    fn proc_totals(&self, members: &[u32], stats: &BTreeMap<u32, ProcStat>, hz: u64) -> Totals {
        let page = procs::page_size();
        let mut t = Totals::default();
        for pid in members {
            let Some(s) = stats.get(pid) else { continue };
            t.cpu_usec += s.cpu_ticks * 1_000_000 / hz;
            t.mem_bytes += s.rss_pages * page;
            if let Some((r, w)) = procs::read_io(&self.proc_root, *pid) {
                t.io_read += r;
                t.io_write += w;
            }
        }
        t
    }
}

fn is_nested(s: &ProcStat, stats: &BTreeMap<u32, ProcStat>) -> bool {
    let mut at = s.ppid;
    for _ in 0..MAX_DEPTH {
        let Some(p) = stats.get(&at) else {
            return false;
        };
        if agent_kind(&p.comm).is_some() {
            return true;
        }
        at = p.ppid;
    }
    false
}

fn tree(root: u32, children: &BTreeMap<u32, Vec<u32>>) -> Vec<u32> {
    let mut seen = BTreeSet::from([root]);
    let mut order = vec![root];
    let mut at = 0;
    while at < order.len() {
        for c in children.get(&order[at]).into_iter().flatten() {
            if seen.insert(*c) {
                order.push(*c);
            }
        }
        at += 1;
    }
    order
}

/// Per-project sums; `jobs` maps a project name to (running, queued) usher jobs.
pub fn project_views(agents: &[Agent], jobs: &BTreeMap<String, (u32, u32)>) -> Vec<ProjectView> {
    let mut by: BTreeMap<String, ProjectView> = BTreeMap::new();
    for a in agents {
        let key = if a.toplevel.is_empty() {
            a.project.clone()
        } else {
            a.toplevel.clone()
        };
        let p = by.entry(key).or_insert_with(|| ProjectView {
            name: a.project.clone(),
            toplevel: a.toplevel.clone(),
            ..ProjectView::default()
        });
        p.agents += 1;
        p.processes += a.processes;
        p.cpu_usec += a.cpu_usec;
        p.cpu_milli += a.cpu_milli;
        p.mem_bytes += a.mem_bytes;
        p.io_read_bytes += a.io_read;
        p.io_write_bytes += a.io_write;
    }
    for p in by.values_mut() {
        let (running, queued) = jobs.get(&p.name).copied().unwrap_or_default();
        p.running_jobs = running;
        p.queued_jobs = queued;
    }
    by.into_values().collect()
}

/// What adoption would do this tick; nothing here changes a process.
#[derive(Clone, Debug, PartialEq)]
pub enum Note {
    WouldMove {
        name: String,
        pid: u32,
        cgroup: String,
    },
    WouldFreeze {
        name: String,
        pid: u32,
        psi_cpu: f64,
    },
    WouldThaw {
        name: String,
        pid: u32,
    },
}

impl Note {
    pub fn line(&self) -> String {
        match self {
            Self::WouldMove { name, pid, cgroup } => format!(
                "ADOPT would move agent {name} (pid {pid}) from {cgroup} into crew.slice/agent-{name}.scope; adopt.move is off"
            ),
            Self::WouldFreeze { name, pid, psi_cpu } => format!(
                "ADOPT would freeze agent {name} (pid {pid}): owner present, cpu pressure {psi_cpu:.1}; adopt.freeze is off"
            ),
            Self::WouldThaw { name, pid } => {
                format!("ADOPT would thaw agent {name} (pid {pid}); adopt.freeze is off")
            }
        }
    }
}

#[derive(Default)]
pub struct Planner {
    seen: BTreeSet<u32>,
    frozen: BTreeMap<u32, String>,
}

impl Planner {
    /// One note per decision, only on change; only actionable agents are ever named.
    pub fn plan(
        &mut self,
        cfg: &Adopt,
        agents: &[Agent],
        owner_present: bool,
        psi_cpu: f64,
    ) -> Vec<Note> {
        let live: BTreeSet<u32> = agents.iter().map(|a| a.pid).collect();
        self.seen.retain(|p| live.contains(p));
        self.frozen.retain(|p, _| live.contains(p));
        let mut notes = Vec::new();
        let actionable = || agents.iter().filter(|a| a.actionable);
        for a in actionable() {
            if self.seen.insert(a.pid) {
                notes.push(Note::WouldMove {
                    name: a.name.clone(),
                    pid: a.pid,
                    cgroup: a.cgroup.clone(),
                });
            }
        }
        let calm = !owner_present || psi_cpu < cfg.freeze_psi_cpu / 2.0;
        if calm {
            for (pid, name) in std::mem::take(&mut self.frozen) {
                notes.push(Note::WouldThaw { name, pid });
            }
        } else if owner_present && psi_cpu >= cfg.freeze_psi_cpu {
            let busiest = actionable()
                .filter(|a| !self.frozen.contains_key(&a.pid))
                .max_by_key(|a| a.cpu_milli);
            if let Some(a) = busiest {
                self.frozen.insert(a.pid, a.name.clone());
                notes.push(Note::WouldFreeze {
                    name: a.name.clone(),
                    pid: a.pid,
                    psi_cpu,
                });
            }
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct P<'a> {
        pid: u32,
        comm: &'a str,
        ppid: u32,
        tty: i64,
        ticks: u64,
        cgroup: &'a str,
        env: &'a str,
        cwd: &'a Path,
    }

    struct Fake {
        proc: tempfile::TempDir,
        cg: tempfile::TempDir,
    }

    impl Fake {
        fn new() -> Self {
            let f = Self {
                proc: tempfile::tempdir().unwrap(),
                cg: tempfile::tempdir().unwrap(),
            };
            fs::write(f.proc.path().join("uptime"), "1000.00 5.00\n").unwrap();
            f
        }

        fn proc(&self, p: P<'_>) {
            let (pid, comm, ppid, tty, ticks) = (p.pid, p.comm, p.ppid, p.tty, p.ticks);
            let d = self.proc.path().join(pid.to_string());
            fs::create_dir(&d).unwrap();
            fs::write(
                d.join("stat"),
                format!(
                    "{pid} ({comm}) S {ppid} {pid} {pid} {tty} -1 0 0 0 0 0 {ticks} 0 0 0 20 0 1 0 50000 0 10 0"
                ),
            )
            .unwrap();
            fs::write(d.join("io"), "read_bytes: 1000\nwrite_bytes: 2000\n").unwrap();
            fs::write(d.join("cgroup"), format!("0::{}\n", p.cgroup)).unwrap();
            fs::write(d.join("environ"), p.env.replace(';', "\0")).unwrap();
            std::os::unix::fs::symlink(p.cwd, d.join("cwd")).unwrap();
        }

        fn cgroup(&self, path: &str, pids: &[u32]) {
            let d = self.cg.path().join(path.trim_start_matches('/'));
            fs::create_dir_all(&d).unwrap();
            let procs: String = pids.iter().map(|p| format!("{p}\n")).collect();
            fs::write(d.join("cgroup.procs"), procs).unwrap();
            fs::write(d.join("cpu.stat"), "usage_usec 7777000\n").unwrap();
            fs::write(d.join("memory.current"), "123456\n").unwrap();
            fs::write(d.join("io.stat"), "8:0 rbytes=10 wbytes=20 rios=0 wios=0\n").unwrap();
        }

        fn collector(&self, protect: &[&str]) -> Collector {
            Collector::new(
                self.proc.path(),
                self.cg.path(),
                protect.iter().map(|s| (*s).to_string()).collect(),
            )
        }
    }

    fn repo(dir: &Path, name: &str) -> PathBuf {
        let top = dir.join(name);
        fs::create_dir_all(top.join("src")).unwrap();
        fs::write(top.join(".git"), "gitdir: x").unwrap();
        top.join("src")
    }

    #[test]
    fn sessions_are_outermost_agent_trees_with_project_and_accounting() {
        let f = Fake::new();
        let w = tempfile::tempdir().unwrap();
        let repo_a = repo(w.path(), "myrepo");
        let crew = repo(w.path(), "crew");
        // A session: claude -> sh -> cargo -> nested claude, alone in its cgroup.
        f.proc(P {
            pid: 10,
            comm: ".claude-wrapped",
            ppid: 1,
            tty: 0,
            ticks: 100,
            cgroup: "/agents/a.scope",
            env: "CREW_AGENT=t1;X=1",
            cwd: &repo_a,
        });
        f.proc(P {
            pid: 11,
            comm: "sh",
            ppid: 10,
            tty: 0,
            ticks: 100,
            cgroup: "/agents/a.scope",
            env: "",
            cwd: &repo_a,
        });
        f.proc(P {
            pid: 12,
            comm: "cargo",
            ppid: 11,
            tty: 0,
            ticks: 100,
            cgroup: "/agents/a.scope",
            env: "",
            cwd: &repo_a,
        });
        f.proc(P {
            pid: 13,
            comm: "claude",
            ppid: 12,
            tty: 0,
            ticks: 100,
            cgroup: "/agents/a.scope",
            env: "",
            cwd: &repo_a,
        });
        f.cgroup("/agents/a.scope", &[10, 11, 12, 13]);
        // owner session: codex with a tty sharing its cgroup with vim.
        f.proc(P {
            pid: 30,
            comm: "codex",
            ppid: 1,
            tty: 34816,
            ticks: 400,
            cgroup: "/term.scope",
            env: "",
            cwd: &crew,
        });
        f.proc(P {
            pid: 31,
            comm: "bash",
            ppid: 30,
            tty: 0,
            ticks: 50,
            cgroup: "/term.scope",
            env: "",
            cwd: &crew,
        });
        f.proc(P {
            pid: 40,
            comm: "vim",
            ppid: 1,
            tty: 0,
            ticks: 9,
            cgroup: "/term.scope",
            env: "",
            cwd: &crew,
        });
        f.cgroup("/term.scope", &[30, 31, 40]);
        f.proc(P {
            pid: 50,
            comm: "sleep",
            ppid: 1,
            tty: 0,
            ticks: 1,
            cgroup: "/x",
            env: "",
            cwd: &crew,
        });

        let agents = f.collector(&[]).collect(Instant::now());
        assert_eq!(agents.len(), 2, "{agents:#?}");
        let a = agents.iter().find(|a| a.pid == 10).unwrap();
        assert_eq!(
            (a.name.as_str(), a.kind, a.processes),
            ("t1", AgentKind::Claude, 4)
        );
        assert_eq!(
            (a.project.as_str(), a.named, a.has_tty, a.actionable),
            ("myrepo", true, false, true)
        );
        assert_eq!(a.source, AccountingSource::Cgroup);
        assert_eq!(
            (a.cpu_usec, a.mem_bytes, a.io_read, a.io_write),
            (7_777_000, 123_456, 10, 20)
        );
        assert_eq!(a.age_ms, 1_000_000 - 500_000);
        let c = agents.iter().find(|a| a.pid == 30).unwrap();
        assert_eq!(
            (c.name.as_str(), c.kind, c.processes),
            ("codex-30", AgentKind::Codex, 2)
        );
        assert_eq!(
            (c.project.as_str(), c.named, c.has_tty, c.actionable),
            ("crew", false, true, false)
        );
        assert_eq!(c.source, AccountingSource::Proc);
        assert_eq!(c.cpu_usec, (400 + 50) * 10_000);
        assert_eq!(c.mem_bytes, 2 * 10 * procs::page_size());
        assert_eq!((c.io_read, c.io_write), (2000, 4000));
    }

    #[test]
    fn protected_names_and_pids_are_never_actionable() {
        let f = Fake::new();
        let w = tempfile::tempdir().unwrap();
        let cwd = repo(w.path(), "p");
        f.proc(P {
            pid: 10,
            comm: "claude",
            ppid: 1,
            tty: 0,
            ticks: 1,
            cgroup: "/a",
            env: "CREW_AGENT=lead",
            cwd: &cwd,
        });
        f.proc(P {
            pid: 11,
            comm: "claude",
            ppid: 1,
            tty: 0,
            ticks: 1,
            cgroup: "/b",
            env: "CREW_AGENT=t2",
            cwd: &cwd,
        });
        f.proc(P {
            pid: 12,
            comm: "claude",
            ppid: 1,
            tty: 0,
            ticks: 1,
            cgroup: "/c",
            env: "CREW_AGENT=t3",
            cwd: &cwd,
        });
        let agents = f.collector(&["lead", "12"]).collect(Instant::now());
        let act: Vec<(&str, bool)> = agents
            .iter()
            .map(|a| (a.name.as_str(), a.actionable))
            .collect();
        assert_eq!(act, [("lead", false), ("t2", true), ("t3", false)]);
    }

    #[test]
    fn cpu_rate_comes_from_two_scans() {
        let f = Fake::new();
        let w = tempfile::tempdir().unwrap();
        let cwd = repo(w.path(), "p");
        f.proc(P {
            pid: 10,
            comm: "claude",
            ppid: 1,
            tty: 0,
            ticks: 100,
            cgroup: "/a",
            env: "",
            cwd: &cwd,
        });
        let mut c = f.collector(&[]);
        let t0 = Instant::now();
        assert_eq!(c.collect(t0)[0].cpu_milli, 0);
        let stat = f.proc.path().join("10/stat");
        let text = fs::read_to_string(&stat)
            .unwrap()
            .replace(" 100 0 0 0 20", " 300 0 0 0 20");
        fs::write(&stat, text).unwrap();
        // 200 ticks = 2 cpu-seconds over 4 seconds is half a core.
        let a = &c.collect(t0 + std::time::Duration::from_secs(4))[0];
        assert_eq!(a.cpu_milli, 500);
    }

    #[test]
    fn project_views_sum_agents_and_attach_job_counts() {
        let mk = |name: &str, top: &str, cpu: u64| Agent {
            name: name.into(),
            kind: AgentKind::Claude,
            pid: 1,
            project: "myrepo".into(),
            toplevel: top.into(),
            cwd: top.into(),
            processes: 2,
            cpu_usec: cpu,
            cpu_milli: 500,
            mem_bytes: 10,
            io_read: 1,
            io_write: 2,
            cgroup: String::new(),
            source: AccountingSource::Proc,
            age_ms: 0,
            named: true,
            has_tty: false,
            actionable: true,
        };
        let agents = [
            mk("a", "/work/myrepo", 100),
            mk("b", "/work/myrepo", 50),
            mk("c", "/w/myrepo", 1),
        ];
        let jobs = BTreeMap::from([("myrepo".to_string(), (2, 1))]);
        let p = project_views(&agents, &jobs);
        assert_eq!(p.len(), 2);
        let main = p.iter().find(|p| p.toplevel == "/work/myrepo").unwrap();
        assert_eq!(
            (
                main.agents,
                main.processes,
                main.cpu_usec,
                main.cpu_milli,
                main.mem_bytes
            ),
            (2, 4, 150, 1000, 20)
        );
        assert_eq!(
            (
                main.io_read_bytes,
                main.io_write_bytes,
                main.running_jobs,
                main.queued_jobs
            ),
            (2, 4, 2, 1)
        );
    }

    fn actionable(name: &str, pid: u32, cpu_milli: u32) -> Agent {
        Agent {
            name: name.into(),
            kind: AgentKind::Claude,
            pid,
            project: String::new(),
            toplevel: String::new(),
            cwd: String::new(),
            processes: 1,
            cpu_usec: 0,
            cpu_milli,
            mem_bytes: 0,
            io_read: 0,
            io_write: 0,
            cgroup: "/g".into(),
            source: AccountingSource::Proc,
            age_ms: 0,
            named: true,
            has_tty: false,
            actionable: true,
        }
    }

    #[test]
    fn dry_run_names_only_actionable_agents_and_logs_changes_once() {
        let cfg = Adopt::default();
        let mut owner = actionable("owner", 1, 9000);
        owner.actionable = false;
        let agents = [actionable("t2", 2, 300), actionable("t3", 3, 800), owner];
        let mut p = Planner::default();
        let first = p.plan(&cfg, &agents, true, 10.0);
        assert_eq!(first.len(), 2);
        assert!(
            first
                .iter()
                .all(|n| matches!(n, Note::WouldMove { name, .. } if name != "owner"))
        );
        assert!(p.plan(&cfg, &agents, true, 10.0).is_empty());
        let hot = p.plan(&cfg, &agents, true, 60.0);
        assert_eq!(
            hot,
            [Note::WouldFreeze {
                name: "t3".into(),
                pid: 3,
                psi_cpu: 60.0
            }]
        );
        let hotter = p.plan(&cfg, &agents, true, 60.0);
        assert_eq!(
            hotter,
            [Note::WouldFreeze {
                name: "t2".into(),
                pid: 2,
                psi_cpu: 60.0
            }]
        );
        assert!(p.plan(&cfg, &agents, true, 60.0).is_empty());
        let away = p.plan(&cfg, &agents, false, 60.0);
        assert_eq!(away.len(), 2);
        assert!(away.iter().all(|n| matches!(n, Note::WouldThaw { .. })));
        assert!(p.plan(&cfg, &agents, false, 0.0).is_empty());
    }

    #[test]
    fn notes_say_that_nothing_was_done() {
        let n = Note::WouldFreeze {
            name: "t2".into(),
            pid: 2,
            psi_cpu: 55.0,
        };
        assert!(
            n.line().starts_with("ADOPT would freeze agent t2")
                && n.line().contains("adopt.freeze is off")
        );
    }
}
