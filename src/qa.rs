//! Post-translation quality checks.
//!
//! Issues are recorded per chunk in the chapter record and drive targeted
//! retries and `--recheck`.

use crate::name_mapping::{TermEntry, near_miss_names_in};
use crate::output_rules::{OutputRules, honorific_on_non_name};
use crate::utils::count_cjk_chars;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::str::FromStr;

/// Kind of problem found in a translated chunk.
///
/// Serialized in `snake_case` in chapter records (`"name_near_miss"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

impl IssueKind {
    /// The `snake_case` name used in chapter records (`name_near_miss`).
    pub fn as_str(self) -> &'static str {
        match self {
            IssueKind::Failed => "failed",
            IssueKind::Truncated => "truncated",
            IssueKind::ForeignScript => "foreign_script",
            IssueKind::GlossaryMissing => "glossary_missing",
            IssueKind::NameMissing => "name_missing",
            IssueKind::NameNearMiss => "name_near_miss",
            IssueKind::ParagraphMismatch => "paragraph_mismatch",
            IssueKind::LengthRatio => "length_ratio",
            IssueKind::Repetition => "repetition",
            IssueKind::HonorificOnNoun => "honorific_on_noun",
        }
    }
}

impl fmt::Display for IssueKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One problem found in a translated chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    /// What kind of problem this is.
    pub kind: IssueKind,
    /// Human-readable specifics, e.g. `Tatyuya ≈ Tatsuya`.
    pub detail: String,
}

/// Group of issue kinds, as named in `qa.retry_on` and `--recheck`.
///
/// Serialized in lowercase; [`FromStr`] and [`fmt::Display`] use the same names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum IssueClass {
    /// [`IssueKind::Failed`].
    Failed,
    /// [`IssueKind::Truncated`].
    Truncated,
    /// [`IssueKind::ForeignScript`].
    Foreign,
    /// [`IssueKind::GlossaryMissing`].
    Glossary,
    /// [`IssueKind::NameNearMiss`] and [`IssueKind::NameMissing`].
    Names,
    /// [`IssueKind::ParagraphMismatch`].
    Paragraphs,
    /// [`IssueKind::LengthRatio`].
    Length,
    /// [`IssueKind::Repetition`].
    Repetition,
    /// [`IssueKind::HonorificOnNoun`].
    Honorifics,
}

impl IssueClass {
    /// Every class, in documentation order.
    pub const ALL: [IssueClass; 9] = [
        IssueClass::Failed,
        IssueClass::Truncated,
        IssueClass::Foreign,
        IssueClass::Glossary,
        IssueClass::Names,
        IssueClass::Paragraphs,
        IssueClass::Length,
        IssueClass::Repetition,
        IssueClass::Honorifics,
    ];

    /// The class name used in config and on the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            IssueClass::Failed => "failed",
            IssueClass::Truncated => "truncated",
            IssueClass::Foreign => "foreign",
            IssueClass::Glossary => "glossary",
            IssueClass::Names => "names",
            IssueClass::Paragraphs => "paragraphs",
            IssueClass::Length => "length",
            IssueClass::Repetition => "repetition",
            IssueClass::Honorifics => "honorifics",
        }
    }

    /// Whether issues of `kind` belong to this class.
    pub fn matches(self, kind: IssueKind) -> bool {
        match self {
            IssueClass::Failed => kind == IssueKind::Failed,
            IssueClass::Truncated => kind == IssueKind::Truncated,
            IssueClass::Foreign => kind == IssueKind::ForeignScript,
            IssueClass::Glossary => kind == IssueKind::GlossaryMissing,
            IssueClass::Names => matches!(kind, IssueKind::NameNearMiss | IssueKind::NameMissing),
            IssueClass::Paragraphs => kind == IssueKind::ParagraphMismatch,
            IssueClass::Length => kind == IssueKind::LengthRatio,
            IssueClass::Repetition => kind == IssueKind::Repetition,
            IssueClass::Honorifics => kind == IssueKind::HonorificOnNoun,
        }
    }
}

impl fmt::Display for IssueClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for IssueClass {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        IssueClass::ALL
            .into_iter()
            .find(|class| class.as_str() == s)
            .ok_or_else(|| {
                let names: Vec<&str> = IssueClass::ALL
                    .into_iter()
                    .map(IssueClass::as_str)
                    .collect();
                format!(
                    "unknown issue class `{s}` (expected one of: {})",
                    names.join(", ")
                )
            })
    }
}

