//! Deterministic post-processing of translated text.
//!
//! [`postprocess`] always runs on the model's raw output, so re-applying the
//! rules after a `book.json` edit never compounds.

use crate::book::Replacement;
use crate::error::ReplacementError;
use crate::honorifics::HonorificPolicy;
use crate::name_mapping::Gender;
use regex::{Captures, Regex};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// A word followed by a romanized honorific, e.g. `cat-san` or `Chief-san`.
///
/// Group 1 is the word, group 2 the suffix.
pub static ENGLISH_HONORIFIC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b([A-Za-z][A-Za-z']*)-(san|chan|kun|sama|dono|senpai|sensei)\b")
        .expect("valid honorific regex")
});

/// A compiled `book.json` replacement.
#[derive(Debug, Clone)]
pub(crate) struct CompiledReplacement {
    pattern: Regex,
    replace: String,
}

/// Everything [`postprocess`] needs for one novel.
///
/// The default rules keep honorifics, have no replacements and do not know the
/// novel's names (as [`OutputRules::without_known_names`]).
#[derive(Debug, Clone, Default)]
pub struct OutputRules {
    policy: HonorificPolicy,
    /// Known English name words (each name and each of its parts); `None` when
    /// the novel's names are unknown, so every capitalised word counts as a name.
    known_names: Option<HashSet<String>>,
    /// Known multi-word or hyphenated names followed by -sama/-dono, for the
    /// localizing policies (`Monika Weber-sama` → `Lady Monika Weber`, not
    /// `Monika Lady Weber`).
    titled_full_name: Option<Regex>,
    /// Gender of each known name and name part, for English titles.
    genders: HashMap<String, Gender>,
    replacements: Vec<CompiledReplacement>,
}

impl OutputRules {
    /// Build rules from the policy, known English names (full names; their
    /// space/hyphen-separated parts are also recognised) and ordered replacements.
    pub fn new<I, S>(
        policy: HonorificPolicy,
        known_names: I,
        replacements: &[Replacement],
    ) -> Result<Self, ReplacementError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut names = HashSet::new();
        for name in known_names {
            let name = name.as_ref().trim();
            if name.is_empty() {
                continue;
            }
            names.insert(name.to_string());
            names.extend(
                name.split([' ', '-'])
                    .filter(|part| !part.is_empty())
                    .map(str::to_string),
            );
        }
        let titled_full_name = if policy.localizes_titles() {
            let mut full: Vec<&String> = names
                .iter()
                .filter(|name| name.contains([' ', '-']))
                .collect();
            full.sort_unstable_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
            (!full.is_empty()).then(|| {
                let alternatives: Vec<String> =
                    full.iter().map(|name| regex::escape(name)).collect();
                Regex::new(&format!(r"\b({})-(sama|dono)\b", alternatives.join("|")))
                    .expect("escaped names form a valid regex")
            })
        } else {
            None
        };
        Ok(Self {
            policy,
            known_names: Some(names),
            titled_full_name,
            genders: HashMap::new(),
            replacements: compile_replacements(replacements)?,
        })
    }

    /// Add the genders of known names (selected English renderings), used by the
    /// localizing policies to choose `Lord`/`Sir` or `Lady`.
    ///
    /// A multi-word name's gender also applies to its parts. A name or part given
    /// conflicting genders gets none; `unknown` is ignored.
    pub fn with_genders<I, S>(mut self, genders: I) -> Self
    where
        I: IntoIterator<Item = (S, Gender)>,
        S: AsRef<str>,
    {
        let mut seen: HashMap<String, Option<Gender>> = HashMap::new();
        let mut record = |name: &str, gender: Gender| {
            seen.entry(name.to_string())
                .and_modify(|known| {
                    if *known != Some(gender) {
                        *known = None;
                    }
                })
                .or_insert(Some(gender));
        };
        for (name, gender) in genders {
            let name = name.as_ref().trim();
            if name.is_empty() || gender == Gender::Unknown {
                continue;
            }
            record(name, gender);
            if name.contains([' ', '-']) {
                for part in name.split([' ', '-']).filter(|part| !part.is_empty()) {
                    record(part, gender);
                }
            }
        }
        self.genders = seen
            .into_iter()
            .filter_map(|(name, gender)| gender.map(|gender| (name, gender)))
            .collect();
        self
    }

    /// Build rules for a novel whose names are unknown: every capitalised word
    /// counts as a name, so `names-only` strips honorifics after lowercase words
    /// only (`cat-san`, but not `Village Chief-san`).
    pub fn without_known_names(
        policy: HonorificPolicy,
        replacements: &[Replacement],
    ) -> Result<Self, ReplacementError> {
        Ok(Self {
            policy,
            known_names: None,
            titled_full_name: None,
            genders: HashMap::new(),
            replacements: compile_replacements(replacements)?,
        })
    }

    /// The honorific policy these rules apply.
    pub fn policy(&self) -> HonorificPolicy {
        self.policy
    }

    /// Whether `word` (the token before an honorific) is a character name.
    fn is_name(&self, word: &str) -> bool {
        word.starts_with(|c: char| c.is_ascii_uppercase())
            && self
                .known_names
                .as_ref()
                .is_none_or(|names| names.contains(word))
    }

    /// English title for a character name followed by -sama/-dono, by gender.
    fn title_for(&self, name: &str, suffix: &str) -> Option<&'static str> {
        match (suffix, self.genders.get(name)?) {
            ("sama", Gender::Male) => Some("Lord"),
            ("dono", Gender::Male) => Some("Sir"),
            ("sama" | "dono", Gender::Female) => Some("Lady"),
            _ => None,
        }
    }

    /// Render `word` + `suffix` under a localizing policy: -sama/-dono on a name
    /// with a known gender become a title, other -sama/-dono are dropped; -san,
    /// -chan and -kun are dropped under `localize` and kept on names under
    /// `localize-titles`; -senpai/-sensei follow the names-only rule.
    fn localize(&self, matched: &str, word: &str, suffix: &str) -> String {
        match suffix {
            "sama" | "dono" => match self.title_for(word, suffix) {
                Some(title) if self.is_name(word) => format!("{title} {word}"),
                _ => word.to_string(),
            },
            "san" | "chan" | "kun" if self.policy == HonorificPolicy::Localize => word.to_string(),
            _ if self.is_name(word) => matched.to_string(),
            _ => word.to_string(),
        }
    }
}

