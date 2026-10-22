//! Tsundoku CLI - Japanese web novel downloader and translator.

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};
use clap_complete::{Shell, generate};
use futures::{StreamExt, stream};
use std::collections::HashMap;
use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use tokio::sync::{Mutex, mpsc};
use tsundoku::book::{BookMetadata, ChapterMetadata};
use tsundoku::config::Config;
use tsundoku::console::{Console, Style};
use tsundoku::epub::BookInfo;
use tsundoku::name_mapping::{KnownMappings, NameMappingStore};
use tsundoku::name_scout::{
    NameScout, NameScoutEvent, NameScoutProgress, NameScoutStatus, ScoutChunk, ScoutProgressInfo,
    build_chapter_payload,
};
use tsundoku::scrapers::{ChapterInfo, ChapterList, ScraperRegistry};
use tsundoku::translator::{ProgressInfo, TranslationProgress, TranslationStage, Translator};
use tsundoku::utils::{mark_scene_breaks, strip_chapter_number};

const MAX_FILENAME_COMPONENT_BYTES: usize = 255;

/// Japanese web novel downloader and translator.
#[derive(Parser, Debug)]
#[command(name = "tsundoku")]
#[command(author, version, about, long_about = None)]
struct Args {
    /// URL of the novel to download.
    #[arg(required_unless_present = "generate_completions")]
    novel_url: Option<String>,

    /// Generate shell completions to stdout.
    #[arg(long, value_enum)]
    generate_completions: Option<Shell>,

    /// Start downloading from chapter N (1-based).
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    start: Option<u32>,

    /// Stop downloading at chapter N (1-based, inclusive).
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    end: Option<u32>,

    /// Skip manual name mapping review pause.
    #[arg(long)]
    no_name_pause: bool,

    /// Disable name scouting entirely.
    #[arg(long)]
    no_name_scout: bool,

    /// Download or reuse originals and scout mappings without translating.
    #[arg(long, conflicts_with = "no_name_scout")]
    scout_only: bool,

    /// Override number of name scout chapter workers.
    #[arg(long)]
    name_scout_workers: Option<NonZeroUsize>,

    /// Only scout names through chapter N.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    name_scout_limit: Option<u32>,

    /// Override number of story translation chapter workers.
    #[arg(long)]
    translation_workers: Option<NonZeroUsize>,

    /// Enable debug logging for scrapers.
    #[arg(long)]
    debug: bool,
}

/// Downloaded chapter data.
#[allow(dead_code)]
struct ChapterData {
    number: u32,
    title: String,
    content: String,
    filename: String,
}

/// Parameters for processing novels.
struct ProcessParams<'a> {
    console: &'a Console,
    scraper: &'a dyn tsundoku::scrapers::Scraper,
    novel_info: &'a tsundoku::scrapers::NovelInfo,
    output_dir: &'a Path,
    translator: &'a Translator,
    name_scout: &'a NameScout,
    name_mapping: &'a mut NameMappingStore,
    no_name_pause: bool,
    no_name_scout: bool,
    scout_only: bool,
    name_scout_limit: Option<u32>,
    config: &'a Config,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(shell) = args.generate_completions {
        let mut command = Args::command();
        generate(shell, &mut command, "tsundoku", &mut io::stdout());
        return Ok(());
    }

    let novel_url = args
        .novel_url
        .as_deref()
        .context("Novel URL is required unless generating completions")?;
    let console = Console::new();

    console.section("Tsundoku - Web Novel Downloader");

    // Load configuration
    console.step("Loading configuration...");
    let mut config = Config::load().context("Failed to load configuration")?;
    for key in &config.ignored_keys {
        console.warning(&format!(
            "Unknown config key `{key}` is ignored; check its spelling and section (keys are case-sensitive)"
        ));
    }
    config.scraping.debug = args.debug;
    if let Some(workers) = args.name_scout_workers {
        config.name_scout.workers = workers.get();
    }
    if let Some(workers) = args.translation_workers {
        config.translation.workers = workers.get();
    }

    // Check if this is first run (API key not configured)
    if !args.scout_only && !config.api.is_configured() {
        let config_path = Config::config_path()?;
        console.warning(&format!(
            "API key not configured. Please edit: {}",
            config_path.display()
        ));
        console.info("Set your OpenAI-compatible API key in the config file and run again.");
        return Ok(());
    }

    if !args.no_name_scout
        && config
            .scout_api
            .as_ref()
            .filter(|api| api.is_configured())
            .is_none()
    {
        let config_path = Config::config_path()?;
        console.warning(&format!(
            "Name scout API key not configured. Please edit: {}",
            config_path.display()
        ));
        console.info("Set scout_api.key in the config file and run again.");
        return Ok(());
    }

    config
        .validate_for_workflow(!args.scout_only, !args.no_name_scout)
        .context("Invalid configuration")?;
    console.success("Configuration loaded");

    // Find appropriate scraper
    console.step("Finding scraper for URL...");
    let registry = ScraperRegistry::new(&config.scraping);
    let scraper = registry
        .find_for_url(novel_url)
        .ok_or_else(|| anyhow::anyhow!("No scraper found for URL: {}", novel_url))?;

    console.success(&format!("Using {} scraper", scraper.name()));

    // Fetch novel info
    console.step("Fetching novel information...");
    let novel_info = scraper
        .get_novel_info(novel_url)
        .await
        .context("Failed to fetch novel info")?;

    console.success(&format!("Found: {}", novel_info.title));
    console.info(&format!("Novel ID: {}", novel_info.novel_id));

    // Fetch chapter list
    console.step("Fetching chapter list...");
    let chapter_list = scraper
        .get_chapter_list(&novel_info.base_url)
        .await
        .context("Failed to fetch chapter list")?;

    match &chapter_list {
        ChapterList::OneShot => {
            console.success("This is a one-shot story");
        }
        ChapterList::Chapters(chapters) => {
            console.success(&format!("Found {} chapters", chapters.len()));
        }
    }

    // Validate chapter range
    let (start_chapter, end_chapter) =
        validate_chapter_range(args.start, args.end, &chapter_list, &console)?;

    // Initialize name mapping store
    let names_dir = config.names_dir()?;
    let mut name_mapping = NameMappingStore::new(&names_dir, scraper.id(), &novel_info.novel_id)
        .context("Failed to initialize name mapping store")?;

    console.info(&format!(
        "Name mapping: {} names and {} glossary terms loaded",
        name_mapping.len(),
        name_mapping.term_len()
    ));

    // Initialize translator
    let translator = Translator::new(
        config.api.clone(),
        config.translation.clone(),
        config.prompts.title_translation.clone(),
        config.prompts.content_translation.clone(),
    );

    // Initialize name scout
    let scout_api = if args.no_name_scout {
        config.api.clone()
    } else {
        config
            .scout_api_config()
            .context("Name scout API not configured")?
            .clone()
    };
    let name_scout = NameScout::new(
        scout_api,
        config.name_scout.clone(),
        config.prompts.name_scout.clone(),
    );

    // Get output directory
    let output_dir = expand_path(&config.paths.output_directory);

    // Create process params
    let mut params = ProcessParams {
        console: &console,
        scraper,
        novel_info: &novel_info,
        output_dir: &output_dir,
        translator: &translator,
        name_scout: &name_scout,
        name_mapping: &mut name_mapping,
        no_name_pause: args.no_name_pause,
        no_name_scout: args.no_name_scout,
        scout_only: args.scout_only,
        name_scout_limit: args.name_scout_limit,
        config: &config,
    };

    // Process based on chapter type
    if chapter_list.is_oneshot() {
        process_oneshot(&mut params).await?;
    } else if let ChapterList::Chapters(chapters) = &chapter_list {
        process_chapters(&mut params, chapters, start_chapter, end_chapter).await?;
    }

    console.section("Done!");
    Ok(())
}

