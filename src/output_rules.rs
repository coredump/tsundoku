//! Deterministic post-processing of translated text.
//!
//! [`postprocess`] always runs on the model's raw output, so re-applying the
//! rules after a `book.json` edit never compounds.

use crate::book::Replacement;
use crate::error::ReplacementError;
use crate::honorifics::HonorificPolicy;
use regex::{Captures, Regex};
use std::collections::HashSet;
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
#[derive(Debug, Clone, Default)]
pub struct OutputRules {
    policy: HonorificPolicy,
    /// Known English name words (each name and each of its parts).
    known_names: HashSet<String>,
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
        Ok(Self {
            policy,
            known_names: names,
            replacements: compile_replacements(replacements)?,
        })
    }

    /// Whether `word` (the token before an honorific) is a character name.
    fn is_name(&self, word: &str) -> bool {
        word.starts_with(|c: char| c.is_ascii_uppercase()) && self.known_names.contains(word)
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

/// Apply honorific rules, then user replacements in order, to raw model output.
pub fn postprocess(raw: &str, rules: &OutputRules) -> String {
    let mut text = match rules.policy {
        HonorificPolicy::Keep => raw.to_string(),
        HonorificPolicy::Drop => ENGLISH_HONORIFIC.replace_all(raw, "$1").into_owned(),
        HonorificPolicy::NamesOnly => ENGLISH_HONORIFIC
            .replace_all(raw, |caps: &Captures| {
                if rules.is_name(&caps[1]) {
                    caps[0].to_string()
                } else {
                    caps[1].to_string()
                }
            })
            .into_owned(),
    };
    for replacement in &rules.replacements {
        text = replacement
            .pattern
            .replace_all(&text, replacement.replace.as_str())
            .into_owned();
    }
    text
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
    fn test_rewriting_from_raw_is_idempotent() {
        let rules = rules(HonorificPolicy::NamesOnly, &[literal("Ruri", "Ruri-chan")]);
        let raw = "Ruri waved at the cat-san.";
        let first = postprocess(raw, &rules);
        assert_eq!(first, "Ruri-chan waved at the cat.");
        assert_eq!(postprocess(raw, &rules), first);
    }
}