/// `[qa]` settings: which issues trigger a retry and the ratio thresholds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QaConfig {
    /// QA retries per chunk, separate from `translation.retries`.
    pub max_retries: u32,
    /// Issue classes that trigger a QA retry.
    pub retry_on: Vec<IssueClass>,
    /// Allowed output/source non-empty line ratio `[low, high]`.
    pub paragraph_ratio: [f64; 2],
    /// Allowed output/source character ratio `[low, high]`.
    pub length_ratio: [f64; 2],
}

impl Default for QaConfig {
    fn default() -> Self {
        Self {
            max_retries: 1,
            retry_on: vec![
                IssueClass::Truncated,
                IssueClass::Foreign,
                IssueClass::Glossary,
                IssueClass::Names,
                IssueClass::Repetition,
            ],
            paragraph_ratio: [0.85, 1.15],
            length_ratio: [1.2, 4.0],
        }
    }
}

impl QaConfig {
    /// Check that both ratio ranges are finite with `0 < low < high`.
    pub fn validate(&self) -> Result<(), String> {
        for (key, [low, high]) in [
            ("qa.paragraph_ratio", self.paragraph_ratio),
            ("qa.length_ratio", self.length_ratio),
        ] {
            if !(low.is_finite() && high.is_finite() && low > 0.0 && low < high) {
                return Err(format!(
                    "{key} must be [low, high] with 0 < low < high, got [{low}, {high}]"
                ));
            }
        }
        Ok(())
    }

    /// Whether an issue of `kind` should trigger a QA retry.
    ///
    /// Always false for record-only kinds ([`IssueKind::NameMissing`],
    /// [`IssueKind::HonorificOnNoun`]), whatever `retry_on` says.
    pub fn retries_on(&self, kind: IssueKind) -> bool {
        match kind {
            IssueKind::NameMissing | IssueKind::HonorificOnNoun => false,
            IssueKind::Failed
            | IssueKind::Truncated
            | IssueKind::ForeignScript
            | IssueKind::GlossaryMissing
            | IssueKind::NameNearMiss
            | IssueKind::ParagraphMismatch
            | IssueKind::LengthRatio
            | IssueKind::Repetition => self.retry_on.iter().any(|class| class.matches(kind)),
        }
    }
}

/// What a chunk's output is checked against.
#[derive(Debug, Clone, Copy)]
pub struct ChunkContext<'a> {
    /// The exact text sent to the model (names already substituted).
    pub source: &'a str,
    /// Glossary entries given to the model for this chapter.
    pub glossary: &'a [TermEntry],
    /// Known English name renderings.
    pub known_names: &'a [String],
    /// The novel's output rules, for [`check_honorifics`]; `None` skips that check.
    pub output_rules: Option<&'a OutputRules>,
}

/// Separator between the found word and the expected name in a near-miss detail.
const NEAR_MISS_SEPARATOR: &str = " ≈ ";

/// A word 4-gram occurring more often than this is repetition.
const REPEATED_NGRAM_MAX: usize = 8;

/// This many identical lines in a row is repetition.
const REPEATED_LINE_RUN: usize = 4;

/// Paragraph mismatches smaller than this many lines are ignored.
const PARAGRAPH_MIN_DIFF: usize = 3;

