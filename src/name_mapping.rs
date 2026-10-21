//! Name mapping store for character name translations.
//!
//! Provides persistent storage of character name mappings. Vote totals determine the
//! derived `english` and `count` fields unless a manual `override` selects a rendering.

use crate::error::NameMappingError;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// Regex for detecting bad characters in original names.
/// Names shouldn't contain punctuation, whitespace, or separators.
static BAD_ORIGINAL_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[\s・･｡､,，。／/：:;!！?？\-—–‑·（）()［\]{}＜＞<>『』「」〈〉【】]")
        .expect("Invalid BAD_ORIGINAL_REGEX")
});

/// Regex for detecting Japanese honorific suffixes.
static HONORIFIC_SUFFIX_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(さん|ちゃん|くん|君|様|さま|殿|氏|先生|先輩|嬢)$")
        .expect("Invalid HONORIFIC_SUFFIX_REGEX")
});

/// Regex for detecting household suffixes such as `バーサーム家` or `田中一家`.
///
/// Substituting the whole word with a bare name would drop "family"/"house".
static HOUSEHOLD_SUFFIX_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r".(家|一家|一族)$").expect("Invalid HOUSEHOLD_SUFFIX_REGEX")
});

/// Regex for a katakana name followed by a title or place suffix, e.g. `メインティア王`
/// or `ミーク姫`.
///
/// Kanji bases are left alone because surnames such as `宮城` end in these characters.
static KATAKANA_TITLE_SUFFIX_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^[ァ-ヺー―]+(王|王国|国|公国|帝国|城|領|軍|族|団|村|町|街|山|川|森|島|教|姫|公|卿|公爵|侯爵|伯爵|子爵|男爵|将軍|団長|隊長|師匠)$",
    )
    .expect("Invalid KATAKANA_TITLE_SUFFIX_REGEX")
});

/// Regex for royal titles on any base, e.g. `大上王` or `ジュカ王子`.
///
/// Personal names essentially never end in these, so the rule is safe for kanji too.
static ROYAL_TITLE_SUFFIX_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r".(王|王子|王女|皇子|皇女|殿下|陛下)$").expect("Invalid ROYAL_TITLE_SUFFIX_REGEX")
});

/// Returns whether a Japanese original can be stored as a name.
///
/// Rejects punctuation, pronouns and titles, honorifics, and name-plus-suffix words;
/// the bare name inside a suffixed word is substituted on its own.
fn is_valid_original_name(original: &str) -> bool {
    original.chars().nth(1).is_some()
        && !BAD_ORIGINAL_REGEX.is_match(original)
        && !ORIGINAL_NAME_DENYLIST.contains(&original)
        && !HONORIFIC_SUFFIX_REGEX.is_match(original)
        && !HOUSEHOLD_SUFFIX_REGEX.is_match(original)
        && !KATAKANA_TITLE_SUFFIX_REGEX.is_match(original)
        && !ROYAL_TITLE_SUFFIX_REGEX.is_match(original)
}

/// English honorifics to reject.
const ENGLISH_HONORIFICS: &[&str] = &[
    "-san", "-chan", "-kun", "-sama", " san", " chan", " kun", " sama",
];

/// Returns whether text contains Japanese kana or CJK ideographs.
fn contains_japanese_script(text: &str) -> bool {
    crate::utils::count_cjk_chars(text) > 0
}

/// Returns whether an English rendering meets the name scout's constraints.
fn is_valid_english_rendering(english: &str) -> bool {
    let english_lower = english.to_lowercase();
    !english.is_empty()
        && !english.chars().any(char::is_whitespace)
        && !contains_japanese_script(english)
        && !ENGLISH_HONORIFICS
            .iter()
            .any(|honorific| english_lower.contains(honorific))
}

/// Returns whether a glossary term is safe to persist and include in a prompt.
fn is_valid_term_text(term: &str) -> bool {
    !term.is_empty() && term.trim() == term && !term.chars().any(char::is_control)
}

/// Returns whether a glossary original is worth matching against chapter text.
///
/// Single characters such as `市` match inside unrelated words (`都市`, `市場`).
fn is_valid_term_original(original: &str) -> bool {
    is_valid_term_text(original) && original.chars().nth(1).is_some()
}

/// Returns whether a glossary rendering adds information for the translator.
///
/// Renderings identical to the original (`HP`, `LV3`) or still containing Japanese
/// script (`Famed伯乐`) are rejected.
fn is_valid_term_rendering(original: &str, english: &str) -> bool {
    is_valid_term_text(english) && english != original && !contains_japanese_script(english)
}

/// Key shared by orthographic variants of the same name.
///
/// Hiragana is folded to katakana (`ぺーリス`/`ペーリス`), dashes used as long-vowel
/// marks become `ー` (`ヒ―データ`/`ヒーデータ`), and small vowels become full size
/// (`ヴィヴアル`/`ヴィヴァル`). Long-vowel marks are kept, because `ヒーラ` (a name) and
/// `ヒーラー` ("healer") differ.
fn name_variant_key(original: &str) -> String {
    original
        .chars()
        .map(|c| {
            let c = match c {
                'ぁ'..='ゖ' => char::from_u32(c as u32 + 0x60).unwrap_or(c),
                '―' | '—' | '－' => 'ー',
                other => other,
            };
            match c {
                'ァ' => 'ア',
                'ィ' => 'イ',
                'ゥ' => 'ウ',
                'ェ' => 'エ',
                'ォ' => 'オ',
                other => other,
            }
        })
        .collect()
}

