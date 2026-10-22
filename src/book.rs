//! Per-novel `book.json` metadata kept next to the translated chapters.
//!
//! `tsundoku` records the novel's source and every translated chapter's exact title
//! here; `tsundoku-epub` reads it so headings and metadata do not depend on file
//! names, which lose characters such as `?` and are length-limited. Fields a person
//! edits (`title`, `author`) are never overwritten once set.

use crate::error::BookMetadataError;
use crate::honorifics::HonorificPolicy;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// File name of the metadata file inside a novel folder.
pub const BOOK_METADATA_FILE: &str = "book.json";

/// Metadata for one novel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookMetadata {
    /// English book title. Filled from the folder name once; edit freely.
    #[serde(default)]
    pub title: String,
    /// Original (Japanese) title from the source site.
    #[serde(default)]
    pub original_title: String,
    /// Author name. Never filled automatically; `null` until set by hand.
    #[serde(default)]
    pub author: Option<String>,
    /// URL of the novel on the source site.
    #[serde(default)]
    pub source_url: String,
    /// Translated chapters by chapter number.
    #[serde(default)]
    pub chapters: BTreeMap<u32, ChapterMetadata>,
    /// Per-novel honorific policy; overrides `[translation] honorifics`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub honorifics: Option<HonorificPolicy>,
    /// Find/replace rules applied in order to translated text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replacements: Vec<Replacement>,
}

/// One find/replace rule from `book.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replacement {
    /// Text to find; a regex when `regex` is true, otherwise a case-sensitive
    /// literal matched as a whole word at each word-character end.
    pub find: String,
    /// Replacement text; `$1`-style groups expand only for regexes.
    pub replace: String,
    /// Treat `find` as a regular expression.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub regex: bool,
}

/// Metadata for one translated chapter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChapterMetadata {
    /// Translated chapter title, exactly as produced (not filename-sanitized).
    pub title: String,
    /// Original (Japanese) chapter title.
    pub original_title: String,
}

impl BookMetadata {
    /// Path of the metadata file in `novel_dir`.
    pub fn path(novel_dir: &Path) -> PathBuf {
        novel_dir.join(BOOK_METADATA_FILE)
    }

    /// Load `book.json` from `novel_dir`, or `None` if the folder has none.
    pub fn load(novel_dir: &Path) -> Result<Option<Self>, BookMetadataError> {
        let path = Self::path(novel_dir);
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(BookMetadataError::Read { path, source }),
        };
        let book: Self =
            serde_json::from_str(&content).map_err(|source| BookMetadataError::Parse {
                path: path.clone(),
                source,
            })?;
        crate::output_rules::compile_replacements(&book.replacements)
            .map_err(|source| BookMetadataError::Replacement { path, source })?;
        Ok(Some(book))
    }

    /// Write `book.json` to `novel_dir`, replacing the previous file atomically.
    pub fn save(&self, novel_dir: &Path) -> Result<(), BookMetadataError> {
        let path = Self::path(novel_dir);
        let partial = path.with_extension("json.part");
        let content =
            serde_json::to_string_pretty(self).map_err(|source| BookMetadataError::Parse {
                path: path.clone(),
                source,
            })?;
        std::fs::write(&partial, content)
            .and_then(|()| std::fs::rename(&partial, &path))
            .map_err(|source| BookMetadataError::Write { path, source })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_missing_file_loads_as_none_and_author_is_written_as_null() {
        let dir = TempDir::new().unwrap();
        assert_eq!(BookMetadata::load(dir.path()).unwrap(), None);

        let mut book = BookMetadata {
            title: "Some Novel".to_string(),
            ..BookMetadata::default()
        };
        book.chapters.insert(
            12,
            ChapterMetadata {
                title: "Chapter 12: Peaceful? Days...".to_string(),
                original_title: "第十二話　平穏な？日々".to_string(),
            },
        );
        book.save(dir.path()).unwrap();

        let raw = std::fs::read_to_string(BookMetadata::path(dir.path())).unwrap();
        assert!(raw.contains("\"author\": null"));
        assert_eq!(BookMetadata::load(dir.path()).unwrap(), Some(book));
    }

    #[test]
    fn test_hand_edited_file_with_author_and_missing_fields_loads() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            BookMetadata::path(dir.path()),
            r#"{ "author": "Someone", "chapters": { "3": { "title": "T", "original_title": "O" } } }"#,
        )
        .unwrap();

        let book = BookMetadata::load(dir.path()).unwrap().unwrap();

        assert_eq!(book.author.as_deref(), Some("Someone"));
        assert_eq!(book.chapters[&3].title, "T");
        assert!(book.title.is_empty());
    }

    #[test]
    fn test_legacy_file_loads_and_saves_without_new_keys() {
        let dir = TempDir::new().unwrap();
        let legacy = r#"{ "title": "T", "original_title": "O", "author": null, "source_url": "u", "chapters": {} }"#;
        std::fs::write(BookMetadata::path(dir.path()), legacy).unwrap();

        let book = BookMetadata::load(dir.path()).unwrap().unwrap();
        assert_eq!(book.honorifics, None);
        assert!(book.replacements.is_empty());

        book.save(dir.path()).unwrap();
        let raw = std::fs::read_to_string(BookMetadata::path(dir.path())).unwrap();
        assert!(!raw.contains("honorifics"));
        assert!(!raw.contains("replacements"));
    }

    #[test]
    fn test_honorifics_and_replacements_load() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            BookMetadata::path(dir.path()),
            r#"{ "honorifics": "names-only", "replacements": [
                { "find": "Tatyuya", "replace": "Tatsuya" },
                { "find": "\\bChief Inspector\\b", "replace": "Section Chief", "regex": true } ] }"#,
        )
        .unwrap();

        let book = BookMetadata::load(dir.path()).unwrap().unwrap();
        assert_eq!(book.honorifics, Some(HonorificPolicy::NamesOnly));
        assert_eq!(book.replacements.len(), 2);
        assert!(!book.replacements[0].regex);
        assert!(book.replacements[1].regex);
    }

    #[test]
    fn test_invalid_regex_is_load_error_naming_entry() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            BookMetadata::path(dir.path()),
            r#"{ "replacements": [ { "find": "a", "replace": "b" }, { "find": "(x", "replace": "y", "regex": true } ] }"#,
        )
        .unwrap();

        let error = BookMetadata::load(dir.path()).unwrap_err();
        assert!(
            matches!(error, BookMetadataError::Replacement { ref source, .. } if source.index == 1)
        );
        assert!(error.to_string().contains("replacements[1]"));
    }
}
