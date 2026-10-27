//! `tsundoku-epub`: build EPUB volumes from a Tsundoku novel folder.

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser};
use clap_complete::{Shell, generate};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tokio::runtime::Runtime;
use tsundoku::Console;
use tsundoku::book::BookMetadata;
use tsundoku::chapter_record::ChapterRecord;
use tsundoku::config::Config;
use tsundoku::epub::{
    BookInfo, ChapterFile, Volume, apply_chapter_titles, collect_chapters, plan_volumes,
    write_volume,
};
use tsundoku::name_mapping::{Gender, NameMappingStore, novel_file_path};
use tsundoku::output_rules::{OutputRules, postprocess};
use tsundoku::qa::IssueKind;

/// Build EPUB volumes from the translated chapters in a Tsundoku novel folder.
#[derive(Parser, Debug)]
#[command(name = "tsundoku-epub", version, about)]
struct Args {
    /// Novel folder created by tsundoku (contains `NNNN - Title.txt` translations).
    #[arg(required_unless_present = "generate_completions")]
    novel_dir: Option<PathBuf>,

    /// Number of chapters per EPUB volume.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..))]
    chapters_per_volume: u32,

    /// Book title (defaults to `title` in book.json, then the folder name without
    /// its `[site: id]` prefix).
    #[arg(long)]
    title: Option<String>,

    /// Author name for the EPUB metadata (defaults to `author` in book.json).
    #[arg(long)]
    author: Option<String>,

    /// Output directory (defaults to `<novel_dir>/EPUB`).
    #[arg(long, short)]
    output: Option<PathBuf>,

    /// Before building, rewrite every chapter `.txt` from the raw translation in
    /// its chunk record, with the honorific policy and book.json replacements
    /// applied. A chapter without a record first gets one holding its current text.
    #[arg(long)]
    rewrite_text: bool,

    /// Generate shell completions to stdout.
    #[arg(long, value_enum)]
    generate_completions: Option<Shell>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(shell) = args.generate_completions {
        generate(
            shell,
            &mut Args::command(),
            "tsundoku-epub",
            &mut io::stdout(),
        );
        return Ok(());
    }

    let novel_dir = args
        .novel_dir
        .context("Novel folder is required unless generating completions")?;
    let novel_dir = novel_dir
        .canonicalize()
        .with_context(|| format!("Novel folder not found: {}", novel_dir.display()))?;
    let folder_name = novel_dir
        .file_name()
        .context("Novel folder has no name")?
        .to_string_lossy();

    let metadata = BookMetadata::load(&novel_dir)?.unwrap_or_default();
    let mut book = BookInfo::from_folder_name(&folder_name).with_metadata(&metadata);
    if let Some(title) = args.title {
        book.title = title;
    }
    if let Some(author) = args.author {
        book.author = Some(author);
    }

    let mut chapters = collect_chapters(&novel_dir)
        .with_context(|| format!("Failed to read novel folder: {}", novel_dir.display()))?;
    apply_chapter_titles(&mut chapters, &metadata);
    if chapters.is_empty() {
        bail!(
            "No translated chapters (`NNNN - Title.txt`) found in {}",
            novel_dir.display()
        );
    }

    let console = Console::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("Failed to start the async runtime")?;
    if args.rewrite_text {
        let rules = rewrite_rules(&console, &folder_name, &metadata)?;
        let summary = runtime.block_on(rewrite_chapter_texts(&novel_dir, &chapters, &rules))?;
        console.success(&summary.describe());
    }

    let output_dir = args.output.unwrap_or_else(|| novel_dir.join("EPUB"));
    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("Failed to create output folder: {}", output_dir.display()))?;

    console.section(&format!("Building EPUB volumes for {}", book.title));
    let modified = SystemTime::now();
    for volume in plan_volumes(chapters, args.chapters_per_volume) {
        let path = output_dir.join(book.volume_file_name(&volume));
        write_volume(&path, &book, &volume, modified)
            .with_context(|| format!("Failed to write {}", path.display()))?;
        console.success(&format!(
            "Vol. {:02}: {} chapters → {}",
            volume.number,
            volume.chapters.len(),
            path.display()
        ));
        let missing = volume.missing();
        if !missing.is_empty() {
            console.warning(&format!(
                "Vol. {:02} is missing chapters {}",
                volume.number,
                format_ranges(&missing)
            ));
        }
        report_volume_qa(&console, &runtime, &novel_dir, &volume);
    }
    Ok(())
}

