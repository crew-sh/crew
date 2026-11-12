//! Why a crate missed the cache: the typed reading of `mbx explain --last`, which says it in prose.
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum Reason {
    /// The key did not change: the stored result was evicted, unavailable, or never stored.
    KeyUnchanged,
    /// Nothing in this crate changed; the named dependencies' artifacts did.
    DependencyChanged(Vec<String>),
    /// Key material that differs from the last recording: arguments, environment values, the compiler.
    KeyDetails(Vec<String>),
    /// Input files that differ from the last recording.
    Inputs(Vec<String>),
    /// mbx has no earlier recording of this crate to compare with.
    NoEarlierRecording,
    /// This compilation was recorded without key details.
    NoDetails,
    /// The key changed but the recorded details do not say how.
    Unexplained,
    /// An embedded C object records absolute paths.
    PathSpecificCObject,
    /// A suggestion from mbx, or a line this reader does not know.
    Note(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Miss {
    pub krate: String,
    pub reasons: Vec<Reason>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Explained {
    pub workspace: String,
    pub command: String,
    pub results: String,
    pub missed: Vec<Miss>,
}

/// A crate line is one word at column 0; mbx's other section lines contain spaces.
fn is_crate_line(l: &str) -> bool {
    !l.is_empty() && !l.starts_with(' ') && !l.contains(char::is_whitespace)
}

fn names(list: &str) -> Vec<String> {
    list.replace(" and ", ", ")
        .split(", ")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn dependency_names(line: &str) -> Option<Vec<String>> {
    let rest = line.split_once("it missed because the artifact")?.1;
    let rest = rest
        .strip_prefix('s')
        .unwrap_or(rest)
        .strip_prefix(" of ")?;
    let list = rest.split_once(" differ").map(|(l, _)| l)?;
    Some(names(list))
}

fn reason_of(line: &str) -> Option<Reason> {
    let t = line.trim();
    Some(if t.starts_with("the action key did not change") {
        Reason::KeyUnchanged
    } else if let Some(deps) = dependency_names(t) {
        Reason::DependencyChanged(deps)
    } else if t.starts_with("no earlier build recorded key details") {
        Reason::NoEarlierRecording
    } else if t.starts_with("key details were not recorded") {
        Reason::NoDetails
    } else if t.starts_with("the action key changed, but the recorded details") {
        Reason::Unexplained
    } else if t.starts_with("this C object embeds absolute paths") {
        Reason::PathSpecificCObject
    } else {
        return None;
    })
}

/// Reads the report; text it does not recognise is kept as notes, never dropped.
pub fn parse(text: &str) -> Explained {
    let mut out = Explained::default();
    let mut in_missed = false;
    // Which list the indented `- item` lines belong to.
    let mut list: Option<fn(Vec<String>) -> Reason> = None;
    for line in text.lines() {
        if !in_missed {
            if let Some(v) = line.trim().strip_prefix("workspace: ") {
                out.workspace = v.to_string();
            } else if let Some(v) = line.trim().strip_prefix("command: ") {
                out.command = v.to_string();
            } else if let Some(v) = line.trim().strip_prefix("results: ") {
                out.results = v.to_string();
            } else if line == "missed crates" {
                in_missed = true;
            }
            continue;
        }
        if is_crate_line(line) {
            out.missed.push(Miss {
                krate: line.to_string(),
                reasons: Vec::new(),
            });
            list = None;
            continue;
        }
        let Some(miss) = out.missed.last_mut() else {
            continue;
        };
        if !line.starts_with(' ') {
            // A new section (bypass summary, cacheability warning) ends the crate list.
            if !line.is_empty() {
                in_missed = false;
            }
            continue;
        }
        let t = line.trim();
        if t.starts_with("key details changed") {
            list = Some(Reason::KeyDetails);
            miss.reasons.push(Reason::KeyDetails(Vec::new()));
        } else if t.starts_with("inputs changed") {
            list = Some(Reason::Inputs);
            miss.reasons.push(Reason::Inputs(Vec::new()));
        } else if let Some(item) = t.strip_prefix("- ") {
            match (miss.reasons.last_mut(), list) {
                (Some(Reason::KeyDetails(v) | Reason::Inputs(v)), Some(_)) => {
                    v.push(item.to_string())
                }
                _ => miss.reasons.push(Reason::Note(t.to_string())),
            }
        } else {
            list = None;
            miss.reasons
                .push(reason_of(t).unwrap_or_else(|| Reason::Note(t.to_string())));
        }
    }
    out
}

fn same_crate(a: &str, b: &str) -> bool {
    a.replace('-', "_") == b.replace('-', "_")
}

pub fn find<'a>(e: &'a Explained, krate: &str) -> Option<&'a Miss> {
    e.missed.iter().find(|m| same_crate(&m.krate, krate))
}

fn describe(r: &Reason) -> Vec<String> {
    match r {
        Reason::KeyUnchanged => vec![
            "the key is the same as in the last recording; the stored result was evicted, unavailable or never stored".into(),
        ],
        Reason::DependencyChanged(d) => vec![format!(
            "nothing in this crate changed; the artifact of {} did, so look at that first",
            d.join(", ")
        )],
        Reason::KeyDetails(v) => std::iter::once("key details changed since the last recording:".to_string())
            .chain(v.iter().map(|i| format!("  - {i}")))
            .collect(),
        Reason::Inputs(v) => std::iter::once("input files changed since the last recording:".to_string())
            .chain(v.iter().map(|i| format!("  - {i}")))
            .collect(),
        Reason::NoEarlierRecording => vec!["no earlier recording of this crate to compare with (a cold cache, or its history expired)".into()],
        Reason::NoDetails => vec!["mbx recorded no key details for this compilation".into()],
        Reason::Unexplained => vec!["the key changed but mbx's record does not say which part".into()],
        Reason::PathSpecificCObject => vec!["a C object embeds absolute paths, so only the same paths can reuse it".into()],
        Reason::Note(n) => vec![n.clone()],
    }
}

pub fn render(e: &Explained, krate: &str) -> String {
    let mut out = format!(
        "workspace {}\nlast build `{}`: {}\n",
        e.workspace, e.command, e.results
    );
    match find(e, krate) {
        Some(m) => {
            out.push_str(&format!("\n{} missed the cache:\n", m.krate));
            for r in &m.reasons {
                for l in describe(r) {
                    out.push_str(&format!("  {l}\n"));
                }
            }
        }
        None if e.missed.is_empty() => out.push_str("\nthe last build recorded no cache misses\n"),
        None => {
            let all: Vec<_> = e.missed.iter().map(|m| m.krate.as_str()).collect();
            out.push_str(&format!(
                "\n{krate} did not miss in the last build. Missed: {}\n",
                all.join(", ")
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests;
