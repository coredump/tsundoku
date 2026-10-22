//! Tsundoku CLI - Japanese web novel downloader and translator.

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};
use clap_complete::{Shell, generate};
use futures::{StreamExt, stream};
use std::collections::{BTreeSet, HashMap};
use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use tokio::sync::{Mutex, mpsc};
use tsundoku::book::{BookMetadata, ChapterMetadata};
use tsundoku::chapter_record::{CHAPTER_RECORD_VERSION, ChapterRecord, ChunkRecord, Staleness};
use tsundoku::config::{Config, TranslationConfig};
use tsundoku::console::{Console, Style};
use tsundoku::epub::{BookInfo, ChapterFile, collect_chapters};
use tsundoku::honorifics::{self, HonorificPolicy};
use tsundoku::name_mapping::{KnownMappings, NameMappingStore, TermEntry};
use tsundoku::name_scout::{
    NameScout, NameScoutEvent, NameScoutProgress, NameScoutStatus, SUMMARY_CAST_MAX_ENTRIES,
    ScoutChunk, ScoutProgressInfo, SummaryContext, build_chapter_payload,
};
use tsundoku::output_rules::{OutputRules, postprocess};
use tsundoku::qa::{self, ChunkContext, Issue, IssueClass, QaConfig};
use tsundoku::ruby;
use tsundoku::scrapers::{ChapterInfo, ChapterList, ScraperRegistry};
use tsundoku::summaries::SummaryStore;
use tsundoku::translator::{
    ChunkPlan, ChunkResult, ProgressInfo, PromptContext, TranslationProgress, TranslationStage,
    Translator,
};
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

    /// Re-translate chapters in the --start/--end range whose chapter record matches
    /// KINDS, a comma list of: `all` (every issue class), `stale` (the text sent to
    /// the model has changed since, e.g. after a name mapping edit), or issue classes
    /// `failed`, `truncated`, `foreign`, `glossary`, `names`, `paragraphs`, `length`,
    /// `repetition`, `honorifics`. Only flagged chunks are re-translated. Never run it
    /// while another tsundoku process is translating the same novel.
    #[arg(
        long,
        value_name = "KINDS",
        value_delimiter = ',',
        value_parser = parse_recheck_kind,
        conflicts_with = "scout_only"
    )]
    recheck: Option<Vec<RecheckKind>>,
}

/// One item of the `--recheck` list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecheckKind {
    /// Every issue class.
    All,
    /// Chunks whose current source differs from the recorded one.
    Stale,
    /// Chunks with an issue of this class.
    Class(IssueClass),
}

/// Parse one `--recheck` item.
fn parse_recheck_kind(value: &str) -> Result<RecheckKind, String> {
    match value.trim() {
        "all" => Ok(RecheckKind::All),
        "stale" => Ok(RecheckKind::Stale),
        other => other
            .parse()
            .map(RecheckKind::Class)
            .map_err(|error: String| format!("{error}, or `all` or `stale`")),
    }
}

/// What `--recheck` re-translates in already translated chapters.
#[derive(Debug, Clone, Default, PartialEq)]
struct Recheck {
    /// Chunks with an issue in one of these classes.
    classes: Vec<IssueClass>,
    /// Chunks whose source changed since they were translated.
    stale: bool,
}

impl Recheck {
    fn from_kinds(kinds: &[RecheckKind]) -> Self {
        let mut recheck = Self::default();
        let mut add = |class: IssueClass| {
            if !recheck.classes.contains(&class) {
                recheck.classes.push(class);
            }
        };
        for kind in kinds {
            match *kind {
                RecheckKind::All => IssueClass::ALL.into_iter().for_each(&mut add),
                RecheckKind::Class(class) => add(class),
                RecheckKind::Stale => {}
            }
        }
        recheck.stale = kinds.contains(&RecheckKind::Stale);
        recheck
    }
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
    summary_store: &'a mut SummaryStore,
    no_name_pause: bool,
    no_name_scout: bool,
    scout_only: bool,
    name_scout_limit: Option<u32>,
    /// `--recheck` selection, if given.
    recheck: Option<Recheck>,
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
    if chapter_list.is_oneshot() && args.recheck.is_some() {
        anyhow::bail!("--recheck works on chapters; one-shot stories have none");
    }
    let recheck = args.recheck.as_deref().map(Recheck::from_kinds);

    // Initialize name mapping store
    let names_dir = config.names_dir()?;
    let mut name_mapping = NameMappingStore::new(&names_dir, scraper.id(), &novel_info.novel_id)
        .context("Failed to initialize name mapping store")?;
    let mut summary_store = SummaryStore::new(&names_dir, scraper.id(), &novel_info.novel_id)
        .context("Failed to load chapter summaries")?;

    console.info(&format!(
        "Name mapping: {} names and {} glossary terms loaded; {} chapter summaries",
        name_mapping.len(),
        name_mapping.term_len(),
        summary_store.len()
    ));

    // Initialize translator
    let translator = Translator::new(
        config.api.clone(),
        config.translation.clone(),
        config.qa.clone(),
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
    )
    .with_summaries(config.name_scout.summaries);

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
        summary_store: &mut summary_store,
        no_name_pause: args.no_name_pause,
        no_name_scout: args.no_name_scout,
        scout_only: args.scout_only,
        name_scout_limit: args.name_scout_limit,
        recheck,
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
            params.summary_store,
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

        // Render the translation input and collect canonical terms and the cast for
        // this story. The one-shot's book.json, if any, may set its honorific
        // policy and replacements.
        let book = BookMetadata::load(&story_dir)?.unwrap_or_default();
        let known_names = known_name_renderings(params.name_mapping);
        let rules = novel_output_rules(&book, &params.config.translation, &known_names)?;
        let input = render_translation_input(params.name_mapping, &content, rules.policy());
        let prompt = chapter_prompt_context(
            params.name_mapping,
            params.summary_store,
            &params.config.translation,
            &rules,
            1,
            &input.rendered,
            &input.mapped,
        );
        let mapped_content = input.mapped;

        let progress = ProgressInfo {
            chapter: 1,
            chunk: 1,
            total_chunks: 1,
            worker: None,
            reporter: None,
        };

        let chunks = params
            .translator
            .translate_content(&mapped_content, &prompt, &known_names, Some(progress))
            .await
            .context("Failed to translate content")?;

