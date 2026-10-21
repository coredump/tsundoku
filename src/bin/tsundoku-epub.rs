//! `tsundoku-epub`: build EPUB volumes from a Tsundoku novel folder.

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser};
use clap_complete::{Shell, generate};
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;
use tsundoku::Console;
use tsundoku::book::BookMetadata;
use tsundoku::epub::{BookInfo, apply_chapter_titles, collect_chapters, plan_volumes, write_volume};

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

    /// Generate shell completions to stdout.
    #[arg(long, value_enum)]
    generate_completions: Option<Shell>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(shell) = args.generate_completions {
        generate(shell, &mut Args::command(), "tsundoku-epub", &mut io::stdout());
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

    let output_dir = args.output.unwrap_or_else(|| novel_dir.join("EPUB"));
    std::fs::create_dir_all(&output_dir).with_context(|| {
        format!("Failed to create output folder: {}", output_dir.display())
    })?;

    let console = Console::new();
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
    }
    Ok(())
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
