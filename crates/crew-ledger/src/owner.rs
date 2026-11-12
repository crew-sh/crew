//! Who may approve: the owner at a terminal, or a scripted owner holding the token. Never an agent.
use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crew_proto::ledger::ApproverContext;

pub const TOKEN_ENV: &str = "CREW_OWNER_TOKEN";

/// What the CLI saw of the process that asks to approve.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Caller {
    pub stdin_tty: bool,
    pub stdout_tty: bool,
    /// CREW_AGENT, when set and not empty.
    pub agent: Option<String>,
    /// The caller runs inside a crew job scope, or CREW_JOB names one.
    pub in_crew_job: bool,
    pub user: String,
    pub pid: u32,
    pub tty: String,
    /// `--owner-confirmed` was given.
    pub owner_confirmed_flag: bool,
    /// The environment's CREW_OWNER_TOKEN matches the owner's token file.
    pub token_matches: bool,
}

/// How an approval was allowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// The owner is at a terminal and must type the target back.
    Terminal,
    /// A script holding the owner's token.
    Token,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Denied {
    #[error("approvals come from the owner: {0}")]
    NotOwner(String),
    #[error(
        "approvals come from the owner: --owner-confirmed needs {TOKEN_ENV} to match the token file ({0})"
    )]
    BadToken(String),
}

/// The channel an approval may use, or why it may not be given from here.
pub fn channel(c: &Caller, token_file: &str) -> Result<Channel, Denied> {
    if c.owner_confirmed_flag {
        return if c.token_matches {
            Ok(Channel::Token)
        } else {
            Err(Denied::BadToken(token_file.to_string()))
        };
    }
    if let Some(agent) = &c.agent {
        return Err(Denied::NotOwner(format!(
            "CREW_AGENT={agent} is set, so this is an agent's session"
        )));
    }
    if c.in_crew_job {
        return Err(Denied::NotOwner("this runs inside a crew job".into()));
    }
    if !c.stdin_tty || !c.stdout_tty {
        return Err(Denied::NotOwner(
            "standard input and output must be a terminal (scripted owners: --owner-confirmed)"
                .into(),
        ));
    }
    Ok(Channel::Terminal)
}

/// The context to record for an approval allowed on `channel`; `typed` is whether the target was typed back.
pub fn context(c: &Caller, channel: Channel, typed: bool) -> ApproverContext {
    ApproverContext {
        interactive: c.stdin_tty && c.stdout_tty,
        typed_confirmation: typed,
        via_owner_token: channel == Channel::Token,
        agent: c.agent.clone().unwrap_or_default(),
        in_crew_job: c.in_crew_job,
        user: c.user.clone(),
        pid: c.pid,
        tty: c.tty.clone(),
    }
}

/// Whether a recorded approval counts: from the owner's terminal with a typed confirmation, or with the owner's token.
pub fn counts(ctx: &ApproverContext) -> bool {
    ctx.via_owner_token
        || (ctx.interactive && ctx.typed_confirmation && ctx.agent.is_empty() && !ctx.in_crew_job)
}

pub fn token_file(config_dir: &Path) -> PathBuf {
    config_dir.join("owner-token")
}