        std::fs::write(
            &translated_path,
            postprocess(&chapter_text(&chunks), &rules),
        )?;
        params.console.success("Translation saved");
        warn_chunk_issues(
            params.console,
            1,
            chunks
                .iter()
                .enumerate()
                .map(|(index, chunk)| (index, chunk.issues.as_slice())),
        );
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
            params.summary_store,
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
        params.summary_store,
        &downloaded_chapters,
        &story_dir,
        &mut book,
        padding,
        &params.config.translation,
        &params.config.qa,
        params.recheck.as_ref(),
    )
    .await?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn translate_chapters(
    console: &Console,
    translator: &Translator,
    name_mapping: &NameMappingStore,
    summary_store: &SummaryStore,
    downloaded_chapters: &[ChapterData],
    story_dir: &Path,
    book: &mut BookMetadata,
    padding: usize,
    translation_config: &TranslationConfig,
    qa_config: &QaConfig,
    recheck: Option<&Recheck>,
) -> Result<()> {
    let workers = translation_config.workers;
    console.info(&format!(
        "Translating with {} chapter worker{}",
        workers,
        if workers == 1 { "" } else { "s" }
    ));

    let known_names = known_name_renderings(name_mapping);
    let rules = novel_output_rules(book, translation_config, &known_names)?;
    let policy = rules.policy();
    let translations = translated_chapter_files(story_dir)?;
    let mut summary = RecheckSummary::default();
    let mut jobs = Vec::new();
    let mut already_translated = Vec::new();

    for chapter_data in downloaded_chapters {
        let chapter_num_str = format!("{:0width$}", chapter_data.number, width = padding);
        let existing = translations.get(&chapter_data.number);
        if existing.is_some() && recheck.is_none() {
            already_translated.push(chapter_data.number);
            continue;
        }

        let input = render_translation_input(name_mapping, &chapter_data.content, policy);
        let prompt = chapter_prompt_context(
            name_mapping,
            summary_store,
            translation_config,
            &rules,
            chapter_data.number,
            &input.rendered,
            &input.mapped,
        );
        let scope = match (existing, recheck) {
            (Some(existing), Some(recheck)) => {
                let title = book
                    .chapters
                    .get(&chapter_data.number)
                    .map(|chapter| chapter.title.as_str())
                    .filter(|title| !title.is_empty())
                    .unwrap_or(&existing.title);
                let ctx = RecheckContext {
                    translator,
                    story_dir,
                    known_names: &known_names,
                    output_rules: &rules,
                    qa_config,
                    recheck,
                };
                let planned = plan_recheck(
                    &ctx,
                    existing,
                    title,
                    &input.mapped,
                    &prompt.glossary,
                    &mut summary,
                )
                .await?;
                match planned {
                    Some(scope) => scope,
                    None => {
                        already_translated.push(chapter_data.number);
                        continue;
                    }
                }
            }
            _ => JobScope::Whole,
        };

        jobs.push(TranslationJob {
            number: chapter_data.number,
            title: chapter_data.title.clone(),
            chapter_num_str,
            mapped_title: honorifics::apply_source(
                &name_mapping.apply_to_text(&ruby::for_translation(&chapter_data.title)),
                policy,
            ),
            mapped_content: input.mapped,
            prompt,
            previous_path: existing.map(|existing| existing.path.clone()),
            scope,
        });
    }

    report_chapter_ranges(
        console,
        &mut already_translated,
        if recheck.is_some() {
            "already translated, nothing to recheck"
        } else {
            "already translated, skipping"
        },
    );

    if !jobs.is_empty() {
        run_translation_jobs(
            console,
            translator,
            &known_names,
            &rules,
            jobs,
            story_dir,
            book,
            workers,
        )
        .await?;
    }

    if recheck.is_some() {
        summary.report(console);
    }

    Ok(())
}

/// Translate jobs on `workers` concurrent workers, saving each chapter as it finishes.
#[allow(clippy::too_many_arguments)]
async fn run_translation_jobs(
    console: &Console,
    translator: &Translator,
    known_names: &[String],
    output_rules: &OutputRules,
    jobs: Vec<TranslationJob>,
    story_dir: &Path,
    book: &mut BookMetadata,
    workers: usize,
) -> Result<()> {
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
                        known_names,
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
            let removed = save_translated_chapter(story_dir, &chapter, output_rules).await?;
            book.chapters.insert(
                chapter.number,
                ChapterMetadata {
                    title: chapter.record.title.clone(),
                    original_title: chapter.original_title.clone(),
                },
            );
            book.save(story_dir)?;
            console.clear_line();
            console.success(&format!("Saved: {}", chapter.filename));
            if let Some(removed) = removed {
                console.info(&format!(
                    "Removed {}, replaced by {}",
                    removed.display(),
                    chapter.filename
                ));
            }
            warn_chunk_issues(
                console,
                chapter.number,
                chapter.translated_chunks.iter().filter_map(|&index| {
                    chapter
                        .record
                        .chunks
                        .get(index)
                        .map(|chunk| (index, chunk.issues.as_slice()))
                }),
            );
        }
    }

    drop(progress_tx);
    let _ = progress_display.await;

    Ok(())
}

/// Build a chapter's prompt context: the canonical terms present in the
/// name-substituted `mapped_content`, the cast whose original names occur in
/// `rendered` (the ruby-rendered chapter text before name substitution), the
/// synopses of the chapters before `chapter`, with names normalised to their
/// selected renderings, and the novel's `output_rules` (honorific policy line and
/// honorific QA).
fn chapter_prompt_context(
    name_mapping: &NameMappingStore,
    summary_store: &SummaryStore,
    translation_config: &TranslationConfig,
    output_rules: &Arc<OutputRules>,
    chapter: u32,
    rendered: &str,
    mapped_content: &str,
) -> PromptContext {
    PromptContext {
        glossary: name_mapping
            .glossary_for_text(mapped_content, translation_config.glossary_max_entries),
        cast: name_mapping.cast_for_text(rendered, translation_config.cast_max_entries),
        previously: summary_store
            .previous(chapter, translation_config.summary_chapters)
            .into_iter()
            .map(|(number, synopsis)| (number, name_mapping.normalize_renderings(&synopsis)))
            .collect(),
        output_rules: Some(Arc::clone(output_rules)),
    }
}

/// The novel's output rules: its honorific policy (`book.json`, else
/// `[translation] honorifics`), the selected English name renderings and the
/// `book.json` replacements.
fn novel_output_rules(
    book: &BookMetadata,
    translation_config: &TranslationConfig,
    known_names: &[String],
) -> Result<Arc<OutputRules>> {
    let rules = book
        .output_rules(translation_config.honorifics, Some(known_names))
        .context("Invalid book.json replacements")?;
    Ok(Arc::new(rules))
}

/// Translation input rendered from original text.
struct TranslationInput {
    /// Ruby rendered for translation, before name substitution (what the cast is
    /// matched against).
    rendered: String,
    /// Exactly what the translator is sent: `rendered` with names substituted,
    /// honorifics after them rewritten by the policy, and blank-line scene breaks
    /// marked.
    mapped: String,
}

/// Render original chapter text into the translation input: ruby rendered for
/// translation, then names substituted, honorifics following them rewritten
/// ([`honorifics::apply_source`]) and blank-line scene breaks marked.
///
/// Translation, one-shots and the `--recheck` stale and legacy checks all use it
/// with the novel's effective policy, so recorded chunk sources stay comparable
/// with a fresh render.
fn render_translation_input(
    name_mapping: &NameMappingStore,
    original: &str,
    policy: HonorificPolicy,
) -> TranslationInput {
    let rendered = ruby::for_translation(original);
    let named = honorifics::apply_source(&name_mapping.apply_to_text(&rendered), policy);
    let mapped = mark_scene_breaks(named);
    TranslationInput { rendered, mapped }
}

/// Selected English renderings of every known name, for the QA name checks.
fn known_name_renderings(name_mapping: &NameMappingStore) -> Vec<String> {
    name_mapping
        .known_mappings()
        .names
        .into_iter()
        .map(|entry| entry.english)
        .collect()
}

/// Translated chapter files in `story_dir` by chapter number, as `tsundoku-epub`
/// sees them (the first file by name when several share a number). Folders such
/// as `Original/` and `.tsundoku/` are ignored.
fn translated_chapter_files(story_dir: &Path) -> Result<HashMap<u32, ChapterFile>> {
    let chapters = collect_chapters(story_dir).with_context(|| {
        format!(
            "Failed to list translated chapters in {}",
            story_dir.display()
        )
    })?;
    Ok(chapters
        .into_iter()
        .map(|chapter| (chapter.number, chapter))
        .collect())
}

