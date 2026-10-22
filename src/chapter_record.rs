//! Per-chapter record of translated chunks and their QA issues.
//!
//! Stored as JSON at `story_dir/.tsundoku/chunks/{chapter}.json`. The hidden
//! folder keeps it out of the chapter listing and the EPUB builder. The
//! chapter `.txt` is built from the record's raw outputs, so single chunks can
//! be re-translated and spliced back in.

use crate::error::ChapterRecordError;
use crate::qa::{Issue, IssueClass, IssueKind};
use crate::translator::ChunkResult;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Format version written to new records; other versions fail to load.
pub const CHAPTER_RECORD_VERSION: u32 = 1;

/// Folder, relative to the story folder, holding chapter records.
const RECORDS_DIR: [&str; 2] = [".tsundoku", "chunks"];

/// Every chunk of one translated chapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChapterRecord {
    /// Format version ([`CHAPTER_RECORD_VERSION`]).
    pub version: u32,
    /// Chapter number.
    pub chapter: u32,
    /// Translated chapter title.
    pub title: String,
    /// Chunks in chapter order.
    pub chunks: Vec<ChunkRecord>,
}

/// One translated chunk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChunkRecord {
    /// Exact text sent to the model, or `None` (`null`) for a legacy chapter
    /// translated before records existed, where it is unknown.
    pub source: Option<String>,
    /// The model's output, before any post-processing.
    pub raw: String,
    /// Problems found in this chunk.
    pub issues: Vec<Issue>,
}

impl From<ChunkResult> for ChunkRecord {
    fn from(result: ChunkResult) -> Self {
        Self {
            source: Some(result.source),
            raw: result.output,
            issues: result.issues,
        }
    }
}

/// How a record compares with the chunks the current source would produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Staleness {
    /// Every chunk's source is unchanged.
    Fresh,
    /// These chunk indices have a different source.
    Chunks(Vec<usize>),
    /// The chunk count differs, so the whole chapter is stale.
    Whole,
    /// The record has no sources to compare (legacy chapter).
    Legacy,
}

/// Version-only view, read before the full record so an unknown version is
/// reported as such rather than as a parse error.
#[derive(Deserialize)]
struct VersionProbe {
    version: u32,
}

impl ChapterRecord {
    /// Path of the record for `chapter` in `story_dir`.
    pub fn path(story_dir: &Path, chapter: u32) -> PathBuf {
        let mut path = story_dir.to_path_buf();
        path.extend(RECORDS_DIR);
        path.join(format!("{chapter}.json"))
    }