/// Output rules for `--rewrite-text`: the policy from book.json, else the config's
/// `[translation] honorifics`; the novel's known English names and their genders;
/// the book.json replacements. Without the novel's names file, every capitalised
/// word counts as a name, so `names-only` strips honorifics after lowercase words
/// only.
fn rewrite_rules(
    console: &Console,
    folder_name: &str,
    metadata: &BookMetadata,
) -> Result<OutputRules> {
    let config = load_config(console)?;
    let names = match novel_key(folder_name) {
        Some((module, novel_id)) => {
            let names_dir = config.names_dir()?;
            let names = known_names(&names_dir, module, novel_id)?;
            if names.is_none() {
                console.warning(&format!(
                    "No name mapping for [{module}: {novel_id}] in {}; honorifics are kept on every capitalised word",
                    names_dir.display()
                ));
            }
            names
        }
        None => {
            console.warning(
                "Folder name has no `[module: id]` prefix, so the novel's names are unknown; honorifics are kept on every capitalised word",
            );
            None
        }
    };
    let (renderings, genders) = names.unzip();
    let rules = metadata
        .output_rules(config.translation.honorifics, renderings.as_deref())
        .context("Invalid book.json replacements")?;
    Ok(rules.with_genders(genders.unwrap_or_default()))
}

/// The user's config file, or the defaults when there is none (never creates one).
fn load_config(console: &Console) -> Result<Config> {
    let path = Config::config_path()?;
    if !path.exists() {
        return Ok(Config::default());
    }
    let config = Config::load_from(&path)
        .with_context(|| format!("Failed to load configuration from {}", path.display()))?;
    for key in &config.ignored_keys {
        console.warning(&format!(
            "Unknown config key `{key}` is ignored; check its spelling and section (keys are case-sensitive)"
        ));
    }
    Ok(config)
}

/// `(module, novel id)` from a folder named `[module: id] Title`, the format
/// `tsundoku` creates.
fn novel_key(folder_name: &str) -> Option<(&str, &str)> {
    let (key, _) = folder_name.strip_prefix('[')?.split_once(']')?;
    let (module, novel_id) = key.split_once(": ")?;
    (!module.is_empty() && !novel_id.is_empty()).then_some((module, novel_id))
}

/// Selected English renderings of the novel's names and the genders stated for
/// them, from its mapping file in `names_dir`; `None` when there is no mapping
/// file.
fn known_names(
    names_dir: &Path,
    module: &str,
    novel_id: &str,
) -> Result<Option<(Vec<String>, Vec<(String, Gender)>)>> {
    let path = novel_file_path(names_dir, module, novel_id, ".json");
    if !path.exists() {
        return Ok(None);
    }
    let store = NameMappingStore::new(names_dir, module, novel_id)
        .with_context(|| format!("Failed to load the name mapping {}", path.display()))?;
    let renderings = store
        .known_mappings()
        .names
        .into_iter()
        .map(|entry| entry.english)
        .collect();
    Ok(Some((renderings, store.name_genders())))
}

/// What `--rewrite-text` did.
#[derive(Debug, Default, PartialEq)]
struct RewriteSummary {
    /// Chapters whose text was regenerated from their record.
    rewritten: usize,
    /// Records created from the current text of chapters that had none.
    snapshots: usize,
    /// Chapter files whose content changed.
    changed: usize,
}

impl RewriteSummary {
    fn describe(&self) -> String {
        format!(
            "Rewrote {} chapter(s) from their records: {} file(s) changed, {} record(s) created from existing text",
            self.rewritten, self.changed, self.snapshots
        )
    }
}

/// Rewrite each chapter's `.txt` as `postprocess(record.raw_text())`.
///
/// A chapter without a record was translated before records existed: its current
/// text is first saved as a legacy record (unknown source, no issues), so every
/// later run starts from the same raw text and the rules never compound. Files
/// whose text is unchanged are not written.
async fn rewrite_chapter_texts(
    novel_dir: &Path,
    chapters: &[ChapterFile],
    rules: &OutputRules,
) -> Result<RewriteSummary> {
    let mut summary = RewriteSummary::default();
    for chapter in chapters {
        let number = chapter.number;
        let current = tokio::fs::read_to_string(&chapter.path)
            .await
            .with_context(|| format!("Failed to read {}", chapter.path.display()))?;
        let record = match ChapterRecord::load(novel_dir, number)
            .await
            .with_context(|| format!("Failed to load the record of chapter {number}"))?
        {
            Some(record) => record,
            None => {
                let record = ChapterRecord::legacy(
                    number,
                    chapter.title.clone(),
                    current.clone(),
                    Vec::new(),
                );
                record
                    .save(novel_dir)
                    .await
                    .with_context(|| format!("Failed to save the record of chapter {number}"))?;
                summary.snapshots += 1;
                record
            }
        };
        let text = postprocess(&record.raw_text(), rules);
        if text != current {
            tokio::fs::write(&chapter.path, &text)
                .await
                .with_context(|| format!("Failed to write {}", chapter.path.display()))?;
            summary.changed += 1;
        }
        summary.rewritten += 1;
    }
    Ok(summary)
}

