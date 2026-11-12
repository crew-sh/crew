use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use crew_proto::prost::Message;
use crew_proto::{JobKind, JobRequest, SizeEntry, SizeSample, SizesFile};

const KEEP_SAMPLES: usize = 20;
const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Learned {
    pub peak_mem_bytes: u64,
    pub cores_milli: u32,
}

#[derive(Debug, Default)]
pub struct Sizes {
    samples: BTreeMap<String, Vec<SizeSample>>,
}

fn base(arg: &str) -> &str {
    arg.rsplit('/').next().unwrap_or(arg)
}

fn cargo_detail(args: &[String]) -> String {
    let mut sub = "";
    let mut packages = Vec::new();
    let mut profile = "dev".to_string();
    let mut it = args.iter().filter(|a| !a.starts_with('+')).peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-p" | "--package" => packages.extend(it.next().cloned()),
            "--profile" => profile = it.next().cloned().unwrap_or(profile),
            "--release" => profile = "release".into(),
            "--" => break,
            s if s.starts_with("-p") && !s.starts_with("--") && s.len() > 2 => packages.push(s[2..].to_string()),
            s if s.starts_with("--package=") => packages.push(s["--package=".len()..].to_string()),
            s if s.starts_with("--profile=") => profile = s["--profile=".len()..].to_string(),
            s if sub.is_empty() && !s.starts_with('-') => sub = s,
            _ => {}
        }
    }
    packages.sort();
    format!("cargo {sub} -p {} {profile}", packages.join(","))
}

/// Groups runs of the same command: cargo by subcommand, packages and profile, else the first two words.
pub fn key(req: &JobRequest) -> String {
    let kind = JobKind::try_from(req.kind).unwrap_or(JobKind::Other);
    let mut argv = req.argv.iter().map(String::as_str);
    let mut program = argv.next().map_or("", base);
    let mut rest: Vec<String> = argv.map(str::to_string).collect();
    if program == "hz" && rest.first().is_some_and(|a| a == "cargo") {
        program = "cargo";
        rest.remove(0);
    }
    let detail = if program == "cargo" {
        cargo_detail(&rest)
    } else {
        std::iter::once(program).chain(rest.first().map(String::as_str)).collect::<Vec<_>>().join(" ")
    };
    format!("{}|{}|{detail}", req.project, kind.name())
}

fn median<T: Ord + Copy>(mut v: Vec<T>) -> Option<T> {
    v.sort();
    v.get(v.len() / 2).copied()
}

impl Sizes {
    pub fn load(path: &Path) -> Self {
        let Ok(bytes) = fs::read(path) else { return Self::default() };
        let Ok(file) = SizesFile::decode(bytes.as_slice()) else { return Self::default() };
        Self { samples: file.entries.into_iter().map(|e| (e.key, e.samples)).collect() }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let file = SizesFile {
            version: FORMAT_VERSION,
            entries: self.samples.iter().map(|(k, s)| SizeEntry { key: k.clone(), samples: s.clone() }).collect(),
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("pb.tmp");
        fs::write(&tmp, file.encode_to_vec())?;
        fs::rename(tmp, path)
    }

    pub fn record(&mut self, key: &str, peak_mem_bytes: u64, cores_milli: u32) {
        let list = self.samples.entry(key.to_string()).or_default();
        list.push(SizeSample { peak_mem_bytes, cores_milli });
        if list.len() > KEEP_SAMPLES {
            list.remove(0);
        }
    }

    pub fn expected(&self, key: &str) -> Option<Learned> {
        let list = self.samples.get(key).filter(|l| !l.is_empty())?;
        Some(Learned {
            peak_mem_bytes: median(list.iter().map(|s| s.peak_mem_bytes).collect())?,
            cores_milli: median(list.iter().map(|s| s.cores_milli).collect())?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(argv: &[&str]) -> JobRequest {
        JobRequest {
            kind: JobKind::Build as i32,
            project: "horizon".into(),
            argv: argv.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn cargo_keys_ignore_flags_that_do_not_change_size() {
        let a = key(&req(&["cargo", "clippy", "-p", "b", "-p", "a", "--all-targets", "--", "-D", "warnings"]));
        let b = key(&req(&["/home/x/.cargo/bin/cargo", "+nightly", "clippy", "--package=a", "-pb"]));
        assert_eq!(a, "horizon|build|cargo clippy -p a,b dev");
        assert_eq!(a, b);
        assert_eq!(key(&req(&["hz", "cargo", "build", "--release"])), "horizon|build|cargo build -p  release");
        assert_eq!(key(&req(&["cargo", "build", "--profile", "fast"])), "horizon|build|cargo build -p  fast");
    }

    #[test]
    fn other_commands_use_first_two_words() {
        assert_eq!(key(&req(&["/usr/bin/python3", "tools/parity.py", "mob_ai", "sculk"])), "horizon|build|python3 tools/parity.py");
        assert_eq!(key(&req(&["true"])), "horizon|build|true");
    }

    #[test]
    fn expected_is_the_median_and_survives_a_save() {
        let mut s = Sizes::default();
        assert_eq!(s.expected("k"), None);
        for peak in [3, 100, 5, 7, 9] {
            s.record("k", peak, 1000 + peak as u32);
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sizes.pb");
        s.save(&path).unwrap();
        let loaded = Sizes::load(&path);
        assert_eq!(loaded.expected("k"), Some(Learned { peak_mem_bytes: 7, cores_milli: 1007 }));
        assert_eq!(Sizes::load(&dir.path().join("missing")).expected("k"), None);
    }

    #[test]
    fn only_recent_samples_count() {
        let mut s = Sizes::default();
        for _ in 0..KEEP_SAMPLES {
            s.record("k", 1, 1);
        }
        for _ in 0..KEEP_SAMPLES {
            s.record("k", 50, 1);
        }
        assert_eq!(s.expected("k").unwrap().peak_mem_bytes, 50);
    }
}