/// Give every kana variant of a name the same rendering.
///
/// Votes are pooled across variants; the first manual override (by key order) wins.
fn unify_name_variants(names: &mut HashMap<String, NameInfo>) {
    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
    for original in names.keys() {
        groups
            .entry(name_variant_key(original))
            .or_default()
            .push(original.clone());
    }

    for mut members in groups.into_values().filter(|members| members.len() > 1) {
        members.sort_unstable();
        let mut pooled: HashMap<String, u32> = HashMap::new();
        for member in &members {
            for (english, count) in &names[member].votes {
                *pooled.entry(english.clone()).or_insert(0) += count;
            }
        }
        let r#override = members
            .iter()
            .find_map(|member| names[member].r#override.clone());
        let mut english = names[&members[0]].english.clone();
        let mut count = None;
        recalculate_selection(&pooled, r#override.as_ref(), &mut english, &mut count);

        for member in &members {
            if let Some(info) = names.get_mut(member) {
                info.english = english.clone();
                info.count = count;
            }
        }
    }
}

/// Return name substitutions ordered longest original first.
fn name_replacements(names: &HashMap<String, NameInfo>) -> Vec<(&str, &str)> {
    let mut replacements: Vec<(&str, &str)> = names
        .iter()
        .filter_map(|(original, info)| {
            info.english
                .as_deref()
                .map(|english| (original.as_str(), english))
        })
        .collect();
    replacements.sort_by_key(|(original, _)| std::cmp::Reverse(original.len()));
    replacements
}

/// Returns whether a character continues a katakana word.
///
/// The `・` separator is excluded so `ジュカ・ヨーク` still splits into two names.
fn is_katakana_word_char(c: char) -> bool {
    matches!(c, 'ァ'..='ヺ' | 'ー' | 'ヽ' | 'ヾ')
}

/// Byte offsets where a name may be substituted in `text`.
///
/// An all-katakana name only matches as a whole katakana word, so `ロー` does not
/// rewrite `スローダ` and `ゲン` does not rewrite `クノゲン`. A `ー` before the name only
/// joins it to a katakana word when katakana precedes the `ー`, so the drawn-out
/// interjection in `あらーリノスちゃん` still lets `リノス` match. Other names match
/// anywhere because kanji and kana word boundaries cannot be detected reliably.
fn name_match_offsets<'a>(text: &'a str, original: &'a str) -> impl Iterator<Item = usize> + 'a {
    let katakana_only = original.chars().all(is_katakana_word_char);
    text.match_indices(original).filter_map(move |(start, _)| {
        if katakana_only {
            let mut preceding = text[..start].chars().rev();
            let before = match preceding.next() {
                Some('ー') => preceding.next().filter(|&c| is_katakana_word_char(c)),
                other => other,
            };
            let after = text[start + original.len()..].chars().next();
            if before.is_some_and(is_katakana_word_char) || after.is_some_and(is_katakana_word_char)
            {
                return None;
            }
        }
        Some(start)
    })
}

/// Replace every substitutable occurrence of one name.
fn replace_name(text: &str, original: &str, english: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut copied_until = 0;
    for start in name_match_offsets(text, original) {
        result.push_str(&text[copied_until..start]);
        result.push_str(english);
        copied_until = start + original.len();
    }
    result.push_str(&text[copied_until..]);
    result
}

/// Returns whether name substitution rewrites part of a glossary term's Japanese.
///
/// Such a term can never appear in name-substituted chapter text, and the name
/// mapping already decides its rendering.
fn is_shadowed_by_name(term: &str, names: &HashMap<String, NameInfo>) -> bool {
    names.iter().any(|(original, info)| {
        info.english.is_some() && name_match_offsets(term, original).next().is_some()
    })
}

/// Words that the name scout tends to incorrectly label as character names.
///
/// This is intentionally a simple denylist (exact match) so we don't accidentally
/// filter out legitimate names. Single characters are rejected separately.
const ORIGINAL_NAME_DENYLIST: &[&str] = &[
    // Common Japanese pronouns / self-references
    "彼女",
    "あいつ",
    "こいつ",
    "そいつ",
    "こちとら",
    "こちら",
    "自分",
    "わたし",
    "わたくし",
    "おれ",
    "ぼく",
    "うち",
    "あなた",
    "きみ",
    "お前",
    "おまえ",
    "貴様",
    // Plurals and groups
    "彼ら",
    "彼女ら",
    "俺たち",
    "僕ら",
    "私たち",
    "あなたたち",
    "みんな",
    "一団",
    // Kinship terms
    "父上",
    "母上",
    "兄上",
    "姉上",
    "父親",
    "母親",
    "祖父",
    "祖母",
    "叔父",
    "叔母",
    "伯父",
    "伯母",
    "息子",
    // Titles, ranks, and occupations
    "国王",
    "皇帝",
    "皇帝陛下",
    "陛下",
    "殿下",
    "女王",
    "皇后",
    "王妃",
    "王子",
    "王女",
    "皇子",
    "皇女",
    "将軍",
    "公爵",
    "侯爵",
    "伯爵",
    "子爵",
    "男爵",
    "騎士",
    "賢者",
    "勇者",
    "魔王",
    "聖女",
    "師匠",
    "団長",
    "隊長",
    "主人",
    "支配人",
    "女将",
    "宰相",
    "大臣",
    "公女",
    "公子",
    "大公",
    "王太子",
    "皇太子",
    "女官",
    "侍女",
    "侍従",
    "執事",
    "メイド",
    // Archaic or dialectal pronouns
    "そこもと",
    "そなた",
    "其方",
    "貴殿",
    "貴方",
    "拙者",
    "わし",
    "あたし",
    "おいら",
    "わらわ",
    // Interjections
    "ひー",
    "ひぃ",
    "ヒー",
];

/// Indicates what part of a name this is (family name, given name, or unknown).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NamePart {
    Family,
    Given,
    #[default]
    Unknown,
}

impl std::str::FromStr for NamePart {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_lowercase().as_str() {
            "family" => Self::Family,
            "given" => Self::Given,
            _ => Self::Unknown,
        })
    }
}