/// Compile replacement entries, failing with the index of the first invalid one.
///
/// Literal finds are case-sensitive and match whole words at each end that is
/// a word character.
pub(crate) fn compile_replacements(
    replacements: &[Replacement],
) -> Result<Vec<CompiledReplacement>, ReplacementError> {
    replacements
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let source = if entry.regex {
                entry.find.clone()
            } else {
                let is_word = |c: char| c.is_alphanumeric() || c == '_';
                let start = if entry.find.starts_with(is_word) {
                    r"\b"
                } else {
                    ""
                };
                let end = if entry.find.ends_with(is_word) {
                    r"\b"
                } else {
                    ""
                };
                format!("{start}{}{end}", regex::escape(&entry.find))
            };
            let pattern = Regex::new(&source).map_err(|source| ReplacementError {
                index,
                find: entry.find.clone(),
                source,
            })?;
            // Literal replacements must not expand `$` groups.
            let replace = if entry.regex {
                entry.replace.clone()
            } else {
                entry.replace.replace('$', "$$")
            };
            Ok(CompiledReplacement { pattern, replace })
        })
        .collect()
}

/// Apply user replacements in order, then honorific rules, to raw model output.
///
/// Replacements run first so a spelling fix (`Tatyuya` → `Tatsuya`) turns a
/// misspelled name into a known one before `names-only` decides whether its
/// honorific stays, and so a per-name form (`Mei-chan` → `Meimei`,
/// `Linus-sama` → `Master Linus`) wins over the localizing rules.
pub fn postprocess(raw: &str, rules: &OutputRules) -> String {
    let mut text = raw.to_string();
    for replacement in &rules.replacements {
        text = replacement
            .pattern
            .replace_all(&text, replacement.replace.as_str())
            .into_owned();
    }
    match rules.policy {
        HonorificPolicy::Keep => text,
        HonorificPolicy::Drop => ENGLISH_HONORIFIC.replace_all(&text, "$1").into_owned(),
        HonorificPolicy::NamesOnly => ENGLISH_HONORIFIC
            .replace_all(&text, |caps: &Captures| {
                if rules.is_name(&caps[1]) {
                    caps[0].to_string()
                } else {
                    caps[1].to_string()
                }
            })
            .into_owned(),
        HonorificPolicy::Localize | HonorificPolicy::LocalizeTitles => {
            if let Some(full) = &rules.titled_full_name {
                text = full
                    .replace_all(&text, |caps: &Captures| {
                        match rules.title_for(&caps[1], &caps[2]) {
                            Some(title) => format!("{title} {}", &caps[1]),
                            None => caps[1].to_string(),
                        }
                    })
                    .into_owned();
            }
            ENGLISH_HONORIFIC
                .replace_all(&text, |caps: &Captures| {
                    rules.localize(&caps[0], &caps[1], &caps[2])
                })
                .into_owned()
        }
    }
}