    /// Load the record for `chapter`, or `None` if it has none.
    pub async fn load(story_dir: &Path, chapter: u32) -> Result<Option<Self>, ChapterRecordError> {
        let path = Self::path(story_dir, chapter);
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(ChapterRecordError::Io { path, source }),
        };
        let probe: VersionProbe = match serde_json::from_str(&content) {
            Ok(probe) => probe,
            Err(source) => return Err(ChapterRecordError::Json { path, source }),
        };
        if probe.version != CHAPTER_RECORD_VERSION {
            return Err(ChapterRecordError::UnsupportedVersion {
                path,
                version: probe.version,
            });
        }
        serde_json::from_str(&content)
            .map(Some)
            .map_err(|source| ChapterRecordError::Json { path, source })
    }

    /// Write the record, creating its folder and replacing any previous file
    /// atomically.
    pub async fn save(&self, story_dir: &Path) -> Result<(), ChapterRecordError> {
        let path = Self::path(story_dir, self.chapter);
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir)
                .await
                .map_err(|source| ChapterRecordError::Io {
                    path: dir.to_path_buf(),
                    source,
                })?;
        }
        let content = match serde_json::to_string_pretty(self) {
            Ok(content) => content,
            Err(source) => return Err(ChapterRecordError::Json { path, source }),
        };
        let partial = path.with_extension("json.part");
        if let Err(source) = tokio::fs::write(&partial, content).await {
            return Err(ChapterRecordError::Io {
                path: partial,
                source,
            });
        }
        tokio::fs::rename(&partial, &path)
            .await
            .map_err(|source| ChapterRecordError::Io { path, source })
    }

    /// A record for a chapter translated before records existed: one chunk
    /// holding the whole text, with an unknown source.
    pub fn legacy(chapter: u32, title: String, text: String, issues: Vec<Issue>) -> Self {
        Self {
            version: CHAPTER_RECORD_VERSION,
            chapter,
            title,
            chunks: vec![ChunkRecord {
                source: None,
                raw: text,
                issues,
            }],
        }
    }

    /// Whether any chunk's source is unknown.
    pub fn is_legacy(&self) -> bool {
        self.chunks.iter().any(|chunk| chunk.source.is_none())
    }

    /// The chapter text: raw chunk outputs joined with a blank line.
    pub fn raw_text(&self) -> String {
        self.chunks
            .iter()
            .map(|chunk| chunk.raw.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Indices of chunks with an issue in any of `classes`.
    pub fn chunks_matching(&self, classes: &[IssueClass]) -> Vec<usize> {
        self.chunks
            .iter()
            .enumerate()
            .filter(|(_, chunk)| {
                chunk
                    .issues
                    .iter()
                    .any(|issue| classes.iter().any(|class| class.matches(issue.kind)))
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// Compare recorded sources with the chunks the current source produces.
    ///
    /// Legacy records are never stale: their sources are unknown.
    pub fn staleness(&self, current_sources: &[String]) -> Staleness {
        if self.is_legacy() {
            return Staleness::Legacy;
        }
        if self.chunks.len() != current_sources.len() {
            return Staleness::Whole;
        }
        let changed: Vec<usize> = self
            .chunks
            .iter()
            .zip(current_sources)
            .enumerate()
            .filter(|(_, (chunk, current))| chunk.source.as_deref() != Some(current.as_str()))
            .map(|(index, _)| index)
            .collect();
        if changed.is_empty() {
            Staleness::Fresh
        } else {
            Staleness::Chunks(changed)
        }
    }

    /// Replace the chunks at the given indices; other chunks are kept.
    ///
    /// Indices must be in range.
    pub fn splice(&mut self, replaced: Vec<(usize, ChunkRecord)>) {
        for (index, chunk) in replaced {
            debug_assert!(
                index < self.chunks.len(),
                "splice index {index} out of range"
            );
            if let Some(slot) = self.chunks.get_mut(index) {
                *slot = chunk;
            }
        }
    }

    /// Number of issues of each kind across all chunks.
    pub fn issue_counts(&self) -> BTreeMap<IssueKind, usize> {
        let mut counts = BTreeMap::new();
        for issue in self.chunks.iter().flat_map(|chunk| &chunk.issues) {
            *counts.entry(issue.kind).or_default() += 1;
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn chunk(source: &str, raw: &str, issues: Vec<Issue>) -> ChunkRecord {
        ChunkRecord {
            source: Some(source.to_string()),
            raw: raw.to_string(),
            issues,
        }
    }

    fn issue(kind: IssueKind) -> Issue {
        Issue {
            kind,
            detail: "detail".to_string(),
        }
    }

    fn record() -> ChapterRecord {
        ChapterRecord {
            version: CHAPTER_RECORD_VERSION,
            chapter: 13,
            title: "Chapter 13: The Gate".to_string(),
            chunks: vec![
                chunk("一", "One.", vec![]),
                chunk("二", "Tatyuya came.", vec![issue(IssueKind::NameNearMiss)]),
                chunk("三", "Three.", vec![issue(IssueKind::LengthRatio)]),
            ],
        }
    }

    fn sources(list: &[&str]) -> Vec<String> {
        list.iter().map(|source| source.to_string()).collect()
    }

    #[test]
    fn test_path_is_in_hidden_chunks_folder() {
        assert_eq!(
            ChapterRecord::path(Path::new("/novel"), 13),
            Path::new("/novel/.tsundoku/chunks/13.json")
        );
    }

    #[tokio::test]
    async fn test_round_trip_and_missing_file() {
        let dir = TempDir::new().unwrap();
        assert_eq!(ChapterRecord::load(dir.path(), 13).await.unwrap(), None);

        let record = record();
        record.save(dir.path()).await.unwrap();
        assert_eq!(
            ChapterRecord::load(dir.path(), 13).await.unwrap(),
            Some(record)
        );
        assert!(
            !ChapterRecord::path(dir.path(), 13)
                .with_extension("json.part")
                .exists()
        );
    }

    #[tokio::test]
    async fn test_legacy_source_is_written_as_null_and_read_back() {
        let dir = TempDir::new().unwrap();
        let record = ChapterRecord::legacy(
            2,
            "Chapter 2".to_string(),
            "Old text.".to_string(),
            vec![issue(IssueKind::ParagraphMismatch)],
        );
        record.save(dir.path()).await.unwrap();

        let json = std::fs::read_to_string(ChapterRecord::path(dir.path(), 2)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["chunks"][0]["source"], serde_json::Value::Null);
        assert_eq!(
            value["chunks"][0]["issues"][0]["kind"],
            "paragraph_mismatch"
        );

        let loaded = ChapterRecord::load(dir.path(), 2).await.unwrap().unwrap();
        assert!(loaded.is_legacy());
        assert_eq!(loaded, record);
    }

    #[tokio::test]
    async fn test_unsupported_version_is_an_error() {
        let dir = TempDir::new().unwrap();
        let path = ChapterRecord::path(dir.path(), 4);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"version": 2, "chapter": 4, "future": true}"#).unwrap();

        let error = ChapterRecord::load(dir.path(), 4).await.unwrap_err();
        assert!(
            matches!(
                error,
                ChapterRecordError::UnsupportedVersion { version: 2, .. }
            ),
            "{error:?}"
        );

        std::fs::write(&path, "not json").unwrap();
        let error = ChapterRecord::load(dir.path(), 4).await.unwrap_err();
        assert!(
            matches!(error, ChapterRecordError::Json { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn test_raw_text_joins_chunks_with_blank_line() {
        assert_eq!(record().raw_text(), "One.\n\nTatyuya came.\n\nThree.");
    }

    #[test]
    fn test_splice_replaces_only_given_chunks() {
        let mut record = record();
        record.splice(vec![(1, chunk("二", "Tatsuya came.", vec![]))]);

        let original = self::record();
        assert_eq!(record.chunks[0], original.chunks[0]);
        assert_eq!(record.chunks[1], chunk("二", "Tatsuya came.", vec![]));
        assert_eq!(record.chunks[2], original.chunks[2]);
        assert_eq!(record.raw_text(), "One.\n\nTatsuya came.\n\nThree.");
    }

    #[test]
    fn test_staleness() {
        let record = record();
        assert_eq!(
            record.staleness(&sources(&["一", "二", "三"])),
            Staleness::Fresh
        );
        assert_eq!(
            record.staleness(&sources(&["一", "弐", "参"])),
            Staleness::Chunks(vec![1, 2])
        );
        assert_eq!(record.staleness(&sources(&["一", "二"])), Staleness::Whole);

        let legacy = ChapterRecord::legacy(13, "T".to_string(), "Text".to_string(), vec![]);
        assert_eq!(legacy.staleness(&sources(&["一"])), Staleness::Legacy);
    }

    #[test]
    fn test_chunks_matching_and_issue_counts() {
        let record = record();
        assert_eq!(record.chunks_matching(&[IssueClass::Names]), [1]);
        assert_eq!(
            record.chunks_matching(&[IssueClass::Length, IssueClass::Names]),
            [1, 2]
        );
        assert!(record.chunks_matching(&[IssueClass::Failed]).is_empty());
        assert_eq!(
            record.issue_counts(),
            BTreeMap::from([(IssueKind::NameNearMiss, 1), (IssueKind::LengthRatio, 1)])
        );
    }
}