/// Processes a one-shot story.
async fn process_oneshot(params: &mut ProcessParams<'_>) -> Result<()> {
    params.console.section("Processing One-Shot Story");

    // Find or create story folder
    let folder_name = if params.scout_only {
        find_or_create_scout_folder(
            params.console,
            params.output_dir,
            params.scraper.id(),
            &params.novel_info.novel_id,
            &params.novel_info.title,
        )
    } else {
        find_or_create_folder(
            params.console,
            params.output_dir,
            params.scraper.id(),
            &params.novel_info.novel_id,
            &params.novel_info.title,
            params.translator,
        )
        .await?
    };

    let story_dir = params.output_dir.join(&folder_name);
    std::fs::create_dir_all(&story_dir)?;

    // Download original content if not exists
    let original_path = story_dir.join("original.txt");
    let content = if original_path.exists() {
        params
            .console
            .info("Original content already exists, loading...");
        std::fs::read_to_string(&original_path)?
    } else {
        params.console.step("Downloading original content...");
        let content = params
            .scraper
            .download_chapter(&params.novel_info.base_url)
            .await
            .context("Failed to download content")?;
        std::fs::write(&original_path, &content)?;
        params.console.success(&format!(
            "Saved original ({} chars)",
            content.chars().count()
        ));
        content
    };

    // Run name scout
    let scouted = if params.no_name_scout {
        params.console.info("Name scout disabled; skipping");
        false
    } else {
        run_name_scout(
            params.console,
            params.name_scout,
            params.name_mapping,
            &[(1, &params.novel_info.title, &content)],
            params.config.name_scout.workers,
            params.name_scout_limit,
        )
        .await?
    };

    // Manual review (only if scouting was performed)
    if !params.no_name_pause && scouted {
        manual_name_review(params.console, params.name_mapping, params.config)?;
    }

    if params.scout_only {
        params
            .console
            .success("Mapping scout complete; skipping translation");
        return Ok(());
    }

    // Translate content
    let translated_path = story_dir.join("oneshot.txt");
    if translated_path.exists() {
        params
            .console
            .info("Translation already exists, skipping...");
    } else {
        params.console.step("Translating content...");

        // Apply name mapping, mark blank-line scene breaks, and collect canonical
        // terms for this story.
        let mapped_content = mark_scene_breaks(params.name_mapping.apply_to_text(&content));
        let glossary = params
            .name_mapping
            .glossary_for_text(&mapped_content, params.config.translation.glossary_max_entries);

        let progress = ProgressInfo {
            chapter: 1,
            chunk: 1,
            total_chunks: 1,
            worker: None,
            reporter: None,
        };

        let translated = params
            .translator
            .translate(&mapped_content, false, &glossary, Some(progress))
            .await
            .context("Failed to translate content")?;

        std::fs::write(&translated_path, &translated)?;
        params.console.success("Translation saved");
    }

    Ok(())
}