/// A name entry for recording votes.
#[derive(Debug, Clone)]
pub struct NameEntry {
    /// Original Japanese name.
    pub original: String,
    /// English translation.
    pub english: String,
    /// Which part of the name this is.
    pub part: NamePart,
}

/// A glossary entry for recording terminology votes.
#[derive(Debug, Clone)]
pub struct TermEntry {
    /// Original Japanese terminology.
    pub original: String,
    /// Canonical English rendering.
    pub english: String,
}

/// Renderings already selected for names and terms, used to steer the scout.
#[derive(Debug, Clone, Default)]
pub struct KnownMappings {
    /// Selected name renderings, sorted by original.
    pub names: Vec<TermEntry>,
    /// Selected glossary renderings, sorted by original.
    pub terms: Vec<TermEntry>,
}

impl KnownMappings {
    /// Returns whether there are no known renderings.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty() && self.terms.is_empty()
    }

    /// Return the entries whose original occurs in `text`.
    ///
    /// Names use the same katakana word-boundary rule as name substitution.
    pub fn present_in(&self, text: &str) -> Self {
        Self {
            names: self
                .names
                .iter()
                .filter(|entry| name_match_offsets(text, &entry.original).next().is_some())
                .cloned()
                .collect(),
            terms: self
                .terms
                .iter()
                .filter(|entry| text.contains(entry.original.as_str()))
                .cloned()
                .collect(),
        }
    }
}

/// Recalculate a selected rendering from manual override and vote totals.
fn recalculate_selection(
    votes: &HashMap<String, u32>,
    r#override: Option<&String>,
    english: &mut Option<String>,
    count: &mut Option<u32>,
) {
    if let Some(english_override) = r#override {
        *english = Some(english_override.clone());
        *count = votes.get(english_override).copied();
        return;
    }

    let Some(best_count) = votes.values().copied().max() else {
        *english = None;
        *count = None;
        return;
    };

    if english
        .as_ref()
        .is_some_and(|current| votes.get(current) == Some(&best_count))
    {
        *count = Some(best_count);
        return;
    }

    let best_english = votes
        .iter()
        .filter(|(_, count)| **count == best_count)
        .map(|(english, _)| english)
        .min()
        .cloned();
    *english = best_english;
    *count = Some(best_count);
}

/// Information about a single name in the mapping store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NameInfo {
    /// Which part of the name this is.
    pub part: NamePart,
    /// Vote counts for each English translation.
    pub votes: HashMap<String, u32>,
    /// Manually selected English rendering, if any.
    ///
    /// This takes precedence over vote totals while it is valid.
    #[serde(default, rename = "override", skip_serializing_if = "Option::is_none")]
    pub r#override: Option<String>,
    /// Recalculated selected English rendering from the override or vote totals.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub english: Option<String>,
    /// Recalculated vote count of the selected English rendering.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
}

impl NameInfo {
    /// Create a new empty NameInfo.
    pub fn new(part: NamePart) -> Self {
        Self {
            part,
            votes: HashMap::new(),
            r#override: None,
            english: None,
            count: None,
        }
    }

    /// Recalculate the selected translation from the override or votes.
    pub fn recalculate_best(&mut self) {
        recalculate_selection(
            &self.votes,
            self.r#override.as_ref(),
            &mut self.english,
            &mut self.count,
        );
    }
}

/// Information about a single glossary term in the mapping store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TermInfo {
    /// Vote counts for each English rendering.
    pub votes: HashMap<String, u32>,
    /// Manually selected English rendering, if any.
    ///
    /// This takes precedence over vote totals while it is valid.
    #[serde(default, rename = "override", skip_serializing_if = "Option::is_none")]
    pub r#override: Option<String>,
    /// Recalculated selected English rendering from the override or vote totals.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub english: Option<String>,
    /// Recalculated vote count of the selected English rendering.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
}

impl TermInfo {
    /// Create a new empty TermInfo.
    pub fn new() -> Self {
        Self {
            votes: HashMap::new(),
            r#override: None,
            english: None,
            count: None,
        }
    }

    /// Recalculate the selected rendering from the override or votes.
    pub fn recalculate_best(&mut self) {
        recalculate_selection(
            &self.votes,
            self.r#override.as_ref(),
            &mut self.english,
            &mut self.count,
        );
    }
}

/// The full name and terminology mapping data structure.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NameMappingData {
    /// Map from original Japanese names to their info.
    pub names: HashMap<String, NameInfo>,
    /// Map from original Japanese terminology to its canonical rendering.
    #[serde(default)]
    pub terms: HashMap<String, TermInfo>,
    /// List of chapter numbers that have had names scouted.
    pub coverage: Vec<u32>,
    /// List of chapter numbers that have had terminology scouted.
    #[serde(default)]
    pub term_coverage: Vec<u32>,
}

/// Name mapping store for a specific novel.
pub struct NameMappingStore {
    /// Path to the JSON file.
    filepath: PathBuf,
    /// The mapping data.
    data: NameMappingData,
}