/// Inputs shared by the `--recheck` decisions of one run.
struct RecheckContext<'a> {
    translator: &'a Translator,
    story_dir: &'a Path,
    known_names: &'a [String],
    /// The novel's output rules, for the honorific check of legacy chapters.
    output_rules: &'a OutputRules,
    qa_config: &'a QaConfig,
    recheck: &'a Recheck,
}

/// Counts reported at the end of a `--recheck` run.
#[derive(Debug, Default, PartialEq)]
struct RecheckSummary {
    /// Translated chapters examined.
    checked: usize,
    /// Chapters re-translated as a whole.
    whole: usize,
    /// Chapters re-translated in part.
    partial: usize,
    /// Chunks re-translated in partly re-translated chapters.
    chunks: usize,
    /// Legacy chapters (no record) that got their first, chapter-level record.
    legacy_recorded: usize,
    /// Legacy chapters `stale` could not compare, having no recorded source.
    legacy_stale_skipped: usize,
}

impl RecheckSummary {
    fn report(&self, console: &Console) {
        console.info(&format!(
            "Recheck: {} translated chapter(s) checked; re-translated {} whole and {} in part ({} chunk(s))",
            self.checked, self.whole, self.partial, self.chunks
        ));
        if self.legacy_recorded > 0 {
            console.info(&format!(
                "Recheck: recorded chapter-level QA issues for {} chapter(s) translated before chunk records existed",
                self.legacy_recorded
            ));
        }
        if self.legacy_stale_skipped > 0 {
            console.info(&format!(
                "Recheck: skipped {} legacy chapter(s) for `stale` (no recorded source); recheck them by issue class to replace them",
                self.legacy_stale_skipped
            ));
        }
    }
}

/// Decide what `--recheck` re-translates in an already translated chapter.
///
/// A chapter without a record is a legacy translation: it is checked as a whole
/// against the current translation input (`mapped_content`, with `glossary`, the
/// glossary of the chapter's prompt) and gets a one-chunk record with no
/// source. An existing legacy record (e.g. one snapshotted by `tsundoku-epub
/// --rewrite-text`) is checked again the same way, so its issues follow the
/// current mapping. A legacy chapter with a matching issue is re-translated whole;
/// `stale` cannot apply to it. In a normal record, chunks with a matching issue
/// and (with `stale`) chunks whose source changed are re-translated, from the
/// current source when the chunk count is unchanged and from the recorded source
/// otherwise; a stale chunk count re-translates the whole chapter. Returns `None`
/// when nothing matches.
async fn plan_recheck(
    ctx: &RecheckContext<'_>,
    existing: &ChapterFile,
    title: &str,
    mapped_content: &str,
    glossary: &[TermEntry],
    summary: &mut RecheckSummary,
) -> Result<Option<JobScope>> {
    let number = existing.number;
    summary.checked += 1;
    let legacy_issues = |text: &str| {
        let qa_ctx = ChunkContext {
            source: mapped_content,
            glossary,
            known_names: ctx.known_names,
            output_rules: Some(ctx.output_rules),
        };
        let mut issues = qa::check_chunk(&qa_ctx, text, ctx.qa_config);
        issues.extend(qa::check_honorifics(text, ctx.output_rules));
        issues
    };
    let record = match ChapterRecord::load(ctx.story_dir, number)
        .await
        .with_context(|| format!("Failed to load the record of chapter {number}"))?
    {
        Some(mut record) => {
            if record.is_legacy() {
                let issues = legacy_issues(&record.raw_text());
                if let [chunk] = record.chunks.as_mut_slice()
                    && chunk.issues != issues
                {
                    chunk.issues = issues;
                    record.save(ctx.story_dir).await.with_context(|| {
                        format!("Failed to save the record of chapter {number}")
                    })?;
                }
            }
            record
        }
        None => {
            let text = tokio::fs::read_to_string(&existing.path)
                .await
                .with_context(|| format!("Failed to read {}", existing.path.display()))?;
            let issues = legacy_issues(&text);
            let record = ChapterRecord::legacy(number, title.to_string(), text, issues);
            record
                .save(ctx.story_dir)
                .await
                .with_context(|| format!("Failed to save the record of chapter {number}"))?;
            summary.legacy_recorded += 1;
            record
        }
    };

    let mut flagged: BTreeSet<usize> = record
        .chunks_matching(&ctx.recheck.classes)
        .into_iter()
        .collect();
    if record.is_legacy() {
        if !flagged.is_empty() {
            summary.whole += 1;
            return Ok(Some(JobScope::Whole));
        }
        if ctx.recheck.stale {
            summary.legacy_stale_skipped += 1;
        }
        return Ok(None);
    }

    // Blank content is translated as no chunks at all.
    let current = if mapped_content.trim().is_empty() {
        Vec::new()
    } else {
        ctx.translator.split_text_into_chunks(mapped_content)
    };
    if ctx.recheck.stale {
        match record.staleness(&current) {
            Staleness::Fresh | Staleness::Legacy => {}
            Staleness::Chunks(changed) => flagged.extend(changed),
            Staleness::Whole => {
                summary.whole += 1;
                return Ok(Some(JobScope::Whole));
            }
        }
    }
    if flagged.is_empty() {
        return Ok(None);
    }

    let use_current = current.len() == record.chunks.len();
    let plan = record
        .chunks
        .iter()
        .enumerate()
        .map(|(index, chunk)| {
            // Every chunk of a non-legacy record has its source.
            let source = chunk.source.clone().unwrap_or_default();
            if !flagged.contains(&index) {
                return ChunkPlan::Keep {
                    source,
                    output: chunk.raw.clone(),
                };
            }
            match current.get(index) {
                Some(current) if use_current => ChunkPlan::Translate(current.clone()),
                _ => ChunkPlan::Translate(source),
            }
        })
        .collect();
    summary.partial += 1;
    summary.chunks += flagged.len();
    Ok(Some(JobScope::Chunks { record, plan }))
}

/// Write a translated chapter: its record, then the `.txt` built from the record's
/// raw text with `output_rules` applied (the record keeps the raw text, so the
/// rules can be re-applied without compounding), then remove the file it replaces
/// when the name changed (`tsundoku-epub` keeps only the first file per chapter
/// number). Returns the removed file.
async fn save_translated_chapter(
    story_dir: &Path,
    chapter: &TranslatedChapter,
    output_rules: &OutputRules,
) -> Result<Option<PathBuf>> {
    chapter
        .record
        .save(story_dir)
        .await
        .with_context(|| format!("Failed to save the record of chapter {}", chapter.number))?;
    let path = story_dir.join(&chapter.filename);
    tokio::fs::write(&path, postprocess(&chapter.record.raw_text(), output_rules))
        .await
        .with_context(|| format!("Failed to write {}", path.display()))?;

    let Some(previous) = chapter.previous_path.as_ref().filter(|old| **old != path) else {
        return Ok(None);
    };
    match tokio::fs::remove_file(previous).await {
        Ok(()) => Ok(Some(previous.clone())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("Failed to remove {}", previous.display()))
        }
    }
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