/// Processes multi-chapter stories.
async fn process_chapters(
    params: &mut ProcessParams<'_>,
    chapters: &[ChapterInfo],
    start_chapter: u32,
    end_chapter: u32,
) -> Result<()> {
    params.console.section("Processing Multi-Chapter Story");

    // Find or create story folder
    let folder_name = if params.scout_only {
        find_or_create_scout_folder(
            params.console,
            params.output_dir,
            params.scraper.id(),
            &params.novel_info.novel_id,
            &params.novel_info.title,
        )
    } else {
        find_or_create_folder(
            params.console,
            params.output_dir,
            params.scraper.id(),
            &params.novel_info.novel_id,
            &params.novel_info.title,
            params.translator,
        )
        .await?
    };

    let story_dir = params.output_dir.join(&folder_name);
    let original_dir = story_dir.join("Original");
    std::fs::create_dir_all(&original_dir)?;
    let mut cached_originals = cached_originals_by_chapter(&original_dir)?;

    // Record the novel's source in book.json; hand-edited title and author are kept.
    let mut book = BookMetadata::load(&story_dir)?.unwrap_or_default();
    if book.title.is_empty() {
        book.title = BookInfo::from_folder_name(&folder_name).title;
    }
    book.original_title.clone_from(&params.novel_info.title);
    book.source_url.clone_from(&params.novel_info.base_url);
    book.save(&story_dir)?;

    // Calculate padding for chapter numbers
    let total_chapters = chapters.len();
    let padding = total_chapters.to_string().len();

    // Download phase
    params.console.section("Download Phase");

    let mut downloaded_chapters: Vec<ChapterData> = Vec::new();
    let mut already_downloaded = Vec::new();

    for chapter in chapters.iter() {
        if chapter.number < start_chapter || chapter.number > end_chapter {
            continue;
        }

        let chapter_num_str = format!("{:0width$}", chapter.number, width = padding);
        let filename = chapter_filename(&chapter_num_str, &chapter.title);
        let original_path = original_dir.join(&filename);
        let content = if original_path.exists() {
            already_downloaded.push(chapter.number);
            std::fs::read_to_string(&original_path)?
        } else if let Some(cached_path) = cached_originals.get(&chapter.number) {
            already_downloaded.push(chapter.number);
            std::fs::read_to_string(cached_path)?
        } else {
            report_chapter_ranges(
                params.console,
                &mut already_downloaded,
                "already downloaded",
            );
            params.console.step(&format!(
                "Downloading chapter {}: {}",
                chapter.number, chapter.title
            ));

            let content = params
                .scraper
                .download_chapter(&chapter.url)
                .await
                .with_context(|| format!("Failed to download chapter {}", chapter.number))?;

            std::fs::write(&original_path, &content)?;
            cached_originals.insert(chapter.number, original_path);
            params
                .console
                .success(&format!("Saved ({} chars)", content.chars().count()));
            content
        };

        downloaded_chapters.push(ChapterData {
            number: chapter.number,
            title: chapter.title.clone(),
            content,
            filename,
        });
    }

    report_chapter_ranges(
        params.console,
        &mut already_downloaded,
        "already downloaded",
    );

    if downloaded_chapters.is_empty() {
        params.console.warning("No chapters downloaded");
        return Ok(());
    }

    // Name scout phase
    let scout_data: Vec<(u32, &str, &str)> = downloaded_chapters
        .iter()
        .map(|c| (c.number, c.title.as_str(), c.content.as_str()))
        .collect();

    let scouted = if params.no_name_scout {
        params.console.info("Name scout disabled; skipping");
        false
    } else {
        run_name_scout(
            params.console,
            params.name_scout,
            params.name_mapping,
            &scout_data,
            params.config.name_scout.workers,
            params.name_scout_limit,
        )
        .await?
    };

    // Manual review (only if scouting was performed)
    if !params.no_name_pause && scouted {
        manual_name_review(params.console, params.name_mapping, params.config)?;
    }

    if params.scout_only {
        params
            .console
            .success("Mapping scout complete; skipping translation");
        return Ok(());
    }

    // Translation phase
    params.console.section("Translation Phase");

    translate_chapters(
        params.console,
        params.translator,
        params.name_mapping,
        &downloaded_chapters,
        &story_dir,
        &mut book,
        padding,
        params.config.translation.workers,
        params.config.translation.glossary_max_entries,
    )
    .await?;

    Ok(())
}

