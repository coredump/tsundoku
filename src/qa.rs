//! Post-translation quality checks.
//!
//! Issues are recorded per chunk in the chapter record and drive targeted
//! retries and `--recheck`.

use serde::{Deserialize, Serialize};

/// Kind of problem found in a translated chunk.
///
/// Serialized in `snake_case` in chapter records (`"name_near_miss"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum IssueKind {
    /// The model refused or returned nothing; the output is a failure marker.
    Failed,
    /// The reply stopped at the token limit (`finish_reason == "length"`).
    Truncated,
    /// Chinese or Japanese characters remain in the output.
    ForeignScript,
    /// A glossary term occurs in the source but its English rendering is absent.
    GlossaryMissing,
    /// A known name in the source is absent from the output.
    NameMissing,
    /// The output contains a near-miss spelling of a name in the source.
    NameNearMiss,
    /// The output's non-empty line count is far from the source's.
    ParagraphMismatch,
    /// The output's length is far from the expected ratio to the source.
    LengthRatio,
    /// The output repeats a phrase or line abnormally often.
    Repetition,
    /// An honorific suffix is attached to something that is not a known name.
    HonorificOnNoun,
}

/// One problem found in a translated chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    /// What kind of problem this is.
    pub kind: IssueKind,
    /// Human-readable specifics, e.g. `Tatyuya ≈ Tatsuya`.
    pub detail: String,
}
