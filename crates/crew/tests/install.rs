//! `site/install.sh` against a release laid out on disk: the real script, a stand-in download server (file URLs), no network.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BINS: [&str; 5] = [
    "crew",
    "crew-usher",
    "crew-plan",
    "crew-queue",
    "crew-depot",
];

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../site/install.sh")
}

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn arch() -> Option<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Some("x86_64"),
        "aarch64" => Some("aarch64"),
        _ => None,
    }
}

fn scratch() -> tempfile::TempDir {
    let base = std::env::var_os("CREW_TEST_TMP").map_or_else(std::env::temp_dir, PathBuf::from);
    fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("crew-install-")
        .tempdir_in(base)
        .unwrap()
}

/// A release directory holding the tarball for this machine and the sums file; `bins` are what the tarball contains.
fn release(dir: &Path, arch: &str, bins: &[&str]) -> PathBuf {
    let name = format!("crew-{arch}-unknown-linux-musl");
    let tree = dir.join("build").join(&name);
    fs::create_dir_all(&tree).unwrap();
    for bin in bins {
        let p = tree.join(bin);
        fs::write(&p, format!("#!/bin/sh\necho {bin}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(
        tree.join("crew-usher.service"),
        "[Service]\nExecStart=%h/.cargo/bin/crew-usher\nRestart=on-failure\n",
    )
    .unwrap();
    let rel = dir.join("release");
    fs::create_dir_all(&rel).unwrap();
    let tarball = rel.join(format!("{name}.tar.gz"));
    let ok = Command::new("tar")
        .arg("-czf")
        .arg(&tarball)
        .arg("-C")
        .arg(dir.join("build"))
        .arg(&name)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let sums = Command::new("sha256sum")
        .arg(format!("{name}.tar.gz"))
        .current_dir(&rel)
        .output()
        .unwrap();
    fs::write(rel.join("SHA256SUMS"), sums.stdout).unwrap();
    rel
}

struct Home {
    root: PathBuf,
}

impl Home {
    fn install_dir(&self) -> PathBuf {
        self.root.join("bin")
    }

    fn run(&self, release: &Path, extra: &[(&str, &str)]) -> Output {
        let mut c = Command::new("sh");
        c.arg(script())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", &self.root)
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("CREW_INSTALL_DIR", self.install_dir())
            .env("CREW_RELEASE_URL", format!("file://{}", release.display()));
        for (k, v) in extra {
            c.env(k, v);
        }
        c.output().unwrap()
    }
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// The tools the test needs; a missing one skips (a CI runner has them all).
fn ready() -> Option<&'static str> {
    let arch = arch()?;
    if !(have("curl") && have("sha256sum") && have("tar")) {
        eprintln!("skipped: curl, sha256sum and tar are needed");
        return None;
    }
    Some(arch)
}

#[test]
fn the_installer_verifies_the_download_and_installs_every_program() {
    let Some(arch) = ready() else { return };
    let dir = scratch();
    let rel = release(dir.path(), arch, &BINS);
    let home = Home {
        root: dir.path().join("home"),
    };

    let out = home.run(&rel, &[]);
    assert!(out.status.success(), "{}", text(&out));
    for bin in BINS {
        let p = home.install_dir().join(bin);
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o111,
            0o111,
            "{bin} is not executable"
        );
        let ran = Command::new(&p).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&ran.stdout).trim(), bin);
    }
    let unit = fs::read_to_string(home.root.join("data/crew/crew-usher.service")).unwrap();
    assert!(
        unit.contains(&format!(
            "ExecStart={}/crew-usher",
            home.install_dir().display()
        )),
        "{unit}"
    );
    let said = text(&out);
    assert!(
        said.contains("systemctl --user enable --now crew-usher"),
        "{said}"
    );
    assert!(
        said.contains("crew-usher.service ~/.config/systemd/user/"),
        "{said}"
    );
    assert!(
        said.contains("is not on your PATH"),
        "the install dir was not on PATH: {said}"
    );

    // Installing again over running copies works.
    assert!(home.run(&rel, &[]).status.success());
}

#[test]
fn a_download_that_does_not_match_its_sum_installs_nothing() {
    let Some(arch) = ready() else { return };
    let dir = scratch();
    let rel = release(dir.path(), arch, &BINS);
    let tarball = rel.join(format!("crew-{arch}-unknown-linux-musl.tar.gz"));
    let mut bytes = fs::read(&tarball).unwrap();
    bytes.push(0);
    fs::write(&tarball, bytes).unwrap();
    let home = Home {
        root: dir.path().join("home"),
    };

    let out = home.run(&rel, &[]);
    assert!(!out.status.success());
    assert!(text(&out).contains("checksum mismatch"), "{}", text(&out));
    assert!(!home.install_dir().join("crew").exists());
    assert!(!home.root.join("data").exists());
}

#[test]
fn a_release_without_an_entry_for_this_machine_or_a_program_is_refused() {
    let Some(arch) = ready() else { return };
    let dir = scratch();
    let rel = release(dir.path(), arch, &BINS);
    fs::write(rel.join("SHA256SUMS"), "0000  some-other-file.tar.gz\n").unwrap();
    let home = Home {
        root: dir.path().join("home"),
    };
    let out = home.run(&rel, &[]);
    assert!(!out.status.success());
    assert!(text(&out).contains("no entry"), "{}", text(&out));

    let short = scratch();
    let rel = release(short.path(), arch, &BINS[..4]);
    let out = home.run(&rel, &[]);
    assert!(!out.status.success());
    assert!(text(&out).contains("has no crew-depot"), "{}", text(&out));
    assert!(
        !home.install_dir().join("crew").exists(),
        "a release missing a program must not be half installed"
    );
}
