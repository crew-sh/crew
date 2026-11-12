use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use crate::state_file::{self, Read};

use crew_proto::prost::Message;
use crew_proto::{JobKind, JobRequest, SizeEntry, SizeSample, SizesFile};

const KEEP_SAMPLES: usize = 20;
const FORMAT_VERSION: u32 = 1;
/// A command nobody ran for this long is forgotten, so the file does not grow with every command line ever used.
const KEEP_KEY_MS: u64 = 28 * 24 * 3600 * 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Learned {
    pub peak_mem_bytes: u64,
    pub cores_milli: u32,
    /// Median run time; `None` until a run with a known duration was recorded.
    pub wall_ms: Option<u64>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Sizes {
    samples: BTreeMap<String, Vec<SizeSample>>,
    last_seen_ms: BTreeMap<String, u64>,
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
            s if s.starts_with("-p") && !s.starts_with("--") && s.len() > 2 => {
                packages.push(s[2..].to_string())
            }
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
    let program = argv.next().map_or("", base);
    let rest: Vec<String> = argv.map(str::to_string).collect();
    let detail = if program == "cargo" {
        cargo_detail(&rest)
    } else {
        std::iter::once(program)
            .chain(rest.first().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    };
    format!("{}|{}|{detail}", req.project, kind.name())
}

fn median<T: Ord + Copy>(mut v: Vec<T>) -> Option<T> {
    v.sort();
    v.get(v.len() / 2).copied()
}

impl Sizes {
    /// Reads the sizes file; a damaged one is set aside and the usher starts without learned sizes.
    pub fn load(path: &Path, now_ms: u64) -> Read<Self> {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Read::Missing,
            Err(e) => {
                return Read::Unusable {
                    why: format!("cannot read it: {e}"),
                    to: None,
                };
            }
        };
        let why = match SizesFile::decode(bytes.as_slice()) {
            Ok(f) if f.version <= FORMAT_VERSION => {
                let mut sizes = Self::default();
                for e in f.entries {
                    // Files from before keys expired get a fresh lease.
                    let seen = if e.last_seen_unix_ms == 0 {
                        now_ms
                    } else {
                        e.last_seen_unix_ms
                    };
                    sizes.last_seen_ms.insert(e.key.clone(), seen);
                    sizes.samples.insert(e.key, e.samples);
                }
                return Read::Loaded(sizes);
            }
            Ok(f) => format!("written by a newer usher (format {})", f.version),
            Err(e) => format!("not a valid sizes file: {e}"),
        };
        Read::Unusable {
            why,
            to: state_file::quarantine(path).ok(),
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let file = SizesFile {
            version: FORMAT_VERSION,
            entries: self
                .samples
                .iter()
                .map(|(k, s)| SizeEntry {
                    key: k.clone(),
                    samples: s.clone(),
                    last_seen_unix_ms: self.last_seen_ms.get(k).copied().unwrap_or(0),
                })
                .collect(),
        };
        state_file::write_atomic(path, &file.encode_to_vec())
    }

    pub fn record(&mut self, key: &str, run: Learned, now_ms: u64) {
        let list = self.samples.entry(key.to_string()).or_default();
        list.push(SizeSample {
            peak_mem_bytes: run.peak_mem_bytes,
            cores_milli: run.cores_milli,
            wall_ms: run.wall_ms.unwrap_or(0),
        });
        if list.len() > KEEP_SAMPLES {
            list.remove(0);
        }
        self.last_seen_ms.insert(key.to_string(), now_ms);
        self.forget_stale(now_ms);
    }

    fn forget_stale(&mut self, now_ms: u64) {
        let stale: Vec<String> = self
            .last_seen_ms
            .iter()
            .filter(|(_, seen)| now_ms.saturating_sub(**seen) >= KEEP_KEY_MS)
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            self.samples.remove(&k);
            self.last_seen_ms.remove(&k);
        }
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn expected(&self, key: &str) -> Option<Learned> {
        let list = self.samples.get(key).filter(|l| !l.is_empty())?;
        Some(Learned {
            peak_mem_bytes: median(list.iter().map(|s| s.peak_mem_bytes).collect())?,
            cores_milli: median(list.iter().map(|s| s.cores_milli).collect())?,
            wall_ms: median(list.iter().map(|s| s.wall_ms).filter(|w| *w > 0).collect()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(peak_mem_bytes: u64, cores_milli: u32) -> Learned {
        Learned {
            peak_mem_bytes,
            cores_milli,
            wall_ms: None,
        }
    }

    fn req(argv: &[&str]) -> JobRequest {
        JobRequest {
            kind: JobKind::Build as i32,
            project: "myrepo".into(),
            argv: argv.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn cargo_keys_ignore_flags_that_do_not_change_size() {
        let a = key(&req(&[
            "cargo",
            "clippy",
            "-p",
            "b",
            "-p",
            "a",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ]));
        let b = key(&req(&[
            "/usr/local/bin/cargo",
            "+nightly",
            "clippy",
            "--package=a",
            "-pb",
        ]));
        assert_eq!(a, "myrepo|build|cargo clippy -p a,b dev");
        assert_eq!(a, b);
        assert_eq!(
            key(&req(&["cargo", "build", "--release"])),
            "myrepo|build|cargo build -p  release"
        );
        assert_eq!(
            key(&req(&["cargo", "build", "--profile", "fast"])),
            "myrepo|build|cargo build -p  fast"
        );
    }

    #[test]
    fn other_commands_use_first_two_words() {
        assert_eq!(
            key(&req(&[
                "/usr/bin/python3",
                "tools/check.py",
                "unit_a",
                "unit_b"
            ])),
            "myrepo|build|python3 tools/check.py"
        );
        assert_eq!(key(&req(&["true"])), "myrepo|build|true");
    }

    #[test]
    fn expected_is_the_median_and_survives_a_save() {
        let mut s = Sizes::default();
        assert_eq!(s.expected("k"), None);
        for peak in [3, 100, 5, 7, 9] {
            // A sample from before durations were kept says nothing about the duration.
            let wall_ms = (peak != 100).then_some(peak * 1000);
            s.record(
                "k",
                Learned {
                    peak_mem_bytes: peak,
                    cores_milli: 1000 + peak as u32,
                    wall_ms,
                },
                1,
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sizes.pb");
        s.save(&path).unwrap();
        let Read::Loaded(loaded) = Sizes::load(&path, 1) else {
            panic!("a saved file loads");
        };
        assert_eq!(
            loaded.expected("k"),
            Some(Learned {
                peak_mem_bytes: 7,
                cores_milli: 1007,
                wall_ms: Some(7000),
            })
        );
        assert_eq!(Sizes::load(&dir.path().join("missing"), 1), Read::Missing);
    }

    #[test]
    fn only_recent_samples_count() {
        let mut s = Sizes::default();
        for _ in 0..KEEP_SAMPLES {
            s.record("k", run(1, 1), 1);
        }
        for _ in 0..KEEP_SAMPLES {
            s.record("k", run(50, 1), 1);
        }
        assert_eq!(s.expected("k").unwrap().peak_mem_bytes, 50);
    }

    #[test]
    fn a_command_not_run_for_weeks_is_forgotten() {
        let mut s = Sizes::default();
        s.record("old", run(5, 5), 1_000);
        s.record("fresh", run(5, 5), KEEP_KEY_MS);
        s.record("fresh", run(6, 6), KEEP_KEY_MS + 1_000);
        assert_eq!(s.expected("old"), None);
        assert!(s.expected("fresh").is_some());
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn a_damaged_sizes_file_is_set_aside_and_a_file_without_dates_gets_a_lease() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sizes.pb");
        fs::write(&path, b"\x0a\xff\xff").unwrap();
        assert!(matches!(
            Sizes::load(&path, 1),
            Read::Unusable { to: Some(_), .. }
        ));
        assert!(!path.exists());
        let undated = SizesFile {
            version: 1,
            entries: vec![SizeEntry {
                key: "k".into(),
                samples: vec![SizeSample {
                    peak_mem_bytes: 9,
                    cores_milli: 1,
                    wall_ms: 0,
                }],
                last_seen_unix_ms: 0,
            }],
        };
        fs::write(&path, undated.encode_to_vec()).unwrap();
        let Read::Loaded(mut s) = Sizes::load(&path, KEEP_KEY_MS * 2) else {
            panic!("an old-format file loads");
        };
        s.record("other", run(1, 1), KEEP_KEY_MS * 2 + 5);
        assert!(
            s.expected("k").is_some(),
            "an undated key is not expired at once"
        );
    }
}