impl NameMappingStore {
    /// Create a new NameMappingStore for the given module and novel ID.
    ///
    /// # Arguments
    /// * `names_dir` - Directory where name mapping files are stored.
    /// * `module_name` - Name of the scraper module (e.g., "syosetu").
    /// * `novel_id` - Unique identifier for the novel.
    pub fn new(
        names_dir: &Path,
        module_name: &str,
        novel_id: &str,
    ) -> Result<Self, NameMappingError> {
        // Build filename: "{module_name}: {novel_id}.json"
        // On Windows, replace : with - since colons aren't allowed in filenames
        let filename = if cfg!(windows) {
            format!("{} - {}.json", module_name, novel_id)
        } else {
            format!("{}: {}.json", module_name, novel_id)
        };

        let filepath = names_dir.join(&filename);

        let mut store = Self {
            filepath,
            data: NameMappingData::default(),
        };

        // Load from disk if file exists
        if store.filepath.exists() {
            store.reload_from_disk()?;
        }

        // Purge bad votes on load
        store.purge_bad_votes();

        Ok(store)
    }

    /// Get the filepath for this store.
    pub fn filepath(&self) -> &Path {
        &self.filepath
    }

    /// Record votes from a list of name entries.
    pub fn record_votes(&mut self, entries: &[NameEntry]) {
        for entry in entries {
            // Validate entry
            if !is_valid_original_name(&entry.original)
                || !is_valid_english_rendering(&entry.english)
            {
                continue;
            }

            // Get or create entry
            let name_info = self
                .data
                .names
                .entry(entry.original.clone())
                .or_insert_with(|| NameInfo::new(entry.part.clone()));

            // Update part if we have a known part and current is unknown
            if name_info.part == NamePart::Unknown && entry.part != NamePart::Unknown {
                name_info.part = entry.part.clone();
            }

            // Increment vote count
            *name_info.votes.entry(entry.english.clone()).or_insert(0) += 1;

            // Recalculate best
            name_info.recalculate_best();
        }

        unify_name_variants(&mut self.data.names);
        self.purge_shadowed_terms();
    }

    /// Record votes from a list of glossary entries.
    pub fn record_term_votes(&mut self, entries: &[TermEntry]) {
        for entry in entries {
            if !is_valid_term_original(&entry.original)
                || !is_valid_term_rendering(&entry.original, &entry.english)
            {
                continue;
            }

            if is_shadowed_by_name(&entry.original, &self.data.names) {
                continue;
            }

            let term_info = self
                .data
                .terms
                .entry(entry.original.clone())
                .or_insert_with(TermInfo::new);
            *term_info.votes.entry(entry.english.clone()).or_insert(0) += 1;
            term_info.recalculate_best();
        }
    }

    /// Purge invalid name and glossary votes from the mapping.
    pub fn purge_bad_votes(&mut self) {
        self.data.names.retain(|original, info| {
            if !is_valid_original_name(original) {
                return false;
            }

            if info
                .r#override
                .as_deref()
                .is_some_and(|english| !is_valid_english_rendering(english))
            {
                info.r#override = None;
            }

            // Filter out invalid votes.
            info.votes
                .retain(|english, _| is_valid_english_rendering(english));

            // Recalculate best after filtering.
            info.recalculate_best();

            // Keep entries with valid votes or a valid manual override.
            !info.votes.is_empty() || info.r#override.is_some()
        });
        unify_name_variants(&mut self.data.names);

        self.data.terms.retain(|original, info| {
            if !is_valid_term_original(original) {
                return false;
            }

            if info
                .r#override
                .as_deref()
                .is_some_and(|english| !is_valid_term_text(english))
            {
                info.r#override = None;
            }

            info.votes
                .retain(|english, _| is_valid_term_rendering(original, english));
            info.recalculate_best();

            !info.votes.is_empty() || info.r#override.is_some()
        });
        self.purge_shadowed_terms();
    }

    /// Remove glossary terms whose Japanese contains a mapped name.
    fn purge_shadowed_terms(&mut self) {
        let names = &self.data.names;
        self.data
            .terms
            .retain(|original, _| !is_shadowed_by_name(original, names));
    }

    /// Check if a chapter has been scouted.
    pub fn is_chapter_covered(&self, chapter_number: u32) -> bool {
        self.data.coverage.contains(&chapter_number)
    }

    /// Check whether a chapter has had terminology scouted.
    pub fn is_chapter_terms_covered(&self, chapter_number: u32) -> bool {
        self.data.term_coverage.contains(&chapter_number)
    }

    /// Add chapters to the coverage list.
    pub fn add_coverage(&mut self, chapters: &[u32]) {
        let coverage_set: HashSet<u32> = self.data.coverage.iter().copied().collect();
        for &chapter in chapters {
            if !coverage_set.contains(&chapter) {
                self.data.coverage.push(chapter);
            }
        }
        // Sort for consistency
        self.data.coverage.sort_unstable();
    }

    /// Add chapters to the terminology coverage list.
    pub fn add_term_coverage(&mut self, chapters: &[u32]) {
        let coverage_set: HashSet<u32> = self.data.term_coverage.iter().copied().collect();
        for &chapter in chapters {
            if !coverage_set.contains(&chapter) {
                self.data.term_coverage.push(chapter);
            }
        }
        self.data.term_coverage.sort_unstable();
    }

    /// Apply name mappings to text, replacing Japanese names with English.
    /// Replaces longest matches first to handle overlapping names.
    pub fn apply_to_text(&self, text: &str) -> String {
        let mut result = text.to_string();
        for (original, english) in name_replacements(&self.data.names) {
            result = replace_name(&result, original, english);
        }

        result
    }