/// Honorifics attached to something other than a known name (`cat-san`,
/// `Chief-san`), as they appear in `text`. Uses the names-only strip rule.
pub fn honorific_on_non_name(text: &str, rules: &OutputRules) -> Vec<String> {
    ENGLISH_HONORIFIC
        .captures_iter(text)
        .filter(|caps| !rules.is_name(&caps[1]))
        .map(|caps| caps[0].to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(policy: HonorificPolicy, replacements: &[Replacement]) -> OutputRules {
        OutputRules::new(policy, ["Ruri", "Monika Weber"], replacements).unwrap()
    }

    fn literal(find: &str, replace: &str) -> Replacement {
        Replacement {
            find: find.into(),
            replace: replace.into(),
            regex: false,
        }
    }

    fn pattern(find: &str, replace: &str) -> Replacement {
        Replacement {
            find: find.into(),
            replace: replace.into(),
            regex: true,
        }
    }

    #[test]
    fn test_names_only_strips_non_names() {
        let rules = rules(HonorificPolicy::NamesOnly, &[]);
        assert_eq!(postprocess("the cat-san sat", &rules), "the cat sat");
        assert_eq!(
            postprocess("Village Chief-san said", &rules),
            "Village Chief said"
        );
        assert_eq!(
            postprocess("Ruri-chan and Monika-san", &rules),
            "Ruri-chan and Monika-san"
        );
        assert_eq!(
            honorific_on_non_name("the cat-san met Ruri-chan and Chief-sama", &rules),
            vec!["cat-san", "Chief-sama"]
        );
    }

    #[test]
    fn test_drop_and_keep() {
        let text = "Ruri-chan met the cat-san.";
        assert_eq!(
            postprocess(text, &rules(HonorificPolicy::Drop, &[])),
            "Ruri met the cat."
        );
        assert_eq!(postprocess(text, &rules(HonorificPolicy::Keep, &[])), text);
    }

    #[test]
    fn test_literal_replacement_is_whole_word() {
        let rules = rules(HonorificPolicy::Keep, &[literal("Tatyuya", "Tatsuya$1")]);
        assert_eq!(
            postprocess("Tatyuya and the Tatyuyas", &rules),
            "Tatsuya$1 and the Tatyuyas"
        );
    }

    #[test]
    fn test_regex_replacement_with_captures() {
        let rules = rules(
            HonorificPolicy::Keep,
            &[pattern(
                r"\bChief (Inspector|Auditor)\b",
                "Section Chief ($1)",
            )],
        );
        assert_eq!(
            postprocess("the Chief Inspector came", &rules),
            "the Section Chief (Inspector) came"
        );
    }

    #[test]
    fn test_spelling_fix_runs_before_names_only_strip() {
        let rules = OutputRules::new(
            HonorificPolicy::NamesOnly,
            ["Tatsuya"],
            &[literal("Tatyuya", "Tatsuya")],
        )
        .unwrap();
        assert_eq!(
            postprocess("Tatyuya-san waved at the dog-san.", &rules),
            "Tatsuya-san waved at the dog."
        );
    }

    #[test]
    fn test_replacements_apply_in_order() {
        let forward = rules(
            HonorificPolicy::Keep,
            &[literal("A", "B"), literal("B", "C")],
        );
        let backward = rules(
            HonorificPolicy::Keep,
            &[literal("B", "C"), literal("A", "B")],
        );
        assert_eq!(postprocess("A", &forward), "C");
        assert_eq!(postprocess("A", &backward), "B");
    }

    #[test]
    fn test_invalid_regex_names_entry() {
        let error = OutputRules::new(
            HonorificPolicy::Keep,
            Vec::<String>::new(),
            &[literal("ok", "fine"), pattern("(unclosed", "x")],
        )
        .unwrap_err();
        assert_eq!(error.index, 1);
        assert!(error.to_string().contains("replacements[1]"));
    }

    #[test]
    fn test_unknown_names_strip_only_after_lowercase_words() {
        let rules = OutputRules::without_known_names(
            HonorificPolicy::NamesOnly,
            &[literal("Tatyuya", "Tatsuya")],
        )
        .unwrap();
        assert_eq!(
            postprocess("Tatyuya-kun fed the cat-san for Village Chief-san.", &rules),
            "Tatsuya-kun fed the cat for Village Chief-san."
        );
    }

    fn localize_rules(policy: HonorificPolicy, replacements: &[Replacement]) -> OutputRules {
        OutputRules::new(
            policy,
            ["Isamu", "Annemarie", "Eto", "Monika Weber", "Orihime"],
            replacements,
        )
        .unwrap()
        .with_genders([
            ("Isamu", Gender::Male),
            ("Annemarie", Gender::Female),
            ("Monika Weber", Gender::Female),
            ("Orihime", Gender::Unknown),
        ])
    }

    #[test]
    fn test_localize_titles_names_by_gender() {
        let rules = localize_rules(HonorificPolicy::Localize, &[]);
        assert_eq!(
            postprocess(
                "Isamu-sama and Isamu-dono met Annemarie-sama, Annemarie-dono and Monika Weber-sama.",
                &rules
            ),
            "Lord Isamu and Sir Isamu met Lady Annemarie, Lady Annemarie and Lady Monika Weber."
        );
        // No usable gender, or not a name: the suffix is dropped.
        assert_eq!(
            postprocess("Eto-sama, Orihime-dono and Dragon-sama", &rules),
            "Eto, Orihime and Dragon"
        );
    }

    #[test]
    fn test_localize_drops_familiar_suffixes_but_keeps_senpai_sensei_on_names() {
        let rules = localize_rules(HonorificPolicy::Localize, &[]);
        assert_eq!(
            postprocess(
                "Eto-san, Orihime-chan, Isamu-kun, Orihime-sensei and the cat-sensei",
                &rules
            ),
            "Eto, Orihime, Isamu, Orihime-sensei and the cat"
        );
    }

    #[test]
    fn test_localize_titles_keeps_familiar_suffixes_on_names() {
        let rules = localize_rules(HonorificPolicy::LocalizeTitles, &[]);
        assert_eq!(
            postprocess("Eto-san asked Isamu-sama about the cat-chan.", &rules),
            "Eto-san asked Lord Isamu about the cat."
        );
    }

    #[test]
    fn test_localize_replacements_win_and_rewrites_are_idempotent() {
        let rules = localize_rules(
            HonorificPolicy::Localize,
            &[literal("Orihime-sensei", "Professor Orihime")],
        );
        let raw = "Orihime-sensei sat by Isamu-sama.";
        let first = postprocess(raw, &rules);
        assert_eq!(first, "Professor Orihime sat by Lord Isamu.");
        assert_eq!(postprocess(raw, &rules), first);
    }

    #[test]
    fn test_conflicting_genders_give_no_title() {
        let rules = OutputRules::new(HonorificPolicy::Localize, ["Kei"], &[])
            .unwrap()
            .with_genders([("Kei", Gender::Male), ("Kei", Gender::Female)]);
        assert_eq!(postprocess("Kei-sama", &rules), "Kei");
    }

    #[test]
    fn test_rewriting_from_raw_is_idempotent() {
        let rules = rules(HonorificPolicy::NamesOnly, &[literal("Ruri", "Ruri-chan")]);
        let raw = "Ruri waved at the cat-san.";
        let first = postprocess(raw, &rules);
        assert_eq!(first, "Ruri-chan waved at the cat.");
        assert_eq!(postprocess(raw, &rules), first);
    }
}