async fn translate_chapters(
    console: &Console,
    translator: &Translator,
    name_mapping: &NameMappingStore,
    downloaded_chapters: &[ChapterData],
    story_dir: &Path,
    book: &mut BookMetadata,
    padding: usize,
    workers: usize,
    glossary_max_entries: usize,
) -> Result<()> {
    console.info(&format!(
        "Translating with {} chapter worker{}",
        workers,
        if workers == 1 { "" } else { "s" }
    ));

    let mut jobs = Vec::new();
    let mut already_translated = Vec::new();

    for chapter_data in downloaded_chapters {
        // Check if translation already exists
        let chapter_num_str = format!("{:0width$}", chapter_data.number, width = padding);
        let pattern = format!("{} - ", chapter_num_str);

        let translation_exists = std::fs::read_dir(&story_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .any(|e| e.file_name().to_string_lossy().starts_with(&pattern));

        if translation_exists {
            already_translated.push(chapter_data.number);
            continue;
        }

        let mapped_content = mark_scene_breaks(name_mapping.apply_to_text(&chapter_data.content));
        let glossary = name_mapping.glossary_for_text(&mapped_content, glossary_max_entries);
        jobs.push(TranslationJob {
            number: chapter_data.number,
            title: chapter_data.title.clone(),
            chapter_num_str,
            mapped_title: name_mapping.apply_to_text(&chapter_data.title),
            mapped_content,
            glossary,
        });
    }

    report_chapter_ranges(
        console,
        &mut already_translated,
        "already translated, skipping",
    );

    if jobs.is_empty() {
        return Ok(());
    }

    let (progress_tx, progress_rx) = mpsc::unbounded_channel();
    let progress_display = tokio::spawn(display_translation_stats(progress_rx, workers));
    let worker_ids = Arc::new(Mutex::new((1..=workers).rev().collect::<Vec<_>>()));

    {
        let translated = stream::iter(jobs)
            .map(|job| {
                let worker_ids = Arc::clone(&worker_ids);
                let progress_tx = progress_tx.clone();

                async move {
                    let worker = worker_ids
                        .lock()
                        .await
                        .pop()
                        .expect("worker id pool cannot be empty");

                    let result = translate_chapter_job(
                        console,
                        translator,
                        job,
                        worker,
                        progress_tx.clone(),
                    )
                    .await;

                    let _ = progress_tx.send(TranslationProgress {
                        worker,
                        chapter: 0,
                        chunk: 0,
                        total_chunks: 0,
                        chars: 0,
                        elapsed_secs: 0.0,
                        stage: TranslationStage::Idle,
                    });

                    worker_ids.lock().await.push(worker);
                    result
                }
            })
            .buffer_unordered(workers);

        futures::pin_mut!(translated);
        while let Some(result) = translated.next().await {
            let chapter = result?;
            let translated_path = story_dir.join(&chapter.filename);
            std::fs::write(&translated_path, &chapter.content)?;
            book.chapters.insert(
                chapter.number,
                ChapterMetadata {
                    title: chapter.title,
                    original_title: chapter.original_title,
                },
            );
            book.save(story_dir)?;
            console.clear_line();
            console.success(&format!("Saved: {}", chapter.filename));
            for miss in name_mapping.near_miss_names(&chapter.content) {
                console.warning(&format!(
                    "Chapter {}: `{}` ({}x) looks like a misspelling of `{}`",
                    chapter.number, miss.found, miss.count, miss.expected
                ));
            }
        }
    }

    drop(progress_tx);
    let _ = progress_display.await;

    Ok(())
}

fn report_chapter_ranges(console: &Console, chapters: &mut Vec<u32>, suffix: &str) {
    if chapters.is_empty() {
        return;
    }

    console.info(&format!(
        "Chapters {} {}",
        format_chapter_ranges(chapters),
        suffix
    ));
    chapters.clear();
}

fn format_chapter_ranges(chapters: &[u32]) -> String {
    let mut chapters = chapters.to_vec();
    chapters.sort_unstable();
    chapters.dedup();

    let mut ranges = Vec::new();
    let mut iter = chapters.into_iter();

    let Some(mut start) = iter.next() else {
        return String::new();
    };
    let mut end = start;

    for chapter in iter {
        if chapter == end + 1 {
            end = chapter;
        } else {
            ranges.push(format_chapter_range(start, end));
            start = chapter;
            end = chapter;
        }
    }

    ranges.push(format_chapter_range(start, end));
    ranges.join(", ")
}

fn format_chapter_range(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
}

async fn translate_chapter_job(
    console: &Console,
    translator: &Translator,
    job: TranslationJob,
    worker: usize,
    progress_tx: mpsc::UnboundedSender<TranslationProgress>,
) -> Result<TranslatedChapter> {
    console.step(&format!(
        "Worker {} translating chapter {}: {}",
        worker, job.number, job.title
    ));

    let title_progress = ProgressInfo {
        chapter: job.number,
        chunk: 0,
        total_chunks: 0,
        worker: Some(worker),
        reporter: Some(progress_tx.clone()),
    };

    // Translate only the title text and write the number ourselves, so every chapter
    // is headed `Chapter N: …` instead of "Chapter Two" or "Episode 5". The file's
    // chapter number is used because authors' own numbering contains typos.
    let title_text = strip_chapter_number(&job.mapped_title);
    let translated_title = if title_text.is_empty() {
        format!("Chapter {}", job.number)
    } else {
        let translated_text = translator
            .translate(title_text, true, &[], Some(title_progress))
            .await
            .with_context(|| format!("Failed to translate title for chapter {}", job.number))?;
        format!("Chapter {}: {}", job.number, translated_text)
    };

    let progress = ProgressInfo {
        chapter: job.number,
        chunk: 1,
        total_chunks: 1, // Will be updated by translator
        worker: Some(worker),
        reporter: Some(progress_tx),
    };

    let translated_content = translator
        .translate(&job.mapped_content, false, &job.glossary, Some(progress))
        .await
        .with_context(|| format!("Failed to translate chapter {}", job.number))?;

    Ok(TranslatedChapter {
        filename: chapter_filename(&job.chapter_num_str, &translated_title),
        number: job.number,
        title: translated_title,
        original_title: job.title,
        content: translated_content,
    })
}

struct TranslationJob {
    number: u32,
    title: String,
    chapter_num_str: String,
    mapped_title: String,
    mapped_content: String,
    glossary: Vec<tsundoku::name_mapping::TermEntry>,
}

struct TranslatedChapter {
    filename: String,
    number: u32,
    title: String,
    original_title: String,
    content: String,
}

#[derive(Debug, Clone)]
struct WorkerStat {
    chapter: u32,
    chunk: u32,
    total_chunks: u32,
    chars: usize,
    elapsed_secs: f64,
    stage: TranslationStage,
}

async fn display_translation_stats(
    mut progress_rx: mpsc::UnboundedReceiver<TranslationProgress>,
    workers: usize,
) {
    let console = Console::new();
    let mut stats: HashMap<usize, WorkerStat> = HashMap::new();
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));

    loop {
        tokio::select! {
            progress = progress_rx.recv() => {
                let Some(progress) = progress else {
                    break;
                };
                stats.insert(progress.worker, WorkerStat {
                    chapter: progress.chapter,
                    chunk: progress.chunk,
                    total_chunks: progress.total_chunks,
                    chars: progress.chars,
                    elapsed_secs: progress.elapsed_secs,
                    stage: progress.stage,
                });
            }
            _ = ticker.tick() => {
                render_translation_stats(&console, &stats, workers);
            }
        }
    }

    render_translation_stats(&console, &stats, workers);
    print!("\n");
    let _ = io::stdout().flush();
}