    /// Return canonical glossary entries present in name-substituted text.
    ///
    /// `mapped_text` must be the output of [`Self::apply_to_text`], so only terms the
    /// translation model will actually see are returned. Overridden terms rank first,
    /// then the most-voted terms, capped at `max_entries`.
    pub fn glossary_for_text(&self, mapped_text: &str, max_entries: usize) -> Vec<TermEntry> {
        if max_entries == 0 {
            return Vec::new();
        }

        let mut matching_terms: Vec<_> = self
            .data
            .terms
            .iter()
            .filter_map(|(original, info)| {
                let english = info.english.as_ref()?;
                mapped_text
                    .contains(original.as_str())
                    .then_some((original, english, info))
            })
            .collect();
        matching_terms.sort_by(|(left, _, left_info), (right, _, right_info)| {
            (right_info.r#override.is_some(), right_info.count)
                .cmp(&(left_info.r#override.is_some(), left_info.count))
                .then_with(|| left.cmp(right))
        });

        matching_terms
            .into_iter()
            .take(max_entries)
            .map(|(original, english, _)| TermEntry {
                original: original.clone(),
                english: english.clone(),
            })
            .collect()
    }

    /// Save the mapping to disk.
    pub fn save(&self) -> Result<(), NameMappingError> {
        // Ensure parent directory exists
        if let Some(parent) = self.filepath.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = serde_json::to_string_pretty(&self.data)?;
        std::fs::write(&self.filepath, content)
            .map_err(|e| NameMappingError::WriteError(e.to_string()))?;

        Ok(())
    }

    /// Reload the mapping from disk.
    pub fn reload_from_disk(&mut self) -> Result<(), NameMappingError> {
        let content = std::fs::read_to_string(&self.filepath)?;
        let data: NameMappingData = serde_json::from_str(&content)?;

        // Validate structure (serde already does this, but we ensure required fields)
        self.data = data;

        // Purge bad votes after reload
        self.purge_bad_votes();

        Ok(())
    }

    /// Get the number of names in the mapping.
    pub fn len(&self) -> usize {
        self.data.names.len()
    }

    /// Get the number of glossary terms in the mapping.
    pub fn term_len(&self) -> usize {
        self.data.terms.len()
    }

    /// Check if the mapping is empty.
    pub fn is_empty(&self) -> bool {
        self.data.names.is_empty()
    }

    /// Get the coverage list.
    pub fn coverage(&self) -> &[u32] {
        &self.data.coverage
    }

    /// Get an iterator over all name entries.
    pub fn names(&self) -> impl Iterator<Item = (&str, &NameInfo)> {
        self.data.names.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Return every selected name and term rendering.
    pub fn known_mappings(&self) -> KnownMappings {
        fn selected<'a, I>(entries: I) -> Vec<TermEntry>
        where
            I: Iterator<Item = (&'a String, Option<&'a String>)>,
        {
            let mut selected: Vec<TermEntry> = entries
                .filter_map(|(original, english)| {
                    english.map(|english| TermEntry {
                        original: original.clone(),
                        english: english.clone(),
                    })
                })
                .collect();
            selected.sort_unstable_by(|left, right| left.original.cmp(&right.original));
            selected
        }

        KnownMappings {
            names: selected(
                self.data
                    .names
                    .iter()
                    .map(|(original, info)| (original, info.english.as_ref())),
            ),
            terms: selected(
                self.data
                    .terms
                    .iter()
                    .map(|(original, info)| (original, info.english.as_ref())),
            ),
        }
    }

    /// Get the raw data (for testing/debugging).
    pub fn data(&self) -> &NameMappingData {
        &self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_name_part_from_str() {
        assert_eq!("family".parse::<NamePart>().unwrap(), NamePart::Family);
        assert_eq!("FAMILY".parse::<NamePart>().unwrap(), NamePart::Family);
        assert_eq!("given".parse::<NamePart>().unwrap(), NamePart::Given);
        assert_eq!("unknown".parse::<NamePart>().unwrap(), NamePart::Unknown);
        assert_eq!("invalid".parse::<NamePart>().unwrap(), NamePart::Unknown);
    }

    #[test]
    fn test_record_votes() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();

        // Record some votes
        store.record_votes(&[
            NameEntry {
                original: "田中".to_string(),
                english: "Tanaka".to_string(),
                part: NamePart::Family,
            },
            NameEntry {
                original: "田中".to_string(),
                english: "Tanaka".to_string(),
                part: NamePart::Family,
            },
            NameEntry {
                original: "太郎".to_string(),
                english: "Taro".to_string(),
                part: NamePart::Given,
            },
        ]);

        assert_eq!(store.len(), 2);

        let tanaka = store.data.names.get("田中").unwrap();
        assert_eq!(tanaka.english, Some("Tanaka".to_string()));
        assert_eq!(tanaka.count, Some(2));
        assert_eq!(tanaka.part, NamePart::Family);
    }

    #[test]
    fn test_bad_original_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();

        // Names with spaces or punctuation should be rejected
        store.record_votes(&[
            NameEntry {
                original: "田中 太郎".to_string(), // Contains space
                english: "TanakaTaro".to_string(),
                part: NamePart::Unknown,
            },
            NameEntry {
                original: "田中・太郎".to_string(), // Contains ・
                english: "TanakaTaro".to_string(),
                part: NamePart::Unknown,
            },
        ]);

        assert!(store.is_empty());
    }

    #[test]
    fn test_honorific_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();

        // Names with honorifics should be rejected
        store.record_votes(&[
            NameEntry {
                original: "田中さん".to_string(), // Contains -san
                english: "Tanaka".to_string(),
                part: NamePart::Family,
            },
            NameEntry {
                original: "田中".to_string(),
                english: "Tanaka-san".to_string(), // English has honorific
                part: NamePart::Family,
            },
        ]);

        assert!(store.is_empty());
    }

    #[test]
    fn test_whitespace_in_english_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();

        store.record_votes(&[NameEntry {
            original: "田中".to_string(),
            english: "Tanaka San".to_string(), // Contains space
            part: NamePart::Family,
        }]);

        assert!(store.is_empty());
    }

    #[test]
    fn test_original_denylist_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();

        store.record_votes(&[
            NameEntry {
                original: "彼女".to_string(),
                english: "Kanojo".to_string(),
                part: NamePart::Unknown,
            },
            NameEntry {
                original: "俺".to_string(),
                english: "Ore".to_string(),
                part: NamePart::Unknown,
            },
        ]);

        assert!(store.is_empty());
    }