/// Check one translated chunk against its source.
///
/// Emits [`IssueKind::ForeignScript`], [`IssueKind::GlossaryMissing`],
/// [`IssueKind::NameNearMiss`], [`IssueKind::NameMissing`],
/// [`IssueKind::ParagraphMismatch`], [`IssueKind::LengthRatio`] and
/// [`IssueKind::Repetition`], in that order. Never emits `Failed` or
/// `Truncated`: only the translator knows those.
pub fn check_chunk(ctx: &ChunkContext<'_>, output: &str, cfg: &QaConfig) -> Vec<Issue> {
    let mut issues = Vec::new();
    let lower_output = output.to_lowercase();

    let cjk = count_cjk_chars(output);
    if cjk > 0 {
        issues.push(Issue {
            kind: IssueKind::ForeignScript,
            detail: format!("{cjk} Japanese/Chinese characters remain"),
        });
    }

    for term in ctx.glossary {
        if term.original.is_empty()
            || term.english.is_empty()
            || !occurs_outside_longer_terms(ctx.source, term, ctx.glossary)
        {
            continue;
        }
        if !lower_output.contains(&term.english.to_lowercase()) {
            issues.push(Issue {
                kind: IssueKind::GlossaryMissing,
                detail: glossary_detail(term),
            });
        }
    }

    let term_renderings: Vec<&str> = ctx
        .glossary
        .iter()
        .map(|term| term.english.as_str())
        .collect();
    for miss in near_miss_names_in(output, ctx.known_names, &term_renderings) {
        if contains_word(ctx.source, &miss.expected) && !contains_word(ctx.source, &miss.found) {
            issues.push(Issue {
                kind: IssueKind::NameNearMiss,
                detail: format!("{}{NEAR_MISS_SEPARATOR}{}", miss.found, miss.expected),
            });
        }
    }

    let names: BTreeSet<&str> = ctx
        .known_names
        .iter()
        .map(String::as_str)
        .filter(|name| !name.is_empty())
        .collect();
    for name in names {
        if contains_word(ctx.source, name) && !contains_word(&lower_output, &name.to_lowercase()) {
            issues.push(Issue {
                kind: IssueKind::NameMissing,
                detail: name.to_string(),
            });
        }
    }

    let source_lines = non_empty_lines(ctx.source);
    let output_lines = non_empty_lines(output);
    if source_lines > 0 && source_lines.abs_diff(output_lines) >= PARAGRAPH_MIN_DIFF {
        let ratio = output_lines as f64 / source_lines as f64;
        if outside(ratio, cfg.paragraph_ratio) {
            issues.push(Issue {
                kind: IssueKind::ParagraphMismatch,
                detail: format!(
                    "{output_lines} lines for {source_lines} source lines (ratio {ratio:.2})"
                ),
            });
        }
    }

    let source_chars = ctx.source.chars().count();
    if source_chars > 0 {
        let output_chars = output.chars().count();
        let ratio = output_chars as f64 / source_chars as f64;
        if outside(ratio, cfg.length_ratio) {
            issues.push(Issue {
                kind: IssueKind::LengthRatio,
                detail: format!(
                    "{output_chars} chars for {source_chars} source chars (ratio {ratio:.2})"
                ),
            });
        }
    }

    issues.extend(repetition(output));
    issues
}

/// Flag honorifics attached to something other than a known name (`cat-san`).
///
/// One issue per distinct occurrence text, in order of first appearance.
pub fn check_honorifics(output: &str, rules: &OutputRules) -> Vec<Issue> {
    let mut seen = BTreeSet::new();
    honorific_on_non_name(output, rules)
        .into_iter()
        .filter(|found| seen.insert(found.clone()))
        .map(|detail| Issue {
            kind: IssueKind::HonorificOnNoun,
            detail,
        })
        .collect()
}

/// Badness score for picking the best of several attempts; lower is better.
///
/// Any `Failed` outweighs any number of `Truncated`, which outweighs any
/// number of `ForeignScript`, which outweighs the other kinds.
pub fn severity(issues: &[Issue]) -> u32 {
    issues
        .iter()
        .map(|issue| match issue.kind {
            IssueKind::Failed => 1_000_000,
            IssueKind::Truncated => 10_000,
            IssueKind::ForeignScript => 100,
            IssueKind::GlossaryMissing
            | IssueKind::NameMissing
            | IssueKind::NameNearMiss
            | IssueKind::ParagraphMismatch
            | IssueKind::LengthRatio
            | IssueKind::Repetition
            | IssueKind::HonorificOnNoun => 1,
        })
        .fold(0, u32::saturating_add)
}

