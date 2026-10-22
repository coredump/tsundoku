//! Per-chapter story summaries written by the mapping scout.
//!
//! Each chapter gets a short English synopsis built from the scout's chunk summaries.
//! The translator shows the synopses of the preceding chapters as context. They are
//! stored next to the name mapping as `"{module}: {id}.summaries.json"`:
//!
//! ```json
//! { "version": 1, "chapters": { "12": "Tatsuya brings Ruri to the village." } }
//! ```

use crate::error::SummaryError;
use crate::name_mapping::novel_file_path;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Version written to and accepted from the summary file.
pub const SUMMARIES_VERSION: u32 = 1;

/// Longest chunk summary kept from the scout, in words.
pub const MAX_CHUNK_SUMMARY_WORDS: usize = 40;

/// Longest chapter synopsis, in words.
pub const MAX_SYNOPSIS_WORDS: usize = 120;

/// Collapse whitespace and keep at most `max_words` words, marking a cut with `…`.
pub(crate) fn cap_words(text: &str, max_words: usize) -> String {
    let mut words = text.split_whitespace();
    let mut capped = words.by_ref().take(max_words).collect::<Vec<_>>().join(" ");
    if words.next().is_some() {
        capped.push('…');
    }
    capped
}

/// Join a chapter's chunk summaries, in text order, into one synopsis.
///
/// Blank summaries are skipped and the result is capped at [`MAX_SYNOPSIS_WORDS`]
/// words. Returns `None` when no summary has any text.
pub fn chapter_synopsis<S: AsRef<str>>(chunk_summaries: &[S]) -> Option<String> {
    let joined = chunk_summaries
        .iter()
        .map(|summary| summary.as_ref().trim())
        .filter(|summary| !summary.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let synopsis = cap_words(&joined, MAX_SYNOPSIS_WORDS);
    (!synopsis.is_empty()).then_some(synopsis)
}

/// On-disk summary file contents.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SummaryData {
    /// File format version, [`SUMMARIES_VERSION`].
    version: u32,
    /// Synopsis per chapter number; an empty string marks a chapter with no story.
    chapters: BTreeMap<u32, String>,
}

impl Default for SummaryData {
    fn default() -> Self {
        Self {
            version: SUMMARIES_VERSION,
            chapters: BTreeMap::new(),
        }
    }
}

/// Chapter summaries for one novel.
#[derive(Debug)]
pub struct SummaryStore {
    /// Path to the JSON file.
    filepath: PathBuf,
    /// The summary data.
    data: SummaryData,
}

impl SummaryStore {
    /// Open the summary store for a novel, loading it when the file exists.
    ///
    /// The file sits in `names_dir` next to the name mapping, named
    /// `"{module_name}: {novel_id}.summaries.json"` (`" - "` instead of `": "` on
    /// Windows).
    pub fn new(names_dir: &Path, module_name: &str, novel_id: &str) -> Result<Self, SummaryError> {
        let filepath = novel_file_path(names_dir, module_name, novel_id, ".summaries.json");
        let data = if filepath.exists() {
            let data: SummaryData = serde_json::from_str(&std::fs::read_to_string(&filepath)?)?;
            if data.version != SUMMARIES_VERSION {
                return Err(SummaryError::UnsupportedVersion(data.version));
            }
            data
        } else {
            SummaryData::default()
        };

        Ok(Self { filepath, data })
    }

    /// Path of the summary file.
    pub fn filepath(&self) -> &Path {
        &self.filepath
    }

    /// Returns whether a chapter has been summarized (possibly with an empty synopsis).
    pub fn has(&self, chapter: u32) -> bool {
        self.data.chapters.contains_key(&chapter)
    }

    /// Synopsis of a chapter, if it has a non-empty one.
    pub fn get(&self, chapter: u32) -> Option<&str> {
        self.data
            .chapters
            .get(&chapter)
            .map(String::as_str)
            .filter(|summary| !summary.is_empty())
    }