    #[test]
    fn test_apply_to_text() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();

        store.record_votes(&[
            NameEntry {
                original: "田中".to_string(),
                english: "Tanaka".to_string(),
                part: NamePart::Family,
            },
            NameEntry {
                original: "太郎".to_string(),
                english: "Taro".to_string(),
                part: NamePart::Given,
            },
        ]);

        let text = "田中太郎は学校に行った。";
        let result = store.apply_to_text(text);
        assert_eq!(result, "TanakaTaroは学校に行った。");
    }

    #[test]
    fn test_longest_match_first() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();

        store.record_votes(&[
            NameEntry {
                original: "田中".to_string(),
                english: "Tanaka".to_string(),
                part: NamePart::Family,
            },
            NameEntry {
                original: "田中島".to_string(),
                english: "Tanakajima".to_string(),
                part: NamePart::Family,
            },
        ]);

        // The longer name must win where both match.
        assert_eq!(
            store.apply_to_text("田中島さんと田中さん"),
            "TanakajimaさんとTanakaさん"
        );
    }

    #[test]
    fn test_coverage_tracking() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();

        assert!(!store.is_chapter_covered(1));

        store.add_coverage(&[1, 3, 5]);
        assert!(store.is_chapter_covered(1));
        assert!(store.is_chapter_covered(3));
        assert!(!store.is_chapter_covered(2));

        // Adding duplicate should not create duplicates
        store.add_coverage(&[1, 2]);
        assert_eq!(store.coverage(), &[1, 2, 3, 5]);
    }

    #[test]
    fn test_save_and_reload() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();

        store.record_votes(&[NameEntry {
            original: "田中".to_string(),
            english: "Tanaka".to_string(),
            part: NamePart::Family,
        }]);
        store.add_coverage(&[1, 2, 3]);
        store.save().unwrap();

        // Create a new store from the same file
        let store2 = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        assert_eq!(store2.len(), 1);
        assert!(store2.is_chapter_covered(2));
    }

    #[test]
    fn test_vote_consensus() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();

        // Vote for different translations
        store.record_votes(&[
            NameEntry {
                original: "優子".to_string(),
                english: "Yuko".to_string(),
                part: NamePart::Given,
            },
            NameEntry {
                original: "優子".to_string(),
                english: "Yuuko".to_string(),
                part: NamePart::Given,
            },
            NameEntry {
                original: "優子".to_string(),
                english: "Yuko".to_string(),
                part: NamePart::Given,
            },
        ]);

        let info = store.data.names.get("優子").unwrap();
        assert_eq!(info.english, Some("Yuko".to_string())); // Yuko has 2 votes
        assert_eq!(info.count, Some(2));
    }

    #[test]
    fn test_name_override_beats_higher_vote_total_and_survives_new_votes() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        std::fs::write(
            store.filepath(),
            r#"{
                "names": {
                    "優子": {
                        "part": "given",
                        "votes": { "Yuuko": 1, "Yuko": 5 },
                        "override": "Yuuko",
                        "english": "Yuko",
                        "count": 5
                    }
                },
                "coverage": []
            }"#,
        )
        .unwrap();
        store.reload_from_disk().unwrap();

        assert_eq!(store.apply_to_text("優子"), "Yuuko");

        store.record_votes(&[
            NameEntry {
                original: "優子".to_string(),
                english: "Yuko".to_string(),
                part: NamePart::Given,
            },
            NameEntry {
                original: "優子".to_string(),
                english: "Yuko".to_string(),
                part: NamePart::Given,
            },
            NameEntry {
                original: "優子".to_string(),
                english: "Yuko".to_string(),
                part: NamePart::Given,
            },
        ]);
        store.save().unwrap();

        let reloaded = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        let info = reloaded.data.names.get("優子").unwrap();
        assert_eq!(reloaded.apply_to_text("優子"), "Yuuko");
        assert_eq!(info.english.as_deref(), Some("Yuuko"));
        assert_eq!(info.count, Some(1));
        assert_eq!(info.votes.get("Yuko"), Some(&8));
    }

    #[test]
    fn test_name_override_without_votes_survives_reload() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        std::fs::write(
            store.filepath(),
            r#"{
                "names": {
                    "優子": {
                        "part": "given",
                        "votes": {},
                        "override": "Yuuko"
                    }
                },
                "coverage": []
            }"#,
        )
        .unwrap();
        store.reload_from_disk().unwrap();
        store.save().unwrap();

        let reloaded = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        let info = reloaded.data.names.get("優子").unwrap();
        assert_eq!(reloaded.apply_to_text("優子"), "Yuuko");
        assert_eq!(info.count, None);
        assert!(info.votes.is_empty());
    }

    #[test]
    fn test_name_override_removal_restores_vote_winner() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        std::fs::write(
            store.filepath(),
            r#"{
                "names": {
                    "優子": {
                        "part": "given",
                        "votes": { "Yuuko": 1, "Yuko": 5 },
                        "override": "Yuuko"
                    }
                },
                "coverage": []
            }"#,
        )
        .unwrap();
        store.reload_from_disk().unwrap();

        let info = store.data.names.get_mut("優子").unwrap();
        info.r#override = None;
        info.recalculate_best();

        assert_eq!(info.english.as_deref(), Some("Yuko"));
        assert_eq!(info.count, Some(5));
    }

    #[test]
    fn test_name_override_legacy_mapping_loads() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        std::fs::write(
            store.filepath(),
            r#"{
                "names": {
                    "優子": {
                        "part": "given",
                        "votes": { "Yuuko": 1, "Yuko": 2 },
                        "english": "Yuko",
                        "count": 2
                    }
                },
                "coverage": []
            }"#,
        )
        .unwrap();
        store.reload_from_disk().unwrap();

        let info = store.data.names.get("優子").unwrap();
        assert_eq!(info.r#override, None);
        assert_eq!(info.english.as_deref(), Some("Yuko"));
        assert_eq!(info.count, Some(2));
    }

    #[test]
    fn test_term_votes_persist_and_filter_by_chapter_text() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        store.record_term_votes(&[
            TermEntry {
                original: "魔石".to_string(),
                english: "Magic Stone".to_string(),
            },
            TermEntry {
                original: "魔石".to_string(),
                english: "Mana Stone".to_string(),
            },
            TermEntry {
                original: "魔石".to_string(),
                english: "Magic Stone".to_string(),
            },
            TermEntry {
                original: "冒険者ギルド".to_string(),
                english: "Adventurers' Guild".to_string(),
            },
        ]);
        store.add_term_coverage(&[1]);
        store.save().unwrap();

        let reloaded = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        let glossary = reloaded.glossary_for_text("魔石を使った。", 100);

        assert!(reloaded.is_chapter_terms_covered(1));
        assert_eq!(glossary.len(), 1);
        assert_eq!(glossary[0].original, "魔石");
        assert_eq!(glossary[0].english, "Magic Stone");
        assert_eq!(
            reloaded.data.terms["魔石"].votes.get("Magic Stone"),
            Some(&2)
        );
    }

    #[test]
    fn test_name_tie_keeps_current_winner_after_save_and_reload() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        std::fs::write(
            store.filepath(),
            r#"{
                "names": {
                    "優子": {
                        "part": "given",
                        "votes": { "Yuko": 1, "Yuuko": 1 },
                        "english": "Yuko",
                        "count": 1
                    }
                },
                "coverage": []
            }"#,
        )
        .unwrap();
        store.reload_from_disk().unwrap();
        store.save().unwrap();

        let reloaded = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        assert_eq!(reloaded.data.names["優子"].english.as_deref(), Some("Yuko"));
    }

    #[test]
    fn test_term_tie_keeps_current_winner_after_save_and_reload() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        std::fs::write(
            store.filepath(),
            r#"{
                "names": {},
                "terms": {
                    "魔石": {
                        "votes": { "Magic Stone": 1, "Mana Stone": 1 },
                        "english": "Magic Stone",
                        "count": 1
                    }
                },
                "coverage": [],
                "term_coverage": []
            }"#,
        )
        .unwrap();
        store.reload_from_disk().unwrap();
        store.save().unwrap();

        let reloaded = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        assert_eq!(
            reloaded.data.terms["魔石"].english.as_deref(),
            Some("Magic Stone")
        );
    }

    #[test]
    fn test_legacy_mapping_requires_terminology_scout() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        std::fs::write(
            store.filepath(),
            r#"{
                "names": {},
                "coverage": [1]
            }"#,
        )
        .unwrap();
        store.reload_from_disk().unwrap();

        assert!(store.is_chapter_covered(1));
        assert!(!store.is_chapter_terms_covered(1));
    }

    fn name(original: &str, english: &str) -> NameEntry {
        NameEntry {
            original: original.to_string(),
            english: english.to_string(),
            part: NamePart::Given,
        }
    }

    fn term(original: &str, english: &str) -> TermEntry {
        TermEntry {
            original: original.to_string(),
            english: english.to_string(),
        }
    }

    #[test]
    fn test_kana_variants_share_pooled_rendering_and_override() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        store.record_votes(&[
            name("ヴィヴァル", "Vival"),
            name("ヴィヴァル", "Vival"),
            name("ヴィヴアル", "Vivarl"),
            name("ヴィヴアル", "Vivarl"),
            name("ヴィヴアル", "Vival"),
            name("ヒーラ", "Heera"),
            name("ヒーラー", "Healer"),
            name("ヒーデータ", "Heedata"),
            name("ヒーデータ", "Heedata"),
            name("ヒ―データ", "Hydata"),
            name("ぺーリス", "Pellis"),
            name("ペーリス", "Peris"),
            name("ペーリス", "Peris"),
        ]);

        assert_eq!(store.apply_to_text("ヴィヴアルとヴィヴァル"), "VivalとVival");
        assert_eq!(store.data.names["ヴィヴアル"].count, Some(3));
        assert_eq!(store.apply_to_text("ヒーラ、ヒーラー"), "Heera、Healer");
        assert_eq!(store.apply_to_text("ヒ―データとヒーデータ"), "HeedataとHeedata");
        assert_eq!(store.apply_to_text("ぺーリスとペーリス"), "PerisとPeris");

        store.data.names.get_mut("ヴィヴアル").unwrap().r#override = Some("Viviar".to_string());
        store.save().unwrap();
        let reloaded = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        assert_eq!(reloaded.apply_to_text("ヴィヴアルとヴィヴァル"), "ViviarとViviar");
        assert_eq!(reloaded.data.names["ヴィヴァル"].r#override, None);
    }

    #[test]
    fn test_uninformative_terms_and_japanese_renderings_are_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        store.record_term_votes(&[
            term("市", "Market"),
            term("HP", "HP"),
            term("名伯楽", "Famed伯乐"),
            term("結界", "Barrier"),
        ]);
        store.record_votes(&[name("マトカル", "Mato卡尔"), name("マトカル", "Matokal")]);

        let mut terms: Vec<_> = store.data.terms.keys().map(String::as_str).collect();
        terms.sort_unstable();
        assert_eq!(terms, ["結界"]);
        assert_eq!(store.data.names["マトカル"].votes.len(), 1);
        assert_eq!(store.glossary_for_text("都市の市場に結界", 100).len(), 1);
    }

    #[test]
    fn test_terms_containing_names_are_dropped_and_never_injected() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        store.record_term_votes(&[
            term("バーサーム", "Versam"),
            term("バーサーム侯爵", "Marquess Bersam"),
            term("魔石", "Magic Stone"),
        ]);
        store.record_votes(&[name("バーサーム", "Barsam")]);
        store.record_term_votes(&[term("バーサーム家", "Bersam Family")]);

        let original = "バーサーム侯爵は魔石を持つ。";
        let mapped = store.apply_to_text(original);
        let glossary = store.glossary_for_text(&mapped, 100);

        assert_eq!(mapped, "Barsam侯爵は魔石を持つ。");
        assert_eq!(glossary.len(), 1);
        assert_eq!(glossary[0].original, "魔石");
        assert_eq!(store.term_len(), 1);
    }

    #[test]
    fn test_glossary_cap_keeps_overrides_then_most_voted_terms() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        store.record_term_votes(&[
            term("暗殺", "assassination"),
            term("結界", "Barrier"),
            term("結界", "Barrier"),
            term("結界", "Barrier"),
            term("魔石", "Magic Stone"),
            term("魔石", "Magic Stone"),
            term("昆布", "kombu"),
        ]);
        store.data.terms.get_mut("昆布").unwrap().r#override = Some("Kelp".to_string());
        store.data.terms.get_mut("昆布").unwrap().recalculate_best();

        let glossary = store.glossary_for_text("暗殺、昆布、魔石、結界", 3);
        let originals: Vec<_> = glossary.iter().map(|entry| entry.original.as_str()).collect();

        assert_eq!(originals, ["昆布", "結界", "魔石"]);
        assert_eq!(glossary[0].english, "Kelp");
    }

    #[test]
    fn test_katakana_names_only_replace_whole_katakana_words() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        store.record_votes(&[name("ロー", "Ro"), name("ジュカ", "Juka"), name("田中", "Tanaka")]);
        store.record_term_votes(&[term("スローダ", "Sloda"), term("ジュカ王国", "Juka Kingdom")]);

        assert_eq!(
            store.apply_to_text("ローはスローダへ。ジュカ・ロー、ジュカ王国、ローー。あらーロー、スーロー。田中太郎"),
            "Roはスローダへ。Juka・Ro、Juka王国、ローー。あらーRo、スーロー。Tanaka太郎"
        );
        assert!(store.data.terms.contains_key("スローダ"));
        assert!(!store.data.terms.contains_key("ジュカ王国"));
    }

    #[test]
    fn test_existing_title_and_kinship_names_are_purged_on_load() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        std::fs::write(
            store.filepath(),
            r#"{
                "names": {
                    "父": { "part": "unknown", "votes": { "Father": 1 } },
                    "帝": { "part": "unknown", "votes": { "Emperor": 4 } },
                    "女": { "part": "unknown", "votes": { "Woman": 1 } },
                    "宰相": { "part": "unknown", "votes": { "Chancellor": 2 } },
                    "リコ": { "part": "given", "votes": { "Rico": 3 } }
                },
                "coverage": []
            }"#,
        )
        .unwrap();
        store.reload_from_disk().unwrap();

        assert_eq!(store.len(), 1);
        assert_eq!(
            store.apply_to_text("叔父上と彼女と宰相と帝国のリコ"),
            "叔父上と彼女と宰相と帝国のRico"
        );
    }

    #[test]
    fn test_known_mappings_use_selection_and_name_boundaries() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        store.record_votes(&[name("ロー", "Ro"), name("リコ", "Lico"), name("リコ", "Lico")]);
        store.record_votes(&[name("リコ", "Rico")]);
        store.data.names.get_mut("リコ").unwrap().r#override = Some("Rico".to_string());
        store.data.names.get_mut("リコ").unwrap().recalculate_best();
        store.record_term_votes(&[term("結界", "Barrier"), term("魔石", "Magic Stone")]);

        let present = store.known_mappings().present_in("リコはスローダで結界を張った。");
        let names: Vec<_> = present
            .names
            .iter()
            .map(|entry| (entry.original.as_str(), entry.english.as_str()))
            .collect();
        let terms: Vec<_> = present.terms.iter().map(|entry| entry.original.as_str()).collect();

        assert_eq!(names, [("リコ", "Rico")]);
        assert_eq!(terms, ["結界"]);
    }

    #[test]
    fn test_suffixed_names_are_rejected_and_bare_name_substitutes() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        store.record_votes(&[
            name("バーサーム家", "Barsam"),
            name("田中一家", "Tanaka"),
            name("メインティア王", "Maindia"),
            name("ミーク姫", "Meeque"),
            name("大上王", "Daijou"),
            name("そこもと", "Sokomoto"),
            name("宮城", "Miyagi"),
            name("シュン太郎", "Shuntaro"),
            name("バーサーム", "Barsam"),
        ]);

        let mut kept: Vec<_> = store.names().map(|(original, _)| original).collect();
        kept.sort_unstable();
        assert_eq!(kept, ["シュン太郎", "バーサーム", "宮城"]);
        assert_eq!(store.apply_to_text("バーサーム家の当主"), "Barsam家の当主");
    }
}