/// User-turn header for a QA retry, naming what the previous attempt got wrong.
///
/// For example `Requirements for this passage: render 魔石 as "Magic Stone";
/// the name is spelled "Tatsuya".` Returns `None` when no issue is actionable.
pub fn requirements(ctx: &ChunkContext<'_>, issues: &[Issue]) -> Option<String> {
    let has = |kind: IssueKind| issues.iter().any(|issue| issue.kind == kind);
    let mut parts = Vec::new();

    for term in ctx.glossary {
        let detail = glossary_detail(term);
        if issues
            .iter()
            .any(|issue| issue.kind == IssueKind::GlossaryMissing && issue.detail == detail)
        {
            parts.push(format!("render {} as \"{}\"", term.original, term.english));
        }
    }

    let expected: BTreeSet<&str> = issues
        .iter()
        .filter(|issue| issue.kind == IssueKind::NameNearMiss)
        .filter_map(|issue| issue.detail.rsplit_once(NEAR_MISS_SEPARATOR))
        .map(|(_, expected)| expected)
        .filter(|expected| ctx.known_names.iter().any(|name| name == expected))
        .collect();
    for name in expected {
        parts.push(format!("the name is spelled \"{name}\""));
    }

    if has(IssueKind::Repetition) {
        parts.push("do not repeat lines or phrases".to_string());
    }
    if has(IssueKind::ForeignScript) {
        parts.push("write only English, with no Japanese or Chinese characters".to_string());
    }

    (!parts.is_empty()).then(|| format!("Requirements for this passage: {}.", parts.join("; ")))
}

/// Issue detail for a missing glossary term; [`requirements`] matches on it.
fn glossary_detail(term: &TermEntry) -> String {
    format!("{} → {}", term.original, term.english)
}

/// Whether `term` occurs in `source` other than inside an occurrence of a longer
/// glossary term that contains it (`認識阻害` inside `認識阻害の魔法`). Such nested
/// occurrences are rendered by the longer term, so requiring both would demand two
/// renderings for the same words.
fn occurs_outside_longer_terms(source: &str, term: &TermEntry, glossary: &[TermEntry]) -> bool {
    let covering: Vec<(usize, usize)> = glossary
        .iter()
        .filter(|other| {
            other.original.len() > term.original.len() && other.original.contains(&term.original)
        })
        .flat_map(|other| {
            source
                .match_indices(other.original.as_str())
                .map(|(start, matched)| (start, start + matched.len()))
        })
        .collect();
    source.match_indices(term.original.as_str()).any(|(start, matched)| {
        let end = start + matched.len();
        !covering
            .iter()
            .any(|&(cover_start, cover_end)| cover_start <= start && end <= cover_end)
    })
}

/// Whether `ratio` is outside `[low, high]`.
fn outside(ratio: f64, [low, high]: [f64; 2]) -> bool {
    ratio < low || ratio > high
}

/// Number of lines with non-whitespace content.
fn non_empty_lines(text: &str) -> usize {
    text.lines().filter(|line| !line.trim().is_empty()).count()
}

/// Latin letters and digits; kana and CJK are not word characters, so a name
/// glued to Japanese (`Tatsuyaは`) is still a whole word.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() && (c as u32) < 0x0250
}

/// Whether `word` occurs in `haystack` as a whole word, optionally followed by a
/// plural `s`.
fn contains_word(haystack: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    haystack.match_indices(word).any(|(start, _)| {
        let before_ok = !haystack[..start]
            .chars()
            .next_back()
            .is_some_and(is_word_char);
        let mut after = haystack[start + word.len()..].chars();
        let after_ok = match after.next() {
            None => true,
            Some('s') => !after.next().is_some_and(is_word_char),
            Some(c) => !is_word_char(c),
        };
        before_ok && after_ok
    })
}

