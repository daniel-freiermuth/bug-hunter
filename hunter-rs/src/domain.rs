//! Strongly-typed domain enums — the DB stores these as TEXT but Rust
//! code uses typed variants. sqlx maps them transparently via Type
//! derive; serde serializes as the lowercase string for JSON API parity.
//!
//! Adding a variant here is a compile error at every match site — the
//! exhaustive-match guarantee that string comparisons can never provide.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Finding status
// ---------------------------------------------------------------------------

/// Finding lifecycle status. The DB column is TEXT; sqlx maps via
/// `rename_all` = "`snake_case`".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum FindingStatus {
    New,
    Rechecking,
    Queued,
    Fixing,
    PrOpen,
    Merged,
    Rejected,
    Wontfix,
    Note,
}

impl FindingStatus {
    pub const ALL: [Self; 9] = [
        Self::New,
        Self::Rechecking,
        Self::Queued,
        Self::Fixing,
        Self::PrOpen,
        Self::Merged,
        Self::Rejected,
        Self::Wontfix,
        Self::Note,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Rechecking => "rechecking",
            Self::Queued => "queued",
            Self::Fixing => "fixing",
            Self::PrOpen => "pr_open",
            Self::Merged => "merged",
            Self::Rejected => "rejected",
            Self::Wontfix => "wontfix",
            Self::Note => "note",
        }
    }

    /// Statuses the verdict endpoint accepts.
    pub fn is_verdict(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Rejected | Self::Wontfix | Self::Note | Self::Merged
        )
    }

    /// Statuses that require a reason when set via verdict.
    pub fn reason_required(self) -> bool {
        matches!(self, Self::Rejected | Self::Wontfix)
    }

    /// Suppressed statuses (feed the suppression corpus).
    pub fn is_suppressed(self) -> bool {
        matches!(self, Self::Rejected | Self::Wontfix)
    }

    /// Active (non-terminal) statuses for novelty comparison.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::New | Self::Rechecking | Self::Queued | Self::Fixing | Self::PrOpen
        )
    }
}

impl std::fmt::Display for FindingStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Job state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed,
    Killed,
    Denied,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Killed => "killed",
            Self::Denied => "denied",
        }
    }
}

impl std::fmt::Display for JobState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Severity (replaces the existing types::Severity with sqlx + serde)
// ---------------------------------------------------------------------------

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, sqlx::Type,
)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum Severity {
    Low,
    Medium,
    High,
}

impl Severity {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }

    pub fn rank(self) -> i64 {
        match self {
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Finding type (bug, dep_update, test_gap, …)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum FindingType {
    Bug,
    DepUpdate,
    TestGap,
    Refactor,
    Modernization,
    Standards,
}

impl FindingType {
    pub const ALL: [Self; 6] = [
        Self::Bug,
        Self::DepUpdate,
        Self::TestGap,
        Self::Refactor,
        Self::Modernization,
        Self::Standards,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bug => "bug",
            Self::DepUpdate => "dep_update",
            Self::TestGap => "test_gap",
            Self::Refactor => "refactor",
            Self::Modernization => "modernization",
            Self::Standards => "standards",
        }
    }
}

impl std::fmt::Display for FindingType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for FindingType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "bug" => Ok(Self::Bug),
            "dep_update" => Ok(Self::DepUpdate),
            "test_gap" => Ok(Self::TestGap),
            "refactor" => Ok(Self::Refactor),
            "modernization" => Ok(Self::Modernization),
            "standards" => Ok(Self::Standards),
            _ => Err(format!("unknown finding type: {s:?}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Repo job kinds (hunt + analysis scans) — Rust-only type narrowing;
// DB/JSON traffic keeps using the flat `JobKind` enum below.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepoJobKind {
    Hunt,
    TestGap,
    DepUpdate,
    Refactor,
    Modernization,
    Standards,
}

impl RepoJobKind {
    pub const ALL: [Self; 6] = [
        Self::Hunt,
        Self::TestGap,
        Self::DepUpdate,
        Self::Refactor,
        Self::Modernization,
        Self::Standards,
    ];

    /// Everything except Hunt — the rotation-analysis types that get
    /// starvation-prevention timestamp bumps on failure.
    pub fn is_analysis(self) -> bool {
        !matches!(self, Self::Hunt)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hunt => "hunt",
            Self::TestGap => "test_gap",
            Self::DepUpdate => "dep_update",
            Self::Refactor => "refactor",
            Self::Modernization => "modernization",
            Self::Standards => "standards",
        }
    }
}
impl std::fmt::Display for RepoJobKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RepoJobKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "hunt" => Ok(Self::Hunt),
            "test_gap" => Ok(Self::TestGap),
            "dep_update" => Ok(Self::DepUpdate),
            "refactor" => Ok(Self::Refactor),
            "modernization" => Ok(Self::Modernization),
            "standards" => Ok(Self::Standards),
            _ => Err(format!("unknown repo job kind: {s:?}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Finding job kinds (fix + PR lifecycle).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FindingJobKind {
    Fix,
    Engage,
    Harvest,
    Recheck,
}

impl FindingJobKind {
    pub const ALL: [Self; 4] = [Self::Fix, Self::Engage, Self::Harvest, Self::Recheck];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fix => "fix",
            Self::Engage => "engage",
            Self::Harvest => "harvest",
            Self::Recheck => "recheck",
        }
    }
}