fn render_translation_stats(console: &Console, stats: &HashMap<usize, WorkerStat>, workers: usize) {
    let mut aggregate = 0.0;
    let mut worker_parts = Vec::new();

    for worker in 1..=workers {
        let label = console.style(&format!("W{}", worker), &[Style::Cyan, Style::Bold]);

        if let Some(stat) = stats.get(&worker) {
            match stat.stage {
                TranslationStage::Idle => {
                    worker_parts.push(format!("{}: {}", label, console.muted("idle")));
                }
                TranslationStage::Title => {
                    worker_parts.push(format!(
                        "{}: {}",
                        label,
                        console.muted(&format!("ch{} title", stat.chapter))
                    ));
                }
                TranslationStage::Preparing => {
                    worker_parts.push(format!(
                        "{}: {}",
                        label,
                        console.muted(&format!(
                            "ch{} preparing {}/{}",
                            stat.chapter, stat.chunk, stat.total_chunks
                        ))
                    ));
                }
                TranslationStage::Content => {
                    let speed = if stat.elapsed_secs > 0.0 {
                        stat.chars as f64 / stat.elapsed_secs
                    } else {
                        0.0
                    };
                    aggregate += speed;
                    worker_parts.push(format!(
                        "{}: ch{} {}/{} {}",
                        label,
                        stat.chapter,
                        stat.chunk,
                        stat.total_chunks,
                        console.style(&format!("{:.0} ch/s", speed), &[Style::Green, Style::Bold])
                    ));
                }
            }
        } else {
            worker_parts.push(format!("{}: {}", label, console.muted("idle")));
        }
    }

    let info = console.label("INFO", Style::Blue);
    let aggregate = console.style(
        &format!("{:.0} ch/s", aggregate),
        &[Style::Green, Style::Bold],
    );

    print!(
        "\r\x1b[2K{} Translating. All: {}, {}",
        info,
        aggregate,
        worker_parts.join(", ")
    );
    let _ = io::stdout().flush();
}

/// Runs the mapping scout on chapters missing either names or terminology coverage.
/// Returns true if any scouting was performed, false if all chapters were covered.
async fn run_name_scout(
    console: &Console,
    name_scout: &NameScout,
    name_mapping: &mut NameMappingStore,
    chapters: &[(u32, &str, &str)], // (number, title, content)
    workers: usize,
    limit: Option<u32>,
) -> Result<bool> {
    console.section("Mapping Scout Phase");

    let uncovered: Vec<_> = chapters
        .iter()
        .filter(|(number, _, _)| limit.is_none_or(|limit| *number <= limit))
        .map(|(number, title, content)| {
            (
                *number,
                *title,
                *content,
                !name_mapping.is_chapter_covered(*number),
                !name_mapping.is_chapter_terms_covered(*number),
            )
        })
        .filter(|(_, _, _, missing_names, missing_terms)| *missing_names || *missing_terms)
        .collect();

    if uncovered.is_empty() {
        console.info("All chapters already scouted for names and terminology");
        return Ok(false);
    }

    console.info(&format!(
        "Scouting {} chapters for character names and terminology with {} chapter worker{}",
        uncovered.len(),
        workers,
        if workers == 1 { "" } else { "s" }
    ));

    let (progress_tx, progress_rx) = mpsc::unbounded_channel();
    let progress_display = tokio::spawn(display_name_scout_stats(progress_rx, workers));
    let worker_ids = Arc::new(Mutex::new((1..=workers).rev().collect::<Vec<_>>()));

    let mut scout_error = None;
    // Workers read the latest selections when they start a chapter, so later chapters
    // reuse renderings voted on by chapters that finished before them.
    let known = RwLock::new(Arc::new(name_mapping.known_mappings()));
    let known = &known;

    {
        let scouted = stream::iter(uncovered)
            .map(|(number, title, content, record_names, record_terms)| {
                let worker_ids = Arc::clone(&worker_ids);
                let progress_tx = progress_tx.clone();

                async move {
                    let worker = worker_ids
                        .lock()
                        .await
                        .pop()
                        .expect("worker id pool cannot be empty");

                    let snapshot = Arc::clone(&known.read().unwrap_or_else(PoisonError::into_inner));
                    let result = scout_chapter_job(
                        name_scout,
                        number,
                        title,
                        content,
                        &snapshot.present_in(content),
                        record_names,
                        record_terms,
                        worker,
                        progress_tx.clone(),
                    )
                    .await;

                    let _ = progress_tx.send(NameScoutEvent::Progress(NameScoutProgress {
                        worker,
                        chapter: 0,
                        chunk: 0,
                        total_chunks: 0,
                        chars: 0,
                        elapsed_secs: 0.0,
                        status: NameScoutStatus::Scouting,
                    }));

                    worker_ids.lock().await.push(worker);
                    result
                }
            })
            .buffer_unordered(workers);

        futures::pin_mut!(scouted);
        while let Some(result) = scouted.next().await {
            let chapter = match result {
                Ok(chapter) => chapter,
                Err(error) => {
                    scout_error = Some(error);
                    break;
                }
            };

            for entries in &chapter.mapping_chunks {
                if chapter.record_names {
                    name_mapping.record_votes(&entries.names);
                }
                if chapter.record_terms {
                    name_mapping.record_term_votes(&entries.terms);
                }
                name_mapping.save()?;
            }

            if chapter.record_names {
                name_mapping.add_coverage(&[chapter.number]);
            }
            if chapter.record_terms {
                name_mapping.add_term_coverage(&[chapter.number]);
            }
            name_mapping.save()?;
            *known.write().unwrap_or_else(PoisonError::into_inner) =
                Arc::new(name_mapping.known_mappings());

            clear_status_line();
            console.success(&format!(
                "Scouted chapter {}: found {} names and {} terms",
                chapter.number, chapter.total_names, chapter.total_terms
            ));
        }
    }

    drop(progress_tx);
    let _ = progress_display.await;

    if let Some(error) = scout_error {
        clear_status_line();
        console.error(&error.to_string());
        return Err(error);
    }

    console.success(&format!(
        "Name mapping now has {} names and {} glossary terms",
        name_mapping.len(),
        name_mapping.term_len()
    ));

    Ok(true)
}