    /// Record a chapter's synopsis in memory; call [`Self::save`] to persist it.
    ///
    /// Whitespace is trimmed. An empty synopsis still marks the chapter as summarized,
    /// so chapters without story content are not requested again.
    pub fn set(&mut self, chapter: u32, summary: String) {
        let trimmed = summary.trim();
        let summary = if trimmed.len() == summary.len() {
            summary
        } else {
            trimmed.to_string()
        };
        self.data.chapters.insert(chapter, summary);
    }

    /// Synopses of chapters `chapter - k` through `chapter - 1`, oldest first.
    ///
    /// Chapters without a synopsis are skipped, so fewer than `k` entries may be
    /// returned. Chapter 1 and `k = 0` return nothing.
    pub fn previous(&self, chapter: u32, k: usize) -> Vec<(u32, String)> {
        let window = u32::try_from(k).unwrap_or(u32::MAX);
        let first = chapter.saturating_sub(window);
        self.data
            .chapters
            .range(first..chapter)
            .filter(|(_, summary)| !summary.is_empty())
            .map(|(number, summary)| (*number, summary.clone()))
            .collect()
    }

    /// Number of chapters summarized.
    pub fn len(&self) -> usize {
        self.data.chapters.len()
    }

    /// Returns whether no chapter has been summarized.
    pub fn is_empty(&self) -> bool {
        self.data.chapters.is_empty()
    }

    /// Write the summaries to disk, creating the directory if needed.
    pub fn save(&self) -> Result<(), SummaryError> {
        if let Some(parent) = self.filepath.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = serde_json::to_string_pretty(&self.data)?;
        std::fs::write(&self.filepath, content)
            .map_err(|e| SummaryError::WriteError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_store_round_trips_through_disk() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = SummaryStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        assert!(store.is_empty());
        store.set(12, "  Tatsuya brings Ruri to the village. ".to_string());
        store.set(13, String::new());
        store.save().unwrap();

        let content = std::fs::read_to_string(store.filepath()).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["chapters"]["12"], "Tatsuya brings Ruri to the village.");
        assert!(
            store
                .filepath()
                .ends_with(if cfg!(windows) {
                    "syosetu - n1234ab.summaries.json"
                } else {
                    "syosetu: n1234ab.summaries.json"
                })
        );

        let reloaded = SummaryStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        assert_eq!(reloaded.len(), 2);
        assert!(reloaded.has(12) && reloaded.has(13) && !reloaded.has(14));
        assert_eq!(reloaded.get(12), Some("Tatsuya brings Ruri to the village."));
        assert_eq!(reloaded.get(13), None);
    }

    #[test]
    fn test_unsupported_version_is_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let store = SummaryStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        std::fs::write(store.filepath(), r#"{"version":2,"chapters":{}}"#).unwrap();

        assert!(matches!(
            SummaryStore::new(temp_dir.path(), "syosetu", "n1234ab"),
            Err(SummaryError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn test_previous_selects_preceding_chapters_oldest_first() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = SummaryStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        for chapter in 1..=5 {
            store.set(chapter, format!("Chapter {chapter} events."));
        }

        assert!(store.previous(1, 2).is_empty());
        assert!(store.previous(5, 0).is_empty());
        assert_eq!(
            store.previous(5, 2),
            [(3, "Chapter 3 events.".to_string()), (4, "Chapter 4 events.".to_string())]
        );

        store.set(4, String::new());
        assert_eq!(store.previous(5, 2), [(3, "Chapter 3 events.".to_string())]);
        assert_eq!(store.previous(3, usize::MAX).len(), 2);
    }

    #[test]
    fn test_chapter_synopsis_joins_in_order_and_caps_words() {
        assert_eq!(
            chapter_synopsis(&["Tatsuya arrives.", "  ", "Ruri  greets\nhim."]),
            Some("Tatsuya arrives. Ruri greets him.".to_string())
        );
        assert_eq!(chapter_synopsis::<&str>(&[]), None);
        assert_eq!(chapter_synopsis(&[" "]), None);

        let long = vec!["word ".repeat(50); 3];
        let synopsis = chapter_synopsis(&long).unwrap();
        assert_eq!(synopsis.split_whitespace().count(), MAX_SYNOPSIS_WORDS);
        assert!(synopsis.ends_with("word…"));
    }
}