impl std::fmt::Display for FindingJobKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for FindingJobKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "fix" => Ok(Self::Fix),
            "engage" => Ok(Self::Engage),
            "harvest" => Ok(Self::Harvest),
            "recheck" => Ok(Self::Recheck),
            _ => Err(format!("unknown finding job kind: {s:?}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Job kind — composed of the two sub-enums. Serializes to flat strings
// ("hunt", "fix", etc.) for DB/JSON compatibility. All parsing and
// serialization delegates to the sub-enums — adding a variant there
// automatically extends JobKind with zero changes here.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobKind {
    Repo(RepoJobKind),
    Finding(FindingJobKind),
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Repo(r) => r.as_str(),
            Self::Finding(f) => f.as_str(),
        }
    }

    pub fn is_repo(self) -> bool {
        matches!(self, Self::Repo(_))
    }

    pub fn is_finding(self) -> bool {
        matches!(self, Self::Finding(_))
    }

    pub fn as_repo(self) -> Option<RepoJobKind> {
        match self {
            Self::Repo(r) => Some(r),
            Self::Finding(_) => None,
        }
    }

    pub fn as_finding(self) -> Option<FindingJobKind> {
        match self {
            Self::Finding(f) => Some(f),
            Self::Repo(_) => None,
        }
    }
}

impl From<RepoJobKind> for JobKind {
    fn from(k: RepoJobKind) -> Self {
        Self::Repo(k)
    }
}

impl From<FindingJobKind> for JobKind {
    fn from(k: FindingJobKind) -> Self {
        Self::Finding(k)
    }
}

impl std::fmt::Display for JobKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for JobKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<RepoJobKind>()
            .map(Self::Repo)
            .or_else(|_| s.parse::<FindingJobKind>().map(Self::Finding))
            .map_err(|_| format!("unknown job kind: {s:?}"))
    }
}

impl Serialize for JobKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for JobKind {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl sqlx::Type<sqlx::Sqlite> for JobKind {
    fn type_info() -> sqlx::sqlite::SqliteTypeInfo {
        <str as sqlx::Type<sqlx::Sqlite>>::type_info()
    }

    fn compatible(ty: &sqlx::sqlite::SqliteTypeInfo) -> bool {
        <str as sqlx::Type<sqlx::Sqlite>>::compatible(ty)
    }
}

impl sqlx::Encode<'_, sqlx::Sqlite> for JobKind {
    fn encode_by_ref(
        &self,
        buf: &mut <sqlx::Sqlite as sqlx::Database>::ArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        self.as_str().encode_by_ref(buf)
    }
}

impl<'r> sqlx::Decode<'r, sqlx::Sqlite> for JobKind {
    fn decode(
        value: <sqlx::Sqlite as sqlx::Database>::ValueRef<'r>,
    ) -> Result<Self, sqlx::error::BoxDynError> {
        let s = <&str as sqlx::Decode<'r, sqlx::Sqlite>>::decode(value)?;
        s.parse()
            .map_err(|e: String| sqlx::error::BoxDynError::from(e))
    }
}

// ---------------------------------------------------------------------------
// Bug class (kebab-case: boundary, error-path, race, …)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
pub enum BugClass {
    #[serde(rename = "boundary")]
    #[sqlx(rename = "boundary")]
    Boundary,
    #[serde(rename = "error-path")]
    #[sqlx(rename = "error-path")]
    ErrorPath,
    #[serde(rename = "race")]
    #[sqlx(rename = "race")]
    Race,
    #[serde(rename = "contract-drift")]
    #[sqlx(rename = "contract-drift")]
    ContractDrift,
    #[serde(rename = "leak")]
    #[sqlx(rename = "leak")]
    Leak,
    #[serde(rename = "logic")]
    #[sqlx(rename = "logic")]
    Logic,
}

impl BugClass {
    pub const ALL: [Self; 6] = [
        Self::Boundary,
        Self::ErrorPath,
        Self::Race,
        Self::ContractDrift,
        Self::Leak,
        Self::Logic,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Boundary => "boundary",
            Self::ErrorPath => "error-path",
            Self::Race => "race",
            Self::ContractDrift => "contract-drift",
            Self::Leak => "leak",
            Self::Logic => "logic",
        }
    }
}

impl std::fmt::Display for BugClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for BugClass {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "boundary" => Ok(Self::Boundary),
            "error-path" => Ok(Self::ErrorPath),
            "race" => Ok(Self::Race),
            "contract-drift" => Ok(Self::ContractDrift),
            "leak" => Ok(Self::Leak),
            "logic" => Ok(Self::Logic),
            _ => Err(format!("unknown bug class: {s:?}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Forge name (github, gitlab)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(rename_all = "lowercase")]
pub enum ForgeName {
    Github,
    Gitlab,
}

impl ForgeName {
    pub const ALL: [Self; 2] = [Self::Github, Self::Gitlab];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Gitlab => "gitlab",
        }
    }
}

impl std::fmt::Display for ForgeName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ForgeName {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "github" => Ok(Self::Github),
            "gitlab" => Ok(Self::Gitlab),
            _ => Err(format!("unknown forge name: {s:?}")),
        }
    }
}