/// Join translated chunks into the one-shot text written to disk, as
/// [`ChapterRecord::raw_text`] does for chapters.
fn chapter_text(chunks: &[ChunkResult]) -> String {
    chunks
        .iter()
        .map(|chunk| chunk.output.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Print one warning per issue a translated chunk was kept with; `chunks` yields
/// 0-based chunk indices with their issues.
fn warn_chunk_issues<'a>(
    console: &Console,
    chapter: u32,
    chunks: impl IntoIterator<Item = (usize, &'a [Issue])>,
) {
    for (index, issues) in chunks {
        for issue in issues {
            console.warning(&format!(
                "Chapter {} chunk {}: {} — {}",
                chapter,
                index + 1,
                issue.kind,
                issue.detail
            ));
        }
    }
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
    known_names: &[String],
    job: TranslationJob,
    worker: usize,
    progress_tx: mpsc::UnboundedSender<TranslationProgress>,
) -> Result<TranslatedChapter> {
    let progress = ProgressInfo {
        chapter: job.number,
        chunk: 1,
        total_chunks: 1, // Will be updated by translator
        worker: Some(worker),
        reporter: Some(progress_tx.clone()),
    };

    let (record, translated_chunks) = match job.scope {
        JobScope::Whole => {
            console.step(&format!(
                "Worker {} translating chapter {}: {}",
                worker, job.number, job.title
            ));

            let title_progress = ProgressInfo {
                chapter: job.number,
                chunk: 0,
                total_chunks: 0,
                worker: Some(worker),
                reporter: Some(progress_tx),
            };

            // Translate only the title text and write the number ourselves, so every
            // chapter is headed `Chapter N: …` instead of "Chapter Two" or "Episode 5".
            // The file's chapter number is used because authors' own numbering
            // contains typos.
            let title_text = strip_chapter_number(&job.mapped_title);
            let translated_title = if title_text.is_empty() {
                format!("Chapter {}", job.number)
            } else {
                let translated_text = translator
                    .translate_title(title_text, Some(title_progress))
                    .await
                    .with_context(|| {
                        format!("Failed to translate title for chapter {}", job.number)
                    })?;
                format!("Chapter {}: {}", job.number, translated_text)
            };

            let chunks = translator
                .translate_content(
                    &job.mapped_content,
                    &job.prompt,
                    known_names,
                    Some(progress),
                )
                .await
                .with_context(|| format!("Failed to translate chapter {}", job.number))?;
            let record = ChapterRecord {
                version: CHAPTER_RECORD_VERSION,
                chapter: job.number,
                title: translated_title,
                chunks: chunks.into_iter().map(ChunkRecord::from).collect(),
            };
            let translated_chunks = (0..record.chunks.len()).collect();
            (record, translated_chunks)
        }
        JobScope::Chunks { mut record, plan } => {
            let chunk_numbers: Vec<u32> = plan
                .iter()
                .enumerate()
                .filter(|(_, chunk)| matches!(chunk, ChunkPlan::Translate(_)))
                .map(|(index, _)| index as u32 + 1)
                .collect();
            console.step(&format!(
                "Worker {} re-translating chapter {} chunk(s) {} of {}: {}",
                worker,
                job.number,
                format_chapter_ranges(&chunk_numbers),
                plan.len(),
                job.title
            ));

            let results = translator
                .translate_chunks(plan, &job.prompt, known_names, Some(progress))
                .await
                .with_context(|| format!("Failed to re-translate chapter {}", job.number))?;
            let translated_chunks = results.iter().map(|(index, _)| *index).collect();
            record.splice(
                results
                    .into_iter()
                    .map(|(index, result)| (index, ChunkRecord::from(result)))
                    .collect(),
            );
            (record, translated_chunks)
        }
    };

    Ok(TranslatedChapter {
        filename: chapter_filename(&job.chapter_num_str, &record.title),
        number: job.number,
        original_title: job.title,
        record,
        previous_path: job.previous_path,
        translated_chunks,
    })
}

struct TranslationJob {
    number: u32,
    title: String,
    chapter_num_str: String,
    mapped_title: String,
    mapped_content: String,
    prompt: PromptContext,
    /// Translation file this job replaces (`--recheck`).
    previous_path: Option<PathBuf>,
    /// What to translate.
    scope: JobScope,
}

/// What a translation job translates.
enum JobScope {
    /// The title and every chunk.
    Whole,
    /// The [`ChunkPlan::Translate`] chunks of a recorded chapter, keeping its title.
    Chunks {
        /// The chapter's record, spliced with the new chunks.
        record: ChapterRecord,
        /// One entry per recorded chunk.
        plan: Vec<ChunkPlan>,
    },
}

struct TranslatedChapter {
    filename: String,
    number: u32,
    original_title: String,
    /// The chapter's complete record; its raw text is the chapter file.
    record: ChapterRecord,
    /// Translation file replaced, removed if `filename` differs.
    previous_path: Option<PathBuf>,
    /// Indices of the chunks translated by this job.
    translated_chunks: Vec<usize>,
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

/// What a chapter still needs from the mapping scout.
#[derive(Debug, Clone, Copy)]
struct ScoutNeeds {
    /// Record name votes and name coverage.
    names: bool,
    /// Record term votes and term coverage.
    terms: bool,
}

/// Runs the mapping scout on chapters missing names or terminology coverage, then,
/// with `name_scout.summaries`, the summary pass on chapters without a synopsis.
///
/// Returns true if any chapter was scouted for names or terms, false if only
/// summaries were written or nothing was needed.
async fn run_name_scout(
    console: &Console,
    name_scout: &NameScout,
    name_mapping: &mut NameMappingStore,
    summary_store: &mut SummaryStore,
    chapters: &[(u32, &str, &str)], // (number, title, content)
    workers: usize,
    limit: Option<u32>,
) -> Result<bool> {
    let mapped =
        run_mapping_scout(console, name_scout, name_mapping, chapters, workers, limit).await?;
    if name_scout.summaries_enabled() {
        run_summary_pass(
            console,
            name_scout,
            name_mapping,
            summary_store,
            chapters,
            limit,
        )
        .await?;
    }
    Ok(mapped)
}

/// Runs the mapping scout on chapters missing names or terminology coverage.
///
/// Returns true if any chapter was scouted.
async fn run_mapping_scout(
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
            let needs = ScoutNeeds {
                names: !name_mapping.is_chapter_covered(*number),
                terms: !name_mapping.is_chapter_terms_covered(*number),
            };
            (*number, *title, *content, needs)
        })
        .filter(|(_, _, _, needs)| needs.names || needs.terms)
        .collect();

    if uncovered.is_empty() {
        console.info("All chapters already scouted for names and terminology");
        return Ok(false);
    }

    let plural = |count: usize| if count == 1 { "" } else { "s" };
    console.info(&format!(
        "Scouting {} chapter{} for character names and terminology with {} chapter worker{}",
        uncovered.len(),
        plural(uncovered.len()),
        workers,
        plural(workers)
    ));

    let (progress_tx, progress_rx) = mpsc::unbounded_channel();
    let progress_display = tokio::spawn(display_name_scout_stats(
        progress_rx,
        workers,
        "Name scouting",
    ));
    let worker_ids = Arc::new(Mutex::new((1..=workers).rev().collect::<Vec<_>>()));

    let mut scout_error = None;
    // Workers read the latest selections when they start a chapter, so later chapters
    // reuse renderings voted on by chapters that finished before them.
    let known = RwLock::new(Arc::new(name_mapping.known_mappings()));
    let known = &known;

    {
        let scouted = stream::iter(uncovered)
            .map(|(number, title, content, needs)| {
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
                        needs,
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
                if chapter.needs.names {
                    name_mapping.record_votes(&entries.names);
                }
                if chapter.needs.terms {
                    name_mapping.record_term_votes(&entries.terms);
                }
                name_mapping.save()?;
            }

            if chapter.needs.names {
                name_mapping.add_coverage(&[chapter.number]);
            }
            if chapter.needs.terms {
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

/// Summarises every chapter in range without a stored synopsis, one at a time in
/// chapter order, so each request can carry the previous chapter's synopsis.
///
/// Runs after the mapping scout, so the names, genders and notes it recorded are
/// listed with each chapter. Each synopsis is saved as soon as it is written. A
/// chapter that fails is reported and skipped; the next chapter then goes without a
/// previous synopsis.
async fn run_summary_pass(
    console: &Console,
    name_scout: &NameScout,
    name_mapping: &NameMappingStore,
    summary_store: &mut SummaryStore,
    chapters: &[(u32, &str, &str)], // (number, title, content)
    limit: Option<u32>,
) -> Result<()> {
    console.section("Chapter Summary Phase");

    let mut pending: Vec<_> = chapters
        .iter()
        .filter(|(number, _, _)| limit.is_none_or(|limit| *number <= limit))
        .filter(|(number, _, _)| !summary_store.has(*number))
        .collect();
    if pending.is_empty() {
        console.info("All chapters already summarized");
        return Ok(());
    }
    pending.sort_by_key(|(number, _, _)| *number);
    console.info(&format!(
        "Summarizing {} chapter{} in chapter order",
        pending.len(),
        if pending.len() == 1 { "" } else { "s" }
    ));

    let known = name_mapping.known_mappings();
    let (progress_tx, progress_rx) = mpsc::unbounded_channel();
    let progress_display = tokio::spawn(display_name_scout_stats(progress_rx, 1, "Summarizing"));
    let mut failed = 0;

    for &&(number, title, content) in &pending {
        // Cached originals are canonicalised here too, so ruby reaches the scout as `｜B《R》`.
        let payload = build_chapter_payload(number, title, &ruby::for_scout(content));
        let previous = number.checked_sub(1).and_then(|previous| {
            summary_store
                .get(previous)
                .map(|synopsis| (previous, name_mapping.normalize_renderings(synopsis)))
        });
        let context = SummaryContext::new(
            known.present_in(&payload),
            &name_mapping.cast_for_text(&payload, SUMMARY_CAST_MAX_ENTRIES),
            previous,
        );
        let progress = ScoutProgressInfo {
            chapter: number,
            worker: 1,
            reporter: Some(progress_tx.clone()),
        };

        let result = name_scout
            .summarize_chapter(&payload, &context, Some(progress))
            .await;
        clear_status_line();
        match result {
            Ok(synopsis) => {
                summary_store.set(number, synopsis);
                summary_store.save()?;
                console.success(&format!("Summarized chapter {number}"));
            }
            Err(error) => {
                failed += 1;
                console.warning(&format!(
                    "Failed to summarize chapter {number}: {error}; continuing with the next chapter"
                ));
            }
        }
    }

    drop(progress_tx);
    let _ = progress_display.await;

    if failed > 0 {
        console.warning(&format!(
            "{failed} chapter(s) not summarized; the next run retries them"
        ));
    }
    console.success(&format!(
        "Chapter summaries now cover {} chapters",
        summary_store.len()
    ));
    Ok(())
}

struct ScoutedChapter {
    number: u32,
    needs: ScoutNeeds,
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
    needs: ScoutNeeds,
    worker: usize,
    progress_tx: mpsc::UnboundedSender<NameScoutEvent>,
) -> Result<ScoutedChapter> {
    // Cached originals are canonicalised here too, so ruby reaches the scout as `｜B《R》`.
    let payload = build_chapter_payload(number, title, &ruby::for_scout(content));
    let progress = ScoutProgressInfo {
        chapter: number,
        worker,
        reporter: Some(progress_tx),
    };
    let mapping_chunks = name_scout
        .collect_mappings_with_progress(&payload, known, Some(progress))
        .await
        .with_context(|| format!("Failed to scout mappings for chapter {}", number))?;

    Ok(ScoutedChapter {
        number,
        needs,
        total_names: mapping_chunks.iter().map(|chunk| chunk.names.len()).sum(),
        total_terms: mapping_chunks.iter().map(|chunk| chunk.terms.len()).sum(),
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

/// Show the scout status line; `activity` names the phase (e.g. "Name scouting").
async fn display_name_scout_stats(
    mut progress_rx: mpsc::UnboundedReceiver<NameScoutEvent>,
    workers: usize,
    activity: &'static str,
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
                render_name_scout_stats(&console, &stats, workers, activity);
            }
            _ = ticker.tick() => {
                render_name_scout_stats(&console, &stats, workers, activity);
            }
        }
    }

    clear_status_line();
}

fn render_name_scout_stats(
    console: &Console,
    stats: &HashMap<usize, ScoutWorkerStat>,
    workers: usize,
    activity: &str,
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
        "\r\x1b[2K{} {}: {}",
        info,
        activity,
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
        .translate_title(original_title, None)
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
    use tsundoku::config::{ApiConfig, TranslationConfig};
    use tsundoku::name_mapping::{NameEntry, TermEntry};

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

    fn name_vote(original: &str, english: &str) -> tsundoku::name_mapping::NameEntry {
        tsundoku::name_mapping::NameEntry {
            original: original.to_string(),
            english: english.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn prompt_context_carries_normalized_previous_synopses() {
        let temp_dir = TempDir::new().unwrap();
        let mut name_mapping =
            NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        name_mapping.record_votes(&[
            name_vote("タツヤ", "Tatsuya"),
            name_vote("タツヤ", "Tatsuya"),
            name_vote("タツヤ", "Tatuya"),
        ]);
        let mut summaries = SummaryStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        summaries.set(3, "Tatuya meets Ruri.".to_string());
        summaries.set(4, "Ruri leaves.".to_string());
        let config = TranslationConfig::default();
        let previously = |chapter, config: &TranslationConfig| {
            chapter_prompt_context(
                &name_mapping,
                &summaries,
                config,
                &Arc::new(OutputRules::default()),
                chapter,
                "",
                "",
            )
            .system_prompt("Base")
            .split_once("\n\nStory so far (context only; do not translate):\n")
            .map(|(_, section)| section.to_string())
        };

        assert_eq!(previously(1, &config), None);
        assert_eq!(
            previously(5, &config).as_deref(),
            Some("- Ch 3: Tatsuya meets Ruri.\n- Ch 4: Ruri leaves.")
        );
        // Chapter 5 has no synopsis, so chapter 6 sees only chapter 4.
        assert_eq!(
            previously(6, &config).as_deref(),
            Some("- Ch 4: Ruri leaves.")
        );
        let disabled = TranslationConfig {
            summary_chapters: 0,
            ..TranslationConfig::default()
        };
        assert_eq!(previously(5, &disabled), None);
    }

    #[tokio::test]
    async fn summary_pass_runs_after_mapping_in_chapter_order_with_context() {
        use tsundoku::config::NameScoutConfig;
        use tsundoku::name_mapping::Gender;
        use tsundoku::name_scout::SUMMARY_SPLIT_CHARS;

        const TEXT_MARK: &str = "Text:\n### Chapter ";
        let (base_url, bodies) = serve_chat(|body| {
            let system = body["messages"][0]["content"].as_str().unwrap();
            let user = body["messages"][1]["content"].as_str().unwrap();
            if system.starts_with("Extract mappings") {
                return r#"{"names":[],"terms":[]}"#.to_string();
            }
            let summary = match user.find(TEXT_MARK) {
                Some(start) => {
                    let number: String = user[start + TEXT_MARK.len()..]
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect();
                    format!("Chapter {number} happens.")
                }
                None => "Ruri keeps walking.".to_string(),
            };
            serde_json::json!({ "summary": summary }).to_string()
        });

        let temp_dir = TempDir::new().unwrap();
        let mut name_mapping =
            NameMappingStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        let ruri = NameEntry {
            gender: Some(Gender::Female),
            note: Some("homunculus Tatsuya created".to_string()),
            ..name_vote("ルリ", "Ruri")
        };
        name_mapping.record_votes(&[ruri.clone(), ruri, name_vote("タツヤ", "Tatsuya")]);
        let stone = TermEntry {
            original: "魔石".to_string(),
            english: "magic stone".to_string(),
        };
        name_mapping.record_term_votes(&[stone.clone(), stone]);
        // Chapters 1-3 are already scouted for names and terms; chapter 4 is not.
        name_mapping.add_coverage(&[1, 2, 3]);
        name_mapping.add_term_coverage(&[1, 2, 3]);
        let mut summaries = SummaryStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        let scout = NameScout::new(
            ApiConfig {
                key: "test-key".to_string(),
                base_url,
                model: "test-model".to_string(),
                ..Default::default()
            },
            NameScoutConfig {
                retries: 1,
                json_retries: 1,
                delay_between_requests_sec: 0.0,
                ..Default::default()
            },
            "Extract mappings".to_string(),
        )
        .with_summaries(true);
        let long = "ルリは歩いた。\n".repeat(SUMMARY_SPLIT_CHARS / 8 + 100);

        let mapping_scouted = run_name_scout(
            &Console::new(),
            &scout,
            &mut name_mapping,
            &mut summaries,
            &[
                (4, "第四話", "タツヤは魔石を見せた。"),
                (1, "第一話", "タツヤはルリを作った。"),
                (2, "第二話", "ルリは笑った。"),
                (3, "第三話", &long),
            ],
            2,
            None,
        )
        .await
        .unwrap();

        assert!(mapping_scouted);
        let bodies = bodies.lock().unwrap();
        let systems: Vec<&str> = bodies
            .iter()
            .map(|body| body["messages"][0]["content"].as_str().unwrap())
            .collect();
        let users: Vec<String> = bodies.iter().map(last_user_turn).collect();
        assert_eq!(bodies.len(), 6, "{users:#?}");
        // Only the uncovered chapter gets a mapping request, and before any summary.
        assert!(systems[0].starts_with("Extract mappings"));
        assert!(users[0].contains("### Chapter 4 - "));
        assert!(
            systems[1..]
                .iter()
                .all(|system| system.starts_with("You summarize one chapter"))
        );

        assert!(users[1].contains("Text:\n### Chapter 1 - "));
        assert!(users[1].contains("- タツヤ → Tatsuya\n"));
        assert!(!users[1].contains("Previous chapter"));

        assert!(users[2].contains("Text:\n### Chapter 2 - "));
        assert!(users[2].contains("Previous chapter (1): Chapter 1 happens.\n"));
        assert!(users[2].contains("- ルリ → Ruri: female; homunculus Tatsuya created\n"));

        // The long chapter is split; the second piece carries the first one's summary.
        assert!(users[3].contains("Text:\n### Chapter 3 - "));
        assert!(users[3].contains("Previous chapter (2): Chapter 2 happens.\n"));
        assert!(!users[3].contains("Earlier in this chapter"));
        assert!(users[4].contains("Earlier in this chapter: Chapter 3 happens.\n"));
        assert!(!users[4].contains("### Chapter 3"));
        assert!(systems[3].contains("at most 50 English words"));

        assert!(users[5].contains("Text:\n### Chapter 4 - "));
        assert!(
            users[5].contains("Previous chapter (3): Chapter 3 happens. Ruri keeps walking.\n")
        );
        assert!(users[5].contains("Terms:\n- 魔石 → magic stone\n"));

        let reloaded = SummaryStore::new(temp_dir.path(), "syosetu", "n1234ab").unwrap();
        assert_eq!(reloaded.len(), 4);
        assert_eq!(
            reloaded.get(3),
            Some("Chapter 3 happens. Ruri keeps walking.")
        );
        assert_eq!(reloaded.get(4), Some("Chapter 4 happens."));
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

    #[test]
    fn recheck_parses_kinds_all_and_stale() {
        let args = Args::try_parse_from([
            "tsundoku",
            "--recheck",
            "names,stale,glossary,names",
            "https://example.com",
        ])
        .unwrap();
        let recheck = Recheck::from_kinds(args.recheck.as_deref().unwrap());
        assert_eq!(
            recheck,
            Recheck {
                classes: vec![IssueClass::Names, IssueClass::Glossary],
                stale: true,
            }
        );

        let all = Recheck::from_kinds(&[RecheckKind::All]);
        assert_eq!(all.classes, IssueClass::ALL);
        assert!(!all.stale);

        let error = Args::try_parse_from(["tsundoku", "--recheck", "typo", "https://example.com"])
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown issue class `typo`"), "{error}");
        assert!(
            Args::try_parse_from([
                "tsundoku",
                "--recheck",
                "all",
                "--scout-only",
                "https://example.com",
            ])
            .is_err()
        );
    }

    /// A story folder with a name mapping and a translator splitting at 14 chars.
    struct RecheckFixture {
        dir: TempDir,
        name_mapping: NameMappingStore,
        translator: Translator,
    }

    impl RecheckFixture {
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            let mut name_mapping =
                NameMappingStore::new(&dir.path().join("names"), "syosetu", "n1").unwrap();
            name_mapping.record_votes(&[NameEntry {
                original: "達也".to_string(),
                english: "Tatsuya".to_string(),
                ..NameEntry::default()
            }]);
            name_mapping.record_term_votes(&[TermEntry {
                original: "魔石".to_string(),
                english: "Magic Stone".to_string(),
            }]);
            let translator = Translator::new(
                ApiConfig::default(),
                TranslationConfig {
                    chunk_size_chars: 14,
                    ..TranslationConfig::default()
                },
                QaConfig::default(),
                String::new(),
                String::new(),
            );
            Self {
                dir,
                name_mapping,
                translator,
            }
        }

        fn story_dir(&self) -> &Path {
            self.dir.path()
        }

        async fn plan(
            &self,
            recheck: &Recheck,
            existing: &ChapterFile,
            original: &str,
            summary: &mut RecheckSummary,
        ) -> Option<JobScope> {
            let known_names = known_name_renderings(&self.name_mapping);
            let output_rules = OutputRules::default();
            let ctx = RecheckContext {
                translator: &self.translator,
                story_dir: self.story_dir(),
                known_names: &known_names,
                output_rules: &output_rules,
                qa_config: &QaConfig::default(),
                recheck,
            };
            let mapped =
                render_translation_input(&self.name_mapping, original, HonorificPolicy::Keep)
                    .mapped;
            let glossary = self.name_mapping.glossary_for_text(&mapped, 100);
            plan_recheck(&ctx, existing, &existing.title, &mapped, &glossary, summary)
                .await
                .unwrap()
        }
    }

    fn chapter_file(story_dir: &Path, number: u32, title: &str, text: &str) -> ChapterFile {
        let path = story_dir.join(chapter_filename(&number.to_string(), title));
        std::fs::write(&path, text).unwrap();
        ChapterFile {
            number,
            title: title.to_string(),
            path,
        }
    }

    #[test]
    fn translation_input_applies_the_honorific_policy_after_names() {
        let fixture = RecheckFixture::new();
        let original = "達也君は笑った。\n村長さんが来た。";
        let render = |policy| render_translation_input(&fixture.name_mapping, original, policy);

        let keep = render(HonorificPolicy::Keep);
        assert_eq!(keep.mapped, "Tatsuya-kunは笑った。\n村長さんが来た。");
        assert_eq!(keep.rendered, original);
        assert_eq!(
            render(HonorificPolicy::NamesOnly).mapped,
            "Tatsuya-kunは笑った。\n村長さんが来た。"
        );
        let drop = render(HonorificPolicy::Drop);
        assert_eq!(drop.mapped, "Tatsuyaは笑った。\n村長さんが来た。");
        assert_eq!(drop.rendered, original);
    }

    #[tokio::test]
    async fn stale_recheck_retranslates_only_chunks_changed_by_a_mapping_edit() {
        let mut fixture = RecheckFixture::new();
        let original = "達也は笑った。\n花子は泣いた。";
        let sources = fixture.translator.split_text_into_chunks(
            &render_translation_input(&fixture.name_mapping, original, HonorificPolicy::Keep)
                .mapped,
        );
        assert_eq!(sources, ["Tatsuyaは笑った。", "花子は泣いた。"]);
        let record = ChapterRecord {
            version: CHAPTER_RECORD_VERSION,
            chapter: 3,
            title: "Chapter 3: Tears".to_string(),
            chunks: sources
                .iter()
                .zip(["Tatsuya laughed.", "花子 cried."])
                .map(|(source, raw)| ChunkRecord {
                    source: Some(source.clone()),
                    raw: raw.to_string(),
                    issues: Vec::new(),
                })
                .collect(),
        };
        record.save(fixture.story_dir()).await.unwrap();
        let existing = chapter_file(fixture.story_dir(), 3, &record.title, &record.raw_text());
        let stale = Recheck {
            classes: Vec::new(),
            stale: true,
        };
        let mut summary = RecheckSummary::default();
        assert!(
            fixture
                .plan(&stale, &existing, original, &mut summary)
                .await
                .is_none()
        );

        fixture.name_mapping.record_votes(&[NameEntry {
            original: "花子".to_string(),
            english: "Hanako".to_string(),
            ..NameEntry::default()
        }]);
        let Some(JobScope::Chunks {
            record: planned,
            plan,
        }) = fixture
            .plan(&stale, &existing, original, &mut summary)
            .await
        else {
            panic!("expected a partial re-translation");
        };

        assert_eq!(planned, record);
        assert_eq!(
            plan,
            [
                ChunkPlan::Keep {
                    source: "Tatsuyaは笑った。".to_string(),
                    output: "Tatsuya laughed.".to_string(),
                },
                ChunkPlan::Translate("Hanakoは泣いた。".to_string()),
            ]
        );
        assert_eq!(
            summary,
            RecheckSummary {
                checked: 2,
                partial: 1,
                chunks: 1,
                ..RecheckSummary::default()
            }
        );
    }

    #[tokio::test]
    async fn legacy_chapter_gets_a_chapter_level_record_and_is_retranslated_whole() {
        let fixture = RecheckFixture::new();
        let original = "魔石を拾った。";
        let existing = chapter_file(
            fixture.story_dir(),
            1,
            "Chapter 1: Old",
            "He picked up a stone.",
        );
        let mut summary = RecheckSummary::default();

        let stale_only = Recheck {
            classes: Vec::new(),
            stale: true,
        };
        assert!(
            fixture
                .plan(&stale_only, &existing, original, &mut summary)
                .await
                .is_none()
        );
        let record = ChapterRecord::load(fixture.story_dir(), 1)
            .await
            .unwrap()
            .unwrap();
        assert!(record.is_legacy());
        assert_eq!(record.title, "Chapter 1: Old");
        assert_eq!(record.raw_text(), "He picked up a stone.");
        assert_eq!(
            record.issue_counts().into_iter().collect::<Vec<_>>(),
            [(qa::IssueKind::GlossaryMissing, 1)]
        );

        let glossary = Recheck {
            classes: vec![IssueClass::Glossary],
            stale: false,
        };
        assert!(matches!(
            fixture
                .plan(&glossary, &existing, original, &mut summary)
                .await,
            Some(JobScope::Whole)
        ));
        assert_eq!(
            summary,
            RecheckSummary {
                checked: 2,
                whole: 1,
                legacy_recorded: 1,
                legacy_stale_skipped: 1,
                ..RecheckSummary::default()
            }
        );
    }

    #[tokio::test]
    async fn existing_legacy_record_without_issues_is_checked_again() {
        let fixture = RecheckFixture::new();
        let existing = chapter_file(
            fixture.story_dir(),
            2,
            "Chapter 2: Snapshot",
            "He picked up a stone.",
        );
        // As `tsundoku-epub --rewrite-text` leaves it: a legacy record with no issues.
        ChapterRecord::legacy(
            2,
            "Chapter 2: Snapshot".to_string(),
            "He picked up a stone.".to_string(),
            Vec::new(),
        )
        .save(fixture.story_dir())
        .await
        .unwrap();
        let glossary = Recheck {
            classes: vec![IssueClass::Glossary],
            stale: false,
        };

        let scope = fixture
            .plan(
                &glossary,
                &existing,
                "魔石を拾った。",
                &mut RecheckSummary::default(),
            )
            .await;

        assert!(matches!(scope, Some(JobScope::Whole)));
        let record = ChapterRecord::load(fixture.story_dir(), 2)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            record.issue_counts().into_iter().collect::<Vec<_>>(),
            [(qa::IssueKind::GlossaryMissing, 1)]
        );
    }

    #[tokio::test]
    async fn legacy_chapter_records_honorifics_on_nouns() {
        let fixture = RecheckFixture::new();
        let existing = chapter_file(
            fixture.story_dir(),
            4,
            "Chapter 4: Cats",
            "Tatsuya fed the cat-san.",
        );
        let honorifics = Recheck {
            classes: vec![IssueClass::Honorifics],
            stale: false,
        };

        let scope = fixture
            .plan(
                &honorifics,
                &existing,
                "達也は猫さんに餌をやった。",
                &mut RecheckSummary::default(),
            )
            .await;

        assert!(matches!(scope, Some(JobScope::Whole)));
        let record = ChapterRecord::load(fixture.story_dir(), 4)
            .await
            .unwrap()
            .unwrap();
        let honorific_issues: Vec<&str> = record.chunks[0]
            .issues
            .iter()
            .filter(|issue| issue.kind == qa::IssueKind::HonorificOnNoun)
            .map(|issue| issue.detail.as_str())
            .collect();
        assert_eq!(honorific_issues, ["cat-san"]);
    }

    #[tokio::test]
    async fn saving_a_retitled_chapter_replaces_the_old_file() {
        let temp_dir = TempDir::new().unwrap();
        let story_dir = temp_dir.path();
        let old = chapter_file(story_dir, 7, "Chapter 7: Old", "Old text.");
        let record = ChapterRecord {
            version: CHAPTER_RECORD_VERSION,
            chapter: 7,
            title: "Chapter 7: New".to_string(),
            chunks: ["One.", "Two."]
                .into_iter()
                .map(|raw| ChunkRecord {
                    source: Some("原文".to_string()),
                    raw: raw.to_string(),
                    issues: Vec::new(),
                })
                .collect(),
        };
        let chapter = TranslatedChapter {
            filename: chapter_filename("7", &record.title),
            number: 7,
            original_title: "新".to_string(),
            record,
            previous_path: Some(old.path.clone()),
            translated_chunks: vec![0, 1],
        };

        let rules = OutputRules::default();
        let removed = save_translated_chapter(story_dir, &chapter, &rules)
            .await
            .unwrap();

        assert_eq!(removed, Some(old.path.clone()));
        assert!(!old.path.exists());
        let files = translated_chapter_files(story_dir).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(
            std::fs::read_to_string(&files[&7].path).unwrap(),
            "One.\n\nTwo."
        );
        assert_eq!(
            ChapterRecord::load(story_dir, 7).await.unwrap().as_ref(),
            Some(&chapter.record)
        );

        // Same title: the file is rewritten in place and nothing is removed.
        let again = TranslatedChapter {
            previous_path: Some(files[&7].path.clone()),
            ..chapter
        };
        assert_eq!(
            save_translated_chapter(story_dir, &again, &rules)
                .await
                .unwrap(),
            None
        );
        assert!(files[&7].path.exists());
    }

    #[tokio::test]
    async fn saved_chapter_text_is_postprocessed_but_the_record_keeps_raw() {
        let temp_dir = TempDir::new().unwrap();
        let story_dir = temp_dir.path();
        let raw = "Ruri waved at the cat-san.";
        let record = ChapterRecord {
            version: CHAPTER_RECORD_VERSION,
            chapter: 2,
            title: "Chapter 2: Cats".to_string(),
            chunks: vec![ChunkRecord {
                source: Some("ルリは猫さんに手を振った。".to_string()),
                raw: raw.to_string(),
                issues: Vec::new(),
            }],
        };
        let chapter = TranslatedChapter {
            filename: chapter_filename("2", &record.title),
            number: 2,
            original_title: "猫".to_string(),
            record,
            previous_path: None,
            translated_chunks: vec![0],
        };
        let book = BookMetadata {
            honorifics: Some(HonorificPolicy::NamesOnly),
            replacements: vec![tsundoku::book::Replacement {
                find: "Ruri".to_string(),
                replace: "Ruri-chan".to_string(),
                regex: false,
            }],
            ..BookMetadata::default()
        };
        let rules =
            novel_output_rules(&book, &TranslationConfig::default(), &["Ruri".into()]).unwrap();
        let path = story_dir.join(&chapter.filename);

        // Saving twice re-applies the rules to the raw text, never to the output.
        for _ in 0..2 {
            save_translated_chapter(story_dir, &chapter, &rules)
                .await
                .unwrap();
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "Ruri-chan waved at the cat."
            );
            let saved = ChapterRecord::load(story_dir, 2).await.unwrap().unwrap();
            assert_eq!(saved.raw_text(), raw);
        }
    }

    /// Serve chat completions on a background thread, answering each request with
    /// `reply(body)`, streamed when the request asks for it; returns the base URL and
    /// the request bodies seen.
    fn serve_chat(
        reply: impl Fn(&serde_json::Value) -> String + Send + 'static,
    ) -> (String, Arc<std::sync::Mutex<Vec<serde_json::Value>>>) {
        use std::io::{BufRead, BufReader, Read};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&bodies);
        std::thread::spawn(move || {
            for socket in listener.incoming() {
                let mut socket = socket.unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut content_length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                        content_length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; content_length];
                reader.read_exact(&mut body).unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let content = reply(&body);
                let stream = body["stream"] == true;
                seen.lock().unwrap().push(body);

                if !stream {
                    let reply =
                        serde_json::json!({ "choices": [{ "message": { "content": content } }] })
                            .to_string();
                    write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
                    )
                    .unwrap();
                    continue;
                }
                let delta = serde_json::json!({ "choices": [{ "index": 0, "delta": { "content": content }, "finish_reason": null }] });
                let stop = serde_json::json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] });
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {delta}\n\ndata: {stop}\n\ndata: [DONE]\n\n"
                )
                .unwrap();
            }
        });
        (base_url, bodies)
    }

    fn last_user_turn(body: &serde_json::Value) -> String {
        body["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn chapters_get_records_and_recheck_retranslates_only_flagged_chunks() {
        let fixed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fixed_in_server = Arc::clone(&fixed);
        let (base_url, bodies) = serve_chat(move |body| {
            let user = last_user_turn(body);
            if body["messages"][0]["content"] == "Translate this title" {
                "Stones".to_string()
            } else if !user.contains("魔石") {
                "The next day, he went to town.".to_string()
            } else if fixed_in_server.load(std::sync::atomic::Ordering::SeqCst) {
                "He picked up a Magic Stone.".to_string()
            } else {
                "He picked up a stone.".to_string()
            }
        });
        let fixture = RecheckFixture::new();
        let story_dir = fixture.story_dir();
        let translator = Translator::new(
            ApiConfig {
                base_url,
                ..ApiConfig::default()
            },
            TranslationConfig {
                chunk_size_chars: 10,
                delay_between_requests_sec: 0.0,
                ..TranslationConfig::default()
            },
            QaConfig {
                max_retries: 0,
                ..QaConfig::default()
            },
            "Translate this title".to_string(),
            "Translate this content".to_string(),
        );
        let chapters = [ChapterData {
            number: 1,
            title: "第一話 魔石".to_string(),
            content: "魔石を拾った。\n次の日、町へ行った。".to_string(),
            filename: String::new(),
        }];
        let mut book = BookMetadata::default();
        async fn run(
            fixture: &RecheckFixture,
            translator: &Translator,
            chapters: &[ChapterData],
            book: &mut BookMetadata,
            recheck: Option<&Recheck>,
        ) {
            let summary_store =
                SummaryStore::new(&fixture.story_dir().join("names"), "syosetu", "n1").unwrap();
            translate_chapters(
                &Console::new(),
                translator,
                &fixture.name_mapping,
                &summary_store,
                chapters,
                fixture.story_dir(),
                book,
                1,
                &TranslationConfig::default(),
                &QaConfig::default(),
                recheck,
            )
            .await
            .unwrap();
        }

        run(&fixture, &translator, &chapters, &mut book, None).await;
        let record = ChapterRecord::load(story_dir, 1).await.unwrap().unwrap();
        assert_eq!(record.title, "Chapter 1: Stones");
        assert_eq!(record.chunks.len(), 2);
        assert_eq!(
            record.chunks[0].issues.first().map(|issue| issue.kind),
            Some(qa::IssueKind::GlossaryMissing)
        );
        let path = story_dir.join("1 - Chapter 1: Stones.txt");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "He picked up a stone.\n\nThe next day, he went to town."
        );
        assert_eq!(bodies.lock().unwrap().len(), 3);

        fixed.store(true, std::sync::atomic::Ordering::SeqCst);
        let glossary = Recheck {
            classes: vec![IssueClass::Glossary],
            stale: false,
        };
        run(&fixture, &translator, &chapters, &mut book, Some(&glossary)).await;
        {
            let bodies = bodies.lock().unwrap();
            assert_eq!(bodies.len(), 4);
            assert_eq!(last_user_turn(&bodies[3]), "魔石を拾った。");
        }
        let rechecked = ChapterRecord::load(story_dir, 1).await.unwrap().unwrap();
        assert_eq!(rechecked.title, record.title);
        assert!(rechecked.chunks[0].issues.is_empty());
        assert_eq!(rechecked.chunks[1], record.chunks[1]);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "He picked up a Magic Stone.\n\nThe next day, he went to town."
        );

        // Nothing left to recheck, and an unchanged mapping is not stale.
        let glossary_and_stale = Recheck {
            stale: true,
            ..glossary
        };
        run(
            &fixture,
            &translator,
            &chapters,
            &mut book,
            Some(&glossary_and_stale),
        )
        .await;
        assert_eq!(bodies.lock().unwrap().len(), 4);
    }
}