/// Print the QA issue counts recorded in the chapter records of `volume`.
///
/// Unreadable records are reported and skipped; they never stop the build.
fn report_volume_qa(console: &Console, runtime: &Runtime, novel_dir: &Path, volume: &Volume) {
    let mut counts: BTreeMap<IssueKind, usize> = BTreeMap::new();
    let mut recorded = 0;
    for chapter in &volume.chapters {
        match runtime.block_on(ChapterRecord::load(novel_dir, chapter.number)) {
            Ok(Some(record)) => {
                recorded += 1;
                for (kind, count) in record.issue_counts() {
                    *counts.entry(kind).or_default() += count;
                }
            }
            Ok(None) => {}
            Err(error) => console.warning(&format!("Chapter {}: {error}", chapter.number)),
        }
    }
    let unrecorded = volume.chapters.len() - recorded;
    if let Some(summary) = volume_qa_summary(volume.number, &counts, recorded, unrecorded) {
        console.info(&summary);
    }
}

/// One-line QA summary of a volume, or `None` when no chapter has a record.
fn volume_qa_summary(
    volume: u32,
    counts: &BTreeMap<IssueKind, usize>,
    recorded: usize,
    unrecorded: usize,
) -> Option<String> {
    if recorded == 0 {
        return None;
    }
    let issues = if counts.is_empty() {
        "no issues".to_string()
    } else {
        counts
            .iter()
            .map(|(kind, count)| format!("{count} {kind}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut summary = format!("Vol. {volume:02} QA: {issues}");
    if unrecorded > 0 {
        summary.push_str(&format!(
            " ({unrecorded} chapter(s) without a record; `tsundoku --recheck` records them)"
        ));
    }
    Some(summary)
}

/// Format sorted numbers as compact ranges, e.g. `3, 5-9, 12`.
fn format_ranges(numbers: &[u32]) -> String {
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    for &number in numbers {
        match ranges.last_mut() {
            Some((_, end)) if *end + 1 == number => *end = number,
            _ => ranges.push((number, number)),
        }
    }
    ranges
        .iter()
        .map(|&(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tsundoku::book::Replacement;
    use tsundoku::chapter_record::{CHAPTER_RECORD_VERSION, ChunkRecord};
    use tsundoku::honorifics::HonorificPolicy;
    use tsundoku::name_mapping::NameEntry;

    fn write_chapter(dir: &Path, file_name: &str, text: &str) {
        std::fs::write(dir.join(file_name), text).unwrap();
    }

    fn chapters(dir: &Path) -> Vec<ChapterFile> {
        collect_chapters(dir).unwrap()
    }

    #[tokio::test]
    async fn rewrite_regenerates_text_from_record_raw() {
        let dir = TempDir::new().unwrap();
        write_chapter(dir.path(), "0001 - Chapter 1: Cats.txt", "Edited by hand.");
        let record = ChapterRecord {
            version: CHAPTER_RECORD_VERSION,
            chapter: 1,
            title: "Chapter 1: Cats".to_string(),
            chunks: ["Ruri fed the cat-san.", "Village Chief-san waved."]
                .into_iter()
                .map(|raw| ChunkRecord {
                    source: Some("原文".to_string()),
                    raw: raw.to_string(),
                    issues: Vec::new(),
                })
                .collect(),
        };
        record.save(dir.path()).await.unwrap();
        let rules = OutputRules::new(HonorificPolicy::NamesOnly, ["Ruri"], &[]).unwrap();

        let summary = rewrite_chapter_texts(dir.path(), &chapters(dir.path()), &rules)
            .await
            .unwrap();

        assert_eq!(
            summary,
            RewriteSummary {
                rewritten: 1,
                snapshots: 0,
                changed: 1,
            }
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("0001 - Chapter 1: Cats.txt")).unwrap(),
            "Ruri fed the cat.\n\nVillage Chief waved."
        );
        assert_eq!(
            ChapterRecord::load(dir.path(), 1).await.unwrap(),
            Some(record)
        );
    }

    #[tokio::test]
    async fn rewrite_snapshots_chapters_without_a_record_once_and_never_compounds() {
        let dir = TempDir::new().unwrap();
        write_chapter(dir.path(), "0002 - Chapter 2: Waves.txt", "Ruri waved.");
        // Records and stray text files under .tsundoku/ are not chapters.
        std::fs::create_dir_all(dir.path().join(".tsundoku")).unwrap();
        write_chapter(&dir.path().join(".tsundoku"), "0009 - Stray.txt", "Ruri.");
        let book = BookMetadata {
            replacements: vec![Replacement {
                find: "Ruri".to_string(),
                replace: "Ruri-chan".to_string(),
                regex: false,
            }],
            ..BookMetadata::default()
        };
        let rules = book.output_rules(HonorificPolicy::Keep, None).unwrap();
        let path = dir.path().join("0002 - Chapter 2: Waves.txt");

        let first = rewrite_chapter_texts(dir.path(), &chapters(dir.path()), &rules)
            .await
            .unwrap();
        assert_eq!(
            first,
            RewriteSummary {
                rewritten: 1,
                snapshots: 1,
                changed: 1,
            }
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "Ruri-chan waved.");
        let record = ChapterRecord::load(dir.path(), 2).await.unwrap().unwrap();
        assert!(record.is_legacy());
        assert_eq!(record.title, "Chapter 2: Waves");
        assert_eq!(record.raw_text(), "Ruri waved.");

        let second = rewrite_chapter_texts(dir.path(), &chapters(dir.path()), &rules)
            .await
            .unwrap();
        assert_eq!(
            second,
            RewriteSummary {
                rewritten: 1,
                snapshots: 0,
                changed: 0,
            }
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "Ruri-chan waved.");
        assert_eq!(
            ChapterRecord::load(dir.path(), 2).await.unwrap(),
            Some(record)
        );
        assert_eq!(chapters(dir.path()).len(), 1);
        assert_eq!(ChapterRecord::load(dir.path(), 9).await.unwrap(), None);
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".tsundoku/0009 - Stray.txt")).unwrap(),
            "Ruri."
        );
    }

    #[test]
    fn novel_key_reads_the_folder_prefix() {
        assert_eq!(
            novel_key("[syosetu: n1234ab] Some Novel"),
            Some(("syosetu", "n1234ab"))
        );
        assert_eq!(novel_key("[n1234ab] Old Format"), None);
        assert_eq!(novel_key("Plain Folder"), None);
    }

    #[test]
    fn known_names_come_from_the_mapping_file_when_present() {
        let dir = TempDir::new().unwrap();
        assert_eq!(known_names(dir.path(), "syosetu", "n1").unwrap(), None);

        let mut store = NameMappingStore::new(dir.path(), "syosetu", "n1").unwrap();
        // A gender is selected from two votes.
        let ruri = NameEntry {
            original: "ルリ".to_string(),
            english: "Ruri".to_string(),
            gender: Some(Gender::Female),
            ..NameEntry::default()
        };
        store.record_votes(&[ruri.clone()]);
        store.record_votes(&[ruri]);
        store.save().unwrap();

        assert_eq!(
            known_names(dir.path(), "syosetu", "n1").unwrap(),
            Some((
                vec!["Ruri".to_string()],
                vec![("Ruri".to_string(), Gender::Female)]
            ))
        );
    }

    #[test]
    fn volume_qa_summary_lists_counts_and_unrecorded_chapters() {
        let counts = BTreeMap::from([
            (IssueKind::NameNearMiss, 2),
            (IssueKind::GlossaryMissing, 3),
        ]);

        assert_eq!(
            volume_qa_summary(1, &counts, 10, 0).as_deref(),
            Some("Vol. 01 QA: 3 glossary_missing, 2 name_near_miss")
        );
        assert_eq!(
            volume_qa_summary(2, &BTreeMap::new(), 4, 1).as_deref(),
            Some(
                "Vol. 02 QA: no issues (1 chapter(s) without a record; `tsundoku --recheck` records them)"
            )
        );
        assert_eq!(volume_qa_summary(3, &BTreeMap::new(), 0, 5), None);
    }
}