/// The token in `path`, if the file is the owner's alone (mode 0600 or tighter) and not empty.
pub fn read_token(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    let mine = unsafe { libc::getuid() };
    if meta.uid() != mine || meta.permissions().mode() & 0o077 != 0 {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    let token = text.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Shows what is being approved and reads one line back; the owner must type `expect` exactly.
pub fn confirm(prompt: &str, expect: &str, input: &mut impl BufRead, out: &mut impl Write) -> bool {
    let _ = write!(out, "{prompt}\nType `{expect}` to approve: ");
    let _ = out.flush();
    let mut line = String::new();
    input.read_line(&mut line).is_ok() && line.trim() == expect
}

/// The caller as the process sees itself.
pub fn caller_here(owner_confirmed_flag: bool, config_dir: &Path) -> Caller {
    let agent = std::env::var("CREW_AGENT").ok().filter(|a| !a.is_empty());
    let own_cgroup = crew_sys::procs::read_cgroup_path(Path::new("/proc"), std::process::id());
    let in_scope = own_cgroup.is_some_and(|p| {
        p.split('/')
            .any(|part| part.starts_with("crew-") && part.ends_with(".scope"))
    });
    let token_matches = owner_confirmed_flag
        && match (
            std::env::var(TOKEN_ENV),
            read_token(&token_file(config_dir)),
        ) {
            (Ok(given), Some(expected)) => !given.is_empty() && given == expected,
            _ => false,
        };
    Caller {
        stdin_tty: std::io::stdin().is_terminal(),
        stdout_tty: std::io::stdout().is_terminal(),
        agent,
        in_crew_job: in_scope || std::env::var_os("CREW_JOB").is_some(),
        user: std::env::var("USER").unwrap_or_default(),
        pid: std::process::id(),
        tty: fs::read_link("/proc/self/fd/0")
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        owner_confirmed_flag,
        token_matches,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal() -> Caller {
        Caller {
            stdin_tty: true,
            stdout_tty: true,
            user: "owner".into(),
            ..Caller::default()
        }
    }

    #[test]
    fn an_owner_at_a_terminal_may_approve() {
        assert_eq!(channel(&terminal(), "f"), Ok(Channel::Terminal));
    }

    #[test]
    fn agents_jobs_and_pipes_may_not() {
        let agent = Caller {
            agent: Some("worker-1".into()),
            ..terminal()
        };
        let job = Caller {
            in_crew_job: true,
            ..terminal()
        };
        let piped_in = Caller {
            stdin_tty: false,
            ..terminal()
        };
        let piped_out = Caller {
            stdout_tty: false,
            ..terminal()
        };
        for c in [agent, job, piped_in, piped_out] {
            assert!(
                matches!(channel(&c, "f"), Err(Denied::NotOwner(_))),
                "{c:?}"
            );
        }
    }

    #[test]
    fn the_token_flag_needs_a_matching_token_and_then_skips_the_terminal() {
        let script = Caller {
            owner_confirmed_flag: true,
            token_matches: true,
            ..Caller::default()
        };
        assert_eq!(channel(&script, "f"), Ok(Channel::Token));
        let wrong = Caller {
            token_matches: false,
            ..script
        };
        assert!(matches!(channel(&wrong, "f"), Err(Denied::BadToken(_))));
    }

    #[test]
    fn only_owner_contexts_count() {
        let typed = context(&terminal(), Channel::Terminal, true);
        assert!(counts(&typed));
        assert!(
            !counts(&context(&terminal(), Channel::Terminal, false)),
            "no typed confirmation"
        );
        let agent = Caller {
            agent: Some("w".into()),
            ..terminal()
        };
        assert!(!counts(&context(&agent, Channel::Terminal, true)));
        assert!(counts(&context(&Caller::default(), Channel::Token, false)));
        assert!(!counts(&ApproverContext::default()));
    }

    #[test]
    fn the_target_must_be_typed_back_exactly() {
        let ask = |reply: &str| confirm("Approve?", "docs", &mut reply.as_bytes(), &mut Vec::new());
        assert!(ask("docs\n"));
        assert!(ask("  docs  \n"));
        assert!(!ask("yes\n"));
        assert!(!ask("docs-2\n"));
        assert!(!ask(""));
    }

    #[test]
    fn a_token_file_others_can_read_is_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let path = token_file(dir.path());
        fs::write(&path, "s3cret\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(read_token(&path), None);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_token(&path).as_deref(), Some("s3cret"));
        fs::write(&path, "\n").unwrap();
        assert_eq!(read_token(&path), None, "an empty token matches nothing");
    }
}
