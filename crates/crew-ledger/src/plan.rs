use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Error;

pub const RESERVED_SCOPE_ID: &str = "scope";

/// Step ids become branch names, directory names and git arguments: letters, digits, `.`, `_` and `-`,
/// not starting with `.` or `-`.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.starts_with(['.', '-'])
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !id.contains("..")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Asked,
    Extra,
    CoreDesign,
}

impl Kind {
    pub const ALL: [Self; 3] = [Self::Asked, Self::Extra, Self::CoreDesign];

    pub fn name(self) -> &'static str {
        match self {
            Self::Asked => "asked",
            Self::Extra => "extra",
            Self::CoreDesign => "core-design",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Review {
    OwnerBefore,
    OwnerAfter,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Model {
    Sonnet,
    Opus,
    Haiku,
    Fable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunKind {
    Build,
    Test,
    Harness,
    Game,
}

impl RunKind {
    pub const ALL: [Self; 4] = [Self::Build, Self::Test, Self::Harness, Self::Game];

    pub fn name(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Test => "test",
            Self::Harness => "harness",
            Self::Game => "game",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == s)
    }

    /// The usher job kinds a step budget covers; bench, lsp and other are not budgeted.
    pub fn from_job_kind(k: crew_proto::JobKind) -> Option<Self> {
        use crew_proto::JobKind;
        match k {
            JobKind::Build => Some(Self::Build),
            JobKind::Test => Some(Self::Test),
            JobKind::Harness => Some(Self::Harness),
            JobKind::Game => Some(Self::Game),
            _ => None,
        }
    }
}

/// An absent limit is unlimited.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub builds: Option<u32>,
    pub test_minutes: Option<u32>,
    pub harnesses: Option<u32>,
    pub games: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub id: String,
    pub title: String,
    pub kind: Kind,
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default = "default_model")]
    pub model: Model,
    #[serde(default)]
    pub owns: Vec<String>,
    #[serde(default)]
    pub verify: Vec<String>,
    #[serde(default)]
    pub budget: Budget,
    pub review: Option<Review>,
    pub issue: Option<u64>,
}

fn default_model() -> Model {
    Model::Sonnet
}

impl Step {
    /// The stated review, else owner-before for core-design and none otherwise.
    pub fn review(&self) -> Review {
        self.review.unwrap_or(match self.kind {
            Kind::CoreDesign => Review::OwnerBefore,
            _ => Review::None,
        })
    }

    /// Extras and owner-before steps cannot start without an approval.
    pub fn needs_approval(&self) -> bool {
        self.kind == Kind::Extra || self.review() == Review::OwnerBefore
    }

    /// A hash of everything the owner looks at when approving: a change to any of it voids the approval.
    pub fn approval_hash(&self) -> String {
        let join = |v: &[String]| v.join("\u{1e}");
        let fields = [
            self.id.clone(),
            self.title.clone(),
            self.kind.name().to_string(),
            match self.review() {
                Review::OwnerBefore => "owner-before",
                Review::OwnerAfter => "owner-after",
                Review::None => "none",
            }
            .to_string(),
            join(&self.deps),
            match self.model {
                Model::Sonnet => "sonnet",
                Model::Opus => "opus",
                Model::Haiku => "haiku",
                Model::Fable => "fable",
            }
            .to_string(),
            join(&self.owns),
            join(&self.verify),
            [
                self.budget.builds,
                self.budget.test_minutes,
                self.budget.harnesses,
                self.budget.games,
            ]
            .map(|n| n.map_or_else(|| "-".to_string(), |n| n.to_string()))
            .join(","),
            self.issue.map_or_else(String::new, |n| n.to_string()),
        ];
        // FNV-1a over the fields; stable across builds and good enough to notice an edit.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in fields.join("\u{1f}").bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        format!("{hash:016x}")
    }

    /// Whether a scope approval covers this step.
    pub fn covered_by_scope(&self) -> bool {
        self.kind == Kind::Extra && self.review() != Review::OwnerBefore
    }

    /// The limit in the units `Use` counts (test: milliseconds).
    pub fn limit(&self, kind: RunKind) -> Option<u64> {
        match kind {
            RunKind::Build => self.budget.builds.map(u64::from),
            RunKind::Test => self.budget.test_minutes.map(|m| u64::from(m) * 60_000),
            RunKind::Harness => self.budget.harnesses.map(u64::from),
            RunKind::Game => self.budget.games.map(u64::from),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub request: String,
    pub steps: Vec<Step>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanFile {
    #[serde(default)]
    step: Vec<Step>,
}

impl Plan {
    pub fn step(&self, id: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.id == id)
    }

    pub fn parse(request: &str, plan_toml: &str) -> Result<Self, String> {
        let file: PlanFile = toml::from_str(plan_toml).map_err(|e| e.to_string())?;
        Ok(Self {
            request: request.trim().to_string(),
            steps: file.step,
        })
    }
}

#[derive(Deserialize)]
struct ProjectFile {
    plan: PlanSection,
}

#[derive(Deserialize)]
struct PlanSection {
    #[serde(default = "default_plan_file")]
    file: String,
    #[serde(default = "default_ledger_file")]
    ledger: String,
    #[serde(default)]
    request: String,
}

fn default_plan_file() -> String {
    "plan.toml".into()
}

fn default_ledger_file() -> String {
    ".crew/ledger.pb".into()
}

/// Reads `[plan]` of `<root>/crew.toml` (other tables belong to other tools) and the plan file it names.
pub fn load(root: &Path) -> Result<(Plan, PathBuf), Error> {
    let cfg_path = root.join("crew.toml");
    let text = fs::read_to_string(&cfg_path).map_err(|e| Error::io(&cfg_path, e))?;
    let cfg: ProjectFile = toml::from_str(&text).map_err(|e| Error::toml(&cfg_path, e))?;
    let plan_path = root.join(&cfg.plan.file);
    let plan_text = fs::read_to_string(&plan_path).map_err(|e| Error::io(&plan_path, e))?;
    let plan = Plan::parse(&cfg.plan.request, &plan_text).map_err(|m| Error::Toml {
        path: plan_path,
        message: m,
    })?;
    Ok((plan, root.join(&cfg.plan.ledger)))
}