struct ScoutedChapter {
    number: u32,
    record_names: bool,
    record_terms: bool,
    total_names: usize,
    total_terms: usize,
    mapping_chunks: Vec<ScoutChunk>,
}

async fn scout_chapter_job(
    name_scout: &NameScout,
    number: u32,
    title: &str,
    content: &str,
    known: &KnownMappings,
    record_names: bool,
    record_terms: bool,
    worker: usize,
    progress_tx: mpsc::UnboundedSender<NameScoutEvent>,
) -> Result<ScoutedChapter> {
    let payload = build_chapter_payload(number, title, content);
    let progress = ScoutProgressInfo {
        chapter: number,
        worker,
        reporter: Some(progress_tx),
    };
    let mapping_chunks = name_scout
        .collect_mappings_with_progress(&payload, known, Some(progress))
        .await
        .with_context(|| format!("Failed to scout mappings for chapter {}", number))?;

    let total_names = mapping_chunks.iter().map(|chunk| chunk.names.len()).sum();
    let total_terms = mapping_chunks.iter().map(|chunk| chunk.terms.len()).sum();

    Ok(ScoutedChapter {
        number,
        record_names,
        record_terms,
        total_names,
        total_terms,
        mapping_chunks,
    })
}

#[derive(Debug, Clone)]
struct ScoutWorkerStat {
    chapter: u32,
    chunk: u32,
    total_chunks: u32,
    elapsed_secs: f64,
    status: NameScoutStatus,
}

async fn display_name_scout_stats(
    mut progress_rx: mpsc::UnboundedReceiver<NameScoutEvent>,
    workers: usize,
) {
    let console = Console::new();
    let mut stats: HashMap<usize, ScoutWorkerStat> = HashMap::new();
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));

    loop {
        tokio::select! {
            event = progress_rx.recv() => {
                let Some(event) = event else {
                    break;
                };
                match event {
                    NameScoutEvent::Progress(progress) => {
                        stats.insert(progress.worker, ScoutWorkerStat {
                            chapter: progress.chapter,
                            chunk: progress.chunk,
                            total_chunks: progress.total_chunks,
                            elapsed_secs: progress.elapsed_secs,
                            status: progress.status,
                        });
                    }
                    NameScoutEvent::Notice(notice) => {
                        clear_status_line();
                        if notice.error {
                            console.error(&notice.message);
                        } else {
                            console.warning(&notice.message);
                        }
                    }
                }
                render_name_scout_stats(&console, &stats, workers);
            }
            _ = ticker.tick() => {
                render_name_scout_stats(&console, &stats, workers);
            }
        }
    }

    clear_status_line();
}

fn render_name_scout_stats(
    console: &Console,
    stats: &HashMap<usize, ScoutWorkerStat>,
    workers: usize,
) {
    let mut worker_parts = Vec::new();

    for worker in 1..=workers {
        let label = console.style(&format!("W{}", worker), &[Style::Cyan, Style::Bold]);

        if let Some(stat) = stats.get(&worker) {
            if stat.chapter == 0 {
                worker_parts.push(format!("{}: {}", label, console.muted("idle")));
            } else {
                let status = match stat.status {
                    NameScoutStatus::Scouting => String::new(),
                    NameScoutStatus::Waiting => {
                        format!(" {}", console.muted("waiting"))
                    }
                    NameScoutStatus::Retrying => {
                        format!(" {}", console.style("retry", &[Style::Yellow, Style::Bold]))
                    }
                    NameScoutStatus::Parsing => {
                        format!(
                            " {}",
                            console.style("parse", &[Style::Magenta, Style::Bold])
                        )
                    }
                };
                worker_parts.push(format!(
                    "{}: c{} {}/{}{} {}",
                    label,
                    stat.chapter,
                    stat.chunk,
                    stat.total_chunks,
                    status,
                    console.muted(&format!("{:.0}s", stat.elapsed_secs))
                ));
            }
        } else {
            worker_parts.push(format!("{}: {}", label, console.muted("idle")));
        }
    }

    let info = console.label("INFO", Style::Blue);

    print!(
        "\r\x1b[2K{} Name scouting: {}",
        info,
        worker_parts.join(", ")
    );
    let _ = io::stdout().flush();
}