/// Detect a word 4-gram repeated too often and runs of identical lines.
fn repetition(output: &str) -> Vec<Issue> {
    let mut issues = Vec::new();

    let words: Vec<String> = output
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|word| !word.is_empty())
        .collect();
    let mut counts: HashMap<&[String], usize> = HashMap::new();
    for gram in words.windows(4) {
        *counts.entry(gram).or_default() += 1;
    }
    let worst = counts
        .into_iter()
        .filter(|&(_, count)| count > REPEATED_NGRAM_MAX)
        .max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.0.cmp(left.0)));
    if let Some((gram, count)) = worst {
        issues.push(Issue {
            kind: IssueKind::Repetition,
            detail: format!("\"{}\" ×{count}", gram.join(" ")),
        });
    }

    let mut previous: Option<&str> = None;
    let mut run = 0;
    let mut longest: Option<(&str, usize)> = None;
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        run = if previous == Some(line) { run + 1 } else { 1 };
        previous = Some(line);
        if run >= REPEATED_LINE_RUN
            && line.chars().any(char::is_alphanumeric)
            && longest.is_none_or(|(_, best)| run > best)
        {
            longest = Some((line, run));
        }
    }
    if let Some((line, run)) = longest {
        issues.push(Issue {
            kind: IssueKind::Repetition,
            detail: format!("line \"{line}\" ×{run} in a row"),
        });
    }

    issues
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::honorifics::HonorificPolicy;

    fn term(original: &str, english: &str) -> TermEntry {
        TermEntry {
            original: original.to_string(),
            english: english.to_string(),
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    fn kinds(issues: &[Issue], kind: IssueKind) -> Vec<&Issue> {
        issues.iter().filter(|issue| issue.kind == kind).collect()
    }

    fn check(source: &str, glossary: &[TermEntry], known: &[String], output: &str) -> Vec<Issue> {
        let ctx = ChunkContext {
            source,
            glossary,
            known_names: known,
            output_rules: None,
        };
        check_chunk(&ctx, output, &QaConfig::default())
    }

    #[test]
    fn test_issue_kind_names_match_serialized_names() {
        for kind in [
            IssueKind::Failed,
            IssueKind::Truncated,
            IssueKind::ForeignScript,
            IssueKind::GlossaryMissing,
            IssueKind::NameMissing,
            IssueKind::NameNearMiss,
            IssueKind::ParagraphMismatch,
            IssueKind::LengthRatio,
            IssueKind::Repetition,
            IssueKind::HonorificOnNoun,
        ] {
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::Value::from(kind.to_string())
            );
        }
    }

    #[test]
    fn test_glossary_miss_only_when_term_in_chunk_and_plural_passes() {
        let glossary = [term("魔石", "Magic Stone")];

        let missed = check("魔石を拾った。", &glossary, &[], "He picked up a gem.");
        assert_eq!(
            kinds(&missed, IssueKind::GlossaryMissing),
            [&Issue {
                kind: IssueKind::GlossaryMissing,
                detail: "魔石 → Magic Stone".to_string(),
            }]
        );

        let plural = check(
            "魔石を拾った。",
            &glossary,
            &[],
            "He picked up the magic stones.",
        );
        assert!(kinds(&plural, IssueKind::GlossaryMissing).is_empty());

        let absent = check("石を拾った。", &glossary, &[], "He picked up a stone.");
        assert!(kinds(&absent, IssueKind::GlossaryMissing).is_empty());
    }

    #[test]
    fn test_glossary_term_nested_in_longer_term_is_not_required_separately() {
        let glossary = [
            term("認識阻害", "Perception Interference"),
            term("認識阻害の魔法", "Perception Inhibition Magic"),
        ];

        let nested = check(
            "認識阻害の魔法をかけた。",
            &glossary,
            &[],
            "I cast Perception Inhibition Magic.",
        );
        assert!(kinds(&nested, IssueKind::GlossaryMissing).is_empty());

        let also_alone = check(
            "認識阻害の魔法をかけた。認識阻害は便利だ。",
            &glossary,
            &[],
            "I cast Perception Inhibition Magic. It is handy.",
        );
        assert_eq!(
            kinds(&also_alone, IssueKind::GlossaryMissing),
            [&Issue {
                kind: IssueKind::GlossaryMissing,
                detail: "認識阻害 → Perception Interference".to_string(),
            }]
        );
    }

    #[test]
    fn test_near_miss_flagged_only_when_name_in_source() {
        let known = names(&["Tatsuya", "Clarissabel"]);

        let flagged = check("Tatsuyaは笑った。", &[], &known, "Tatyuya laughed.");
        assert_eq!(
            kinds(&flagged, IssueKind::NameNearMiss),
            [&Issue {
                kind: IssueKind::NameNearMiss,
                detail: "Tatyuya ≈ Tatsuya".to_string(),
            }]
        );

        let absent = check("彼は笑った。", &[], &known, "Tatyuya laughed.");
        assert!(kinds(&absent, IssueKind::NameNearMiss).is_empty());

        let ordinary = check(
            "Tatsuyaは笑った。",
            &[],
            &known,
            "Then Tatsuya laughed. Suddenly the Tavern Keeper sighed.",
        );
        assert!(kinds(&ordinary, IssueKind::NameNearMiss).is_empty());
        assert!(kinds(&ordinary, IssueKind::NameMissing).is_empty());
    }

    #[test]
    fn test_near_miss_skipped_when_found_word_is_in_source() {
        let known = names(&["Daryl", "Darryl"]);
        let issues = check(
            "DarrylとDarylが来た。",
            &[],
            &known,
            "Darryl and Daryl came.",
        );
        assert!(kinds(&issues, IssueKind::NameNearMiss).is_empty());
    }

    #[test]
    fn test_name_missing_is_whole_word_and_case_insensitive() {
        let known = names(&["Al", "Tatsuya"]);
        let issues = check("Tatsuyaは来た。Alsoは無い。", &[], &known, "He came.");
        assert_eq!(
            kinds(&issues, IssueKind::NameMissing),
            [&Issue {
                kind: IssueKind::NameMissing,
                detail: "Tatsuya".to_string(),
            }]
        );

        let shouted = check("Tatsuyaは来た。", &[], &known, "TATSUYA came.");
        assert!(kinds(&shouted, IssueKind::NameMissing).is_empty());
    }

    #[test]
    fn test_paragraph_thresholds_at_boundaries() {
        let source = "あ\n".repeat(20);
        let lines = |n: usize| "a\n\n".repeat(n);
        let paragraphs = |n: usize| check(&source, &[], &[], &lines(n));

        assert!(kinds(&paragraphs(17), IssueKind::ParagraphMismatch).is_empty());
        assert_eq!(
            kinds(&paragraphs(16), IssueKind::ParagraphMismatch).len(),
            1
        );
        assert!(kinds(&paragraphs(23), IssueKind::ParagraphMismatch).is_empty());
        assert_eq!(
            kinds(&paragraphs(24), IssueKind::ParagraphMismatch).len(),
            1
        );

        // Ratio 0.5 but only two lines apart.
        let small = check("あ\nい\nう\nえ", &[], &[], "a\nb");
        assert!(kinds(&small, IssueKind::ParagraphMismatch).is_empty());
        let three_apart = check("あ\nい\nう\nえ\nお\nか", &[], &[], "a\nb\nc");
        assert_eq!(kinds(&three_apart, IssueKind::ParagraphMismatch).len(), 1);
    }

    #[test]
    fn test_length_thresholds_at_boundaries() {
        let source = "あ".repeat(100);
        let length = |n: usize| check(&source, &[], &[], &"a".repeat(n));

        assert!(kinds(&length(120), IssueKind::LengthRatio).is_empty());
        assert_eq!(kinds(&length(119), IssueKind::LengthRatio).len(), 1);
        assert!(kinds(&length(400), IssueKind::LengthRatio).is_empty());
        assert_eq!(kinds(&length(401), IssueKind::LengthRatio).len(), 1);
    }

    #[test]
    fn test_foreign_script_is_flagged() {
        let issues = check("あ", &[], &[], "He said ありがとう.");
        assert_eq!(kinds(&issues, IssueKind::ForeignScript).len(), 1);
    }

    #[test]
    fn test_repetition_detected_and_normal_prose_passes() {
        let looping = "I will not go. ".repeat(9);
        let issues = repetition(&looping);
        assert_eq!(
            issues,
            [Issue {
                kind: IssueKind::Repetition,
                detail: "\"i will not go\" ×9".to_string(),
            }]
        );
        assert!(repetition(&"I will not go. ".repeat(8)).is_empty());

        let lines = "Run.\n\nRun.\n\nRun.\n\nRun.\n";
        assert_eq!(repetition(lines).len(), 1);
        assert!(repetition("Run.\nRun.\nRun.\nWalk.\nRun.").is_empty());
        assert!(repetition("……\n……\n……\n……").is_empty());

        let prose = "Tatsuya looked out of the window. The rain had stopped, and the \
                     village square was empty again.\n\n\"Shall we go?\" Ruri asked.\n\n\
                     He nodded and picked up his bag. The road to the capital was long, \
                     but the weather would hold until evening.";
        assert!(repetition(prose).is_empty());
    }

    #[test]
    fn test_retries_on_respects_config_and_record_only_kinds() {
        let mut cfg = QaConfig::default();
        assert!(cfg.retries_on(IssueKind::NameNearMiss));
        assert!(cfg.retries_on(IssueKind::GlossaryMissing));
        assert!(!cfg.retries_on(IssueKind::LengthRatio));
        assert!(!cfg.retries_on(IssueKind::NameMissing));

        cfg.retry_on = IssueClass::ALL.to_vec();
        assert!(cfg.retries_on(IssueKind::LengthRatio));
        assert!(!cfg.retries_on(IssueKind::NameMissing));
        assert!(!cfg.retries_on(IssueKind::HonorificOnNoun));
    }

    #[test]
    fn test_issue_class_names_match_serde_and_from_str() {
        for class in IssueClass::ALL {
            let json = serde_json::to_string(&class).unwrap();
            assert_eq!(json, format!("\"{class}\""));
            assert_eq!(class.as_str().parse::<IssueClass>(), Ok(class));
            assert_eq!(serde_json::from_str::<IssueClass>(&json).unwrap(), class);
        }
        assert!("Names".parse::<IssueClass>().is_err());
        assert!("stale".parse::<IssueClass>().is_err());
    }

    #[test]
    fn test_issue_class_matches_kinds() {
        assert!(IssueClass::Names.matches(IssueKind::NameNearMiss));
        assert!(IssueClass::Names.matches(IssueKind::NameMissing));
        assert!(IssueClass::Foreign.matches(IssueKind::ForeignScript));
        assert!(!IssueClass::Foreign.matches(IssueKind::GlossaryMissing));
    }

    #[test]
    fn test_qa_config_defaults_and_validation() {
        let cfg: QaConfig = serde_json::from_str(r#"{"retry_on": ["failed", "names"]}"#).unwrap();
        assert_eq!(cfg.max_retries, 1);
        assert_eq!(cfg.retry_on, [IssueClass::Failed, IssueClass::Names]);
        assert_eq!(cfg.length_ratio, [1.2, 4.0]);
        assert!(QaConfig::default().validate().is_ok());

        for bad in [
            [0.0, 1.0],
            [1.2, 1.2],
            [2.0, 1.0],
            [f64::NAN, 1.0],
            [1.0, f64::INFINITY],
        ] {
            let cfg = QaConfig {
                length_ratio: bad,
                ..QaConfig::default()
            };
            assert!(cfg.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn test_severity_orders_failed_truncated_foreign_others() {
        let issue = |kind| Issue {
            kind,
            detail: String::new(),
        };
        let others = vec![issue(IssueKind::LengthRatio); 50];
        let foreign = [issue(IssueKind::ForeignScript)];
        let truncated = [issue(IssueKind::Truncated)];
        let failed = [issue(IssueKind::Failed)];
        assert_eq!(severity(&[]), 0);
        assert!(severity(&others) < severity(&foreign));
        assert!(severity(&[foreign.to_vec(), others.clone()].concat()) < severity(&truncated));
        assert!(severity(&vec![issue(IssueKind::Truncated); 50]) < severity(&failed));
    }

    #[test]
    fn test_requirements_names_terms_and_spellings() {
        let glossary = [term("魔石", "Magic Stone"), term("王都", "Royal Capital")];
        let known = names(&["Tatsuya"]);
        let ctx = ChunkContext {
            source: "Tatsuyaは魔石を拾った。",
            glossary: &glossary,
            known_names: &known,
            output_rules: None,
        };
        let issues = check_chunk(&ctx, "Tatyuya picked up a gem.", &QaConfig::default());
        assert_eq!(
            requirements(&ctx, &issues).as_deref(),
            Some(
                "Requirements for this passage: render 魔石 as \"Magic Stone\"; \
                 the name is spelled \"Tatsuya\"."
            )
        );

        let record_only = [Issue {
            kind: IssueKind::LengthRatio,
            detail: "x".to_string(),
        }];
        assert_eq!(requirements(&ctx, &record_only), None);
    }

    #[test]
    fn test_check_honorifics_flags_non_names_once() {
        let rules = OutputRules::new(HonorificPolicy::Keep, ["Ruri"], &[]).unwrap();
        let issues = check_honorifics("Ruri-chan fed cat-san. Cat-san ate. cat-san slept.", &rules);
        let details: Vec<&str> = issues.iter().map(|issue| issue.detail.as_str()).collect();
        assert_eq!(details, ["cat-san", "Cat-san"]);
        assert!(
            issues
                .iter()
                .all(|issue| issue.kind == IssueKind::HonorificOnNoun)
        );
    }
}
