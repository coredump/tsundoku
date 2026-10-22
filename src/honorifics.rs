//! Honorific policy and the deterministic source-side rewrite.
//!
//! Name substitution turns `ルリちゃん` into `Ruriちゃん`. [`apply_source`] then
//! rewrites the Japanese suffix that follows Latin text, so the model sees
//! `Ruri-chan` (or `Ruri`) and no longer decides per chunk. Suffixes after
//! Japanese text (`村長さん`) are left for the model.

use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

/// How honorifics are rendered in the translation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HonorificPolicy {
    /// Keep honorifics everywhere (`Ruri-chan`).
    #[default]
    Keep,
    /// Drop -san/-chan/-kun/-sama/-dono everywhere.
    Drop,
    /// Keep honorifics on character names only.
    NamesOnly,
}

/// A Latin token (produced by name substitution) followed by a Japanese honorific.
static SOURCE_HONORIFIC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([A-Za-z][A-Za-z'\-]*)(さん|ちゃん|くん|君|様|さま|殿|先輩|先生)")
        .expect("valid honorific regex")
});

/// Romanized form of a Japanese honorific suffix.
fn romanize(suffix: &str) -> &'static str {
    match suffix {
        "さん" => "san",
        "ちゃん" => "chan",
        "くん" | "君" => "kun",
        "様" | "さま" => "sama",
        "殿" => "dono",
        "先輩" => "senpai",
        "先生" => "sensei",
        _ => unreachable!("suffix not in SOURCE_HONORIFIC"),
    }
}

/// Rewrite Japanese honorifics that follow Latin text according to `policy`.
///
/// `keep` and `names-only` produce `Ruri-chan`; `drop` produces `Ruri` but keeps
/// `-senpai`/`-sensei`, which carry meaning the model should translate.
pub fn apply_source(text: &str, policy: HonorificPolicy) -> String {
    SOURCE_HONORIFIC
        .replace_all(text, |caps: &Captures| {
            let name = &caps[1];
            let suffix = &caps[2];
            match (policy, suffix) {
                (HonorificPolicy::Drop, "先輩" | "先生") => format!("{name}{suffix}"),
                (HonorificPolicy::Drop, _) => name.to_string(),
                _ => format!("{name}-{}", romanize(suffix)),
            }
        })
        .into_owned()
}

/// The system-prompt line describing `policy`.
pub fn prompt_line(policy: HonorificPolicy) -> &'static str {
    match policy {
        HonorificPolicy::Keep => {
            "Keep Japanese honorifics as romanized suffixes (-san, -chan, -kun, -sama, -dono, -senpai, -sensei) exactly where the source uses them."
        }
        HonorificPolicy::Drop => {
            "Drop the honorifics -san, -chan, -kun, -sama and -dono; render 先輩 and 先生 by meaning (senior, teacher, doctor) where they matter."
        }
        HonorificPolicy::NamesOnly => {
            "Keep romanized honorifics (-san, -chan, -kun, -sama, -dono, -senpai, -sensei) only on character names. Never attach them to titles, roles, animals or common nouns; translate 村長さん as 'the village chief'."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUFFIXES: [(&str, &str); 9] = [
        ("さん", "san"),
        ("ちゃん", "chan"),
        ("くん", "kun"),
        ("君", "kun"),
        ("様", "sama"),
        ("さま", "sama"),
        ("殿", "dono"),
        ("先輩", "senpai"),
        ("先生", "sensei"),
    ];

    #[test]
    fn test_each_mode_and_suffix() {
        for (jp, en) in SUFFIXES {
            let source = format!("「Ruri{jp}、おはよう」");
            for policy in [HonorificPolicy::Keep, HonorificPolicy::NamesOnly] {
                assert_eq!(
                    apply_source(&source, policy),
                    format!("「Ruri-{en}、おはよう」")
                );
            }
            let dropped = if matches!(jp, "先輩" | "先生") {
                source.clone()
            } else {
                "「Ruri、おはよう」".to_string()
            };
            assert_eq!(apply_source(&source, HonorificPolicy::Drop), dropped);
        }
    }

    #[test]
    fn test_japanese_noun_untouched() {
        for policy in [
            HonorificPolicy::Keep,
            HonorificPolicy::Drop,
            HonorificPolicy::NamesOnly,
        ] {
            assert_eq!(apply_source("村長さんが来た", policy), "村長さんが来た");
        }
    }

    #[test]
    fn test_kun_kanji() {
        assert_eq!(
            apply_source("Tatsuya君", HonorificPolicy::Keep),
            "Tatsuya-kun"
        );
        assert_eq!(apply_source("Tatsuya君", HonorificPolicy::Drop), "Tatsuya");
    }

    #[test]
    fn test_policy_serde_kebab_case() {
        let policy: HonorificPolicy = serde_json::from_str("\"names-only\"").unwrap();
        assert_eq!(policy, HonorificPolicy::NamesOnly);
        assert_eq!(
            serde_json::to_string(&HonorificPolicy::Drop).unwrap(),
            "\"drop\""
        );
    }
}