fn clear_status_line() {
    print!("\r\x1b[2K");
    let _ = io::stdout().flush();
}

/// Prompts user to review and edit name mappings.
fn manual_name_review(
    console: &Console,
    name_mapping: &mut NameMappingStore,
    config: &Config,
) -> Result<()> {
    console.section("Name Mapping Review");

    let filepath = name_mapping.filepath();
    console.info(&format!("Name mapping file: {}", filepath.display()));

    // Try to open in editor
    let editor_opened = if let Some(ref editor_cmd) = config.paths.editor_command {
        // Use configured editor
        match std::process::Command::new(editor_cmd).arg(filepath).spawn() {
            Ok(_) => {
                console.info(&format!("Opening in {}...", editor_cmd));
                true
            }
            Err(e) => {
                console.warning(&format!("Failed to launch {}: {}", editor_cmd, e));
                false
            }
        }
    } else {
        // Auto-detect editor
        let editors = if cfg!(target_os = "windows") {
            vec!["notepad", "code", "notepad++"]
        } else if cfg!(target_os = "macos") {
            vec!["open", "code", "vim", "nano"]
        } else {
            // Linux and other Unix-like systems
            vec!["kate", "gedit", "code", "vim", "nano", "emacs"]
        };

        let mut opened = false;
        for editor in editors {
            if let Ok(editor_path) = which::which(editor) {
                match std::process::Command::new(&editor_path)
                    .arg(filepath)
                    .spawn()
                {
                    Ok(_) => {
                        console.info(&format!("Opening in {}...", editor));
                        opened = true;
                        break;
                    }
                    Err(_) => continue,
                }
            }
        }
        opened
    };

    if !editor_opened {
        console.info(&format!(
            "Could not auto-detect editor. Please open the file manually: {}",
            filepath.display()
        ));
    }

    // Prompt user
    loop {
        console.info("Review the name mappings and press Enter when done.");
        print!("> ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        // Reload and validate
        match name_mapping.reload_from_disk() {
            Ok(_) => {
                console.success("Name mapping reloaded successfully");
                break;
            }
            Err(e) => {
                console.error(&format!("Failed to reload name mapping: {}", e));
                console.info("Please fix the JSON and try again.");
            }
        }
    }

    Ok(())
}

/// Finds an existing story folder for the novel.
fn find_existing_folder(output_dir: &Path, module_name: &str, novel_id: &str) -> Option<String> {
    let new_format_prefix = format!("[{}: {}]", module_name, novel_id);
    let old_format_prefix = format!("[{}]", novel_id);

    std::fs::read_dir(output_dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .find_map(|entry| {
            if !entry.path().is_dir() {
                return None;
            }

            let name = entry.file_name().to_string_lossy().to_string();
            (name.starts_with(&new_format_prefix) || name.starts_with(&old_format_prefix))
                .then_some(name)
        })
}

/// Finds an existing folder or creates a new one with translated title.
async fn find_or_create_folder(
    console: &Console,
    output_dir: &Path,
    module_name: &str,
    novel_id: &str,
    original_title: &str,
    translator: &Translator,
) -> Result<String> {
    if let Some(name) = find_existing_folder(output_dir, module_name, novel_id) {
        console.info(&format!("Using existing folder: {}", name));
        return Ok(name);
    }

    console.step("Translating title for folder name...");
    let translated_title = translator
        .translate(original_title, true, &[], None)
        .await
        .context("Failed to translate title for folder name")?;

    let folder_prefix = format!("[{}: {}] ", module_name, novel_id);
    let safe_title = sanitize_filename_with_max_bytes(
        &translated_title,
        MAX_FILENAME_COMPONENT_BYTES.saturating_sub(folder_prefix.len()),
    );
    let folder_name = format!("{}{}", folder_prefix, safe_title);
    console.success(&format!("Creating folder: {}", folder_name));

    Ok(folder_name)
}

/// Finds an existing folder or creates one without translating its title.
fn find_or_create_scout_folder(
    console: &Console,
    output_dir: &Path,
    module_name: &str,
    novel_id: &str,
    original_title: &str,
) -> String {
    if let Some(name) = find_existing_folder(output_dir, module_name, novel_id) {
        console.info(&format!("Using existing folder: {}", name));
        return name;
    }

    let folder_prefix = format!("[{}: {}] ", module_name, novel_id);
    let safe_title = sanitize_filename_with_max_bytes(
        original_title,
        MAX_FILENAME_COMPONENT_BYTES.saturating_sub(folder_prefix.len()),
    );
    let folder_name = format!("{}{}", folder_prefix, safe_title);
    console.success(&format!(
        "Creating scout-only folder without translating its title: {}",
        folder_name
    ));

    folder_name
}

/// Validates the chapter range arguments.
fn validate_chapter_range(
    start: Option<u32>,
    end: Option<u32>,
    chapter_list: &ChapterList,
    console: &Console,
) -> Result<(u32, u32)> {
    let total_chapters = chapter_list.len() as u32;

    // One-shots cannot use range
    if chapter_list.is_oneshot() {
        if start.is_some() || end.is_some() {
            anyhow::bail!("Cannot use --start or --end with one-shot stories");
        }
        return Ok((1, 1));
    }

    let start_chapter = start.unwrap_or(1);
    let end_chapter = end.unwrap_or(total_chapters);

    // Validate range
    if start_chapter > end_chapter {
        anyhow::bail!(
            "Start chapter ({}) cannot be greater than end chapter ({})",
            start_chapter,
            end_chapter
        );
    }

    if end_chapter > total_chapters {
        anyhow::bail!(
            "End chapter ({}) exceeds total chapters ({})",
            end_chapter,
            total_chapters
        );
    }

    console.info(&format!(
        "Processing chapters {} to {} of {}",
        start_chapter, end_chapter, total_chapters
    ));

    Ok((start_chapter, end_chapter))
}

fn chapter_filename(chapter_num_str: &str, title: &str) -> String {
    let prefix = format!("{} - ", chapter_num_str);
    let extension = ".txt";
    let max_title_bytes = MAX_FILENAME_COMPONENT_BYTES
        .saturating_sub(prefix.len())
        .saturating_sub(extension.len());
    let safe_title = sanitize_filename_with_max_bytes(title, max_title_bytes);

    format!("{}{}{}", prefix, safe_title, extension)
}

/// Index original chapter files by their numeric filename prefix.
fn cached_originals_by_chapter(original_dir: &Path) -> Result<HashMap<u32, PathBuf>> {
    let mut originals = HashMap::new();

    for entry in std::fs::read_dir(original_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let filename = entry.file_name();
        let filename = filename.to_string_lossy();
        if !filename.ends_with(".txt") {
            continue;
        }

        let Some((number, _)) = filename.split_once(" - ") else {
            continue;
        };
        let Ok(number) = number.parse() else {
            continue;
        };

        originals.entry(number).or_insert(path);
    }

    Ok(originals)
}

/// Sanitizes a string for use as a filename.
fn sanitize_filename(name: &str) -> String {
    // Replace invalid characters with underscore
    let sanitized: String = name
        .chars()
        .map(|c| match c {
            '\\' | '/' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect();

    // Remove trailing dots and spaces
    sanitized.trim_end_matches(['.', ' ']).to_string()
}

fn sanitize_filename_with_max_bytes(name: &str, max_bytes: usize) -> String {
    truncate_to_byte_boundary(&sanitize_filename(name), max_bytes)
}

fn truncate_to_byte_boundary(name: &str, max_bytes: usize) -> String {
    if name.len() <= max_bytes {
        return name.to_string();
    }

    let mut truncated = String::new();
    for ch in name.chars() {
        if truncated.len() + ch.len_utf8() > max_bytes {
            break;
        }
        truncated.push(ch);
    }

    truncated.trim_end_matches(['.', ' ']).to_string()
}

/// Expands ~ in paths to the home directory.
fn expand_path(path: &Path) -> PathBuf {
    let path_str = path.to_string_lossy();
    if let Some(stripped) = path_str.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(stripped);
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn chapter_filename_respects_component_byte_limit() {
        let title = "第".repeat(200);

        let filename = chapter_filename("001", &title);

        assert!(filename.len() <= MAX_FILENAME_COMPONENT_BYTES);
        assert!(filename.ends_with(".txt"));
        assert!(filename.is_char_boundary(filename.len() - ".txt".len()));
    }

    #[test]
    fn cached_originals_survive_999_to_1000_chapter_padding_change() {
        let temp_dir = TempDir::new().unwrap();
        let legacy_filename = chapter_filename("999", "A chapter");
        std::fs::write(temp_dir.path().join(&legacy_filename), "cached").unwrap();

        let current_filename = chapter_filename("0999", "A chapter");
        assert_ne!(legacy_filename, current_filename);

        let originals = cached_originals_by_chapter(temp_dir.path()).unwrap();
        assert_eq!(
            originals.get(&999),
            Some(&temp_dir.path().join(legacy_filename))
        );
    }

    #[test]
    fn filename_truncation_keeps_utf8_boundary_without_hash() {
        let title = "あいうえお";

        let filename = sanitize_filename_with_max_bytes(title, 7);

        assert_eq!(filename, "あい");
        assert!(filename.is_char_boundary(filename.len()));
    }

    #[test]
    fn chapter_ranges_group_contiguous_numbers() {
        assert_eq!(format_chapter_ranges(&[1, 2, 3, 4]), "1-4");
    }

    #[test]
    fn chapter_ranges_preserve_gaps() {
        assert_eq!(format_chapter_ranges(&[1, 2, 4, 6, 7, 8]), "1-2, 4, 6-8");
    }

    #[test]
    fn scout_only_flag_parses() {
        let args =
            Args::try_parse_from(["tsundoku", "--scout-only", "https://example.com"]).unwrap();

        assert!(args.scout_only);
    }

    #[test]
    fn scout_only_conflicts_with_no_name_scout() {
        let args = Args::try_parse_from([
            "tsundoku",
            "--scout-only",
            "--no-name-scout",
            "https://example.com",
        ]);

        assert!(args.is_err());
    }

    #[test]
    fn scout_folder_uses_existing_folder() {
        let temp_dir = TempDir::new().unwrap();
        let existing = "[syosetu: n1234ab] Existing title";
        std::fs::create_dir(temp_dir.path().join(existing)).unwrap();

        let folder = find_or_create_scout_folder(
            &Console::new(),
            temp_dir.path(),
            "syosetu",
            "n1234ab",
            "原題",
        );

        assert_eq!(folder, existing);
    }

    #[test]
    fn scout_folder_uses_original_title_without_translation() {
        let temp_dir = TempDir::new().unwrap();

        let folder = find_or_create_scout_folder(
            &Console::new(),
            temp_dir.path(),
            "syosetu",
            "n1234ab",
            "原題",
        );

        assert_eq!(folder, "[syosetu: n1234ab] 原題");
    }
}
