//! Tsundoku CLI - Japanese web novel downloader and translator.

use anyhow::{Context, Result};
use clap::Parser;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tsundoku::config::Config;
use tsundoku::console::Console;
use tsundoku::name_mapping::NameMappingStore;
use tsundoku::name_scout::{NameScout, build_chapter_payload};
use tsundoku::scrapers::{ChapterInfo, ChapterList, ScraperRegistry};
use tsundoku::translator::{ProgressInfo, Translator};

/// Japanese web novel downloader and translator.
#[derive(Parser, Debug)]
#[command(name = "tsundoku")]
#[command(author, version, about, long_about = None)]
struct Args {
    /// URL of the novel to download.
    novel_url: String,

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
    config: &'a Config,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let console = Console::new();

    console.section("Tsundoku - Web Novel Downloader");

    // Load configuration
    console.step("Loading configuration...");
    let mut config = Config::load().context("Failed to load configuration")?;
    config.scraping.debug = args.debug;

    // Check if this is first run (API key not configured)
    if !config.api.is_configured() {
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
        .validate_with_options(!args.no_name_scout)
        .context("Invalid configuration")?;
    console.success("Configuration loaded");

    // Find appropriate scraper
    console.step("Finding scraper for URL...");
    let registry = ScraperRegistry::new(&config.scraping);
    let scraper = registry
        .find_for_url(&args.novel_url)
        .ok_or_else(|| anyhow::anyhow!("No scraper found for URL: {}", args.novel_url))?;

    console.success(&format!("Using {} scraper", scraper.name()));

    // Fetch novel info
    console.step("Fetching novel information...");
    let novel_info = scraper
        .get_novel_info(&args.novel_url)
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
        "Name mapping: {} names loaded, {} chapters covered",
        name_mapping.len(),
        name_mapping.coverage().len()
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
    let folder_name = find_or_create_folder(
        params.console,
        params.output_dir,
        params.scraper.id(),
        &params.novel_info.novel_id,
        &params.novel_info.title,
        params.translator,
    )
    .await?;

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
        )
        .await?
    };

    // Manual review (only if scouting was performed)
    if !params.no_name_pause && scouted {
        manual_name_review(params.console, params.name_mapping, params.config)?;
    }

    // Translate content
    let translated_path = story_dir.join("oneshot.txt");
    if translated_path.exists() {
        params
            .console
            .info("Translation already exists, skipping...");
    } else {
        params.console.step("Translating content...");

        // Apply name mapping
        let mapped_content = params.name_mapping.apply_to_text(&content);

        let progress = ProgressInfo {
            chapter: 1,
            chunk: 1,
            total_chunks: 1,
        };

        let translated = params
            .translator
            .translate(&mapped_content, false, Some(progress))
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
    let folder_name = find_or_create_folder(
        params.console,
        params.output_dir,
        params.scraper.id(),
        &params.novel_info.novel_id,
        &params.novel_info.title,
        params.translator,
    )
    .await?;

    let story_dir = params.output_dir.join(&folder_name);
    let original_dir = story_dir.join("Original");
    std::fs::create_dir_all(&original_dir)?;

    // Calculate padding for chapter numbers
    let total_chapters = chapters.len();
    let padding = total_chapters.to_string().len();

    // Download phase
    params.console.section("Download Phase");

    let mut downloaded_chapters: Vec<ChapterData> = Vec::new();

    for chapter in chapters.iter() {
        if chapter.number < start_chapter || chapter.number > end_chapter {
            continue;
        }

        let chapter_num_str = format!("{:0width$}", chapter.number, width = padding);
        let filename = format!(
            "{} - {}.txt",
            chapter_num_str,
            sanitize_filename(&chapter.title)
        );
        let original_path = original_dir.join(&filename);

        let content = if original_path.exists() {
            params
                .console
                .info(&format!("Chapter {} already downloaded", chapter.number));
            std::fs::read_to_string(&original_path)?
        } else {
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
        )
        .await?
    };

    // Manual review (only if scouting was performed)
    if !params.no_name_pause && scouted {
        manual_name_review(params.console, params.name_mapping, params.config)?;
    }

    // Translation phase
    params.console.section("Translation Phase");

    for chapter_data in &downloaded_chapters {
        // Check if translation already exists
        let chapter_num_str = format!("{:0width$}", chapter_data.number, width = padding);
        let pattern = format!("{} - ", chapter_num_str);

        let translation_exists = std::fs::read_dir(&story_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .any(|e| e.file_name().to_string_lossy().starts_with(&pattern));

        if translation_exists {
            params.console.info(&format!(
                "Chapter {} already translated, skipping",
                chapter_data.number
            ));
            continue;
        }

        params.console.step(&format!(
            "Translating chapter {}: {}",
            chapter_data.number, chapter_data.title
        ));

        // Translate title
        let mapped_title = params.name_mapping.apply_to_text(&chapter_data.title);
        let translated_title = params
            .translator
            .translate(&mapped_title, true, None)
            .await
            .with_context(|| {
                format!(
                    "Failed to translate title for chapter {}",
                    chapter_data.number
                )
            })?;

        // Validate translated title for filesystem
        let safe_title = sanitize_filename(&translated_title);

        // Apply name mapping to content
        let mapped_content = params.name_mapping.apply_to_text(&chapter_data.content);

        // Translate content
        let progress = ProgressInfo {
            chapter: chapter_data.number,
            chunk: 1,
            total_chunks: 1, // Will be updated by translator
        };

        let translated_content = params
            .translator
            .translate(&mapped_content, false, Some(progress))
            .await
            .context("Failed to translate chapter")?;

        // Save translated chapter
        let translated_filename = format!("{} - {}.txt", chapter_num_str, safe_title);
        let translated_path = story_dir.join(&translated_filename);
        std::fs::write(&translated_path, &translated_content)?;

        params
            .console
            .success(&format!("Saved: {}", translated_filename));
    }

    Ok(())
}

/// Runs name scout on chapters that haven't been covered.
/// Returns true if any scouting was performed, false if all chapters were already covered.
async fn run_name_scout(
    console: &Console,
    name_scout: &NameScout,
    name_mapping: &mut NameMappingStore,
    chapters: &[(u32, &str, &str)], // (number, title, content)
) -> Result<bool> {
    console.section("Name Scout Phase");

    let uncovered: Vec<_> = chapters
        .iter()
        .filter(|(num, _, _)| !name_mapping.is_chapter_covered(*num))
        .collect();

    if uncovered.is_empty() {
        console.info("All chapters already scouted for names");
        return Ok(false);
    }

    console.info(&format!(
        "Scouting {} chapters for character names",
        uncovered.len()
    ));

    for (number, title, content) in uncovered {
        console.step(&format!("Scouting chapter {}: {}", number, title));

        let payload = build_chapter_payload(*number, title, content);
        let name_chunks = name_scout
            .collect_names(&payload)
            .await
            .with_context(|| format!("Failed to scout names for chapter {}", number))?;

        let total_names: usize = name_chunks.iter().map(|c| c.len()).sum();
        console.info(&format!(
            "Found {} names in chapter {}",
            total_names, number
        ));

        // Record votes and save
        for entries in name_chunks {
            name_mapping.record_votes(&entries);
            name_mapping.save()?;
        }

        // Mark chapter as covered
        name_mapping.add_coverage(&[*number]);
        name_mapping.save()?;
    }

    console.success(&format!(
        "Name mapping now has {} names",
        name_mapping.len()
    ));

    Ok(true)
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

/// Finds an existing folder or creates a new one with translated title.
async fn find_or_create_folder(
    console: &Console,
    output_dir: &Path,
    module_name: &str,
    novel_id: &str,
    original_title: &str,
    translator: &Translator,
) -> Result<String> {
    // Check for existing folders
    let new_format_prefix = format!("[{}: {}]", module_name, novel_id);
    let old_format_prefix = format!("[{}]", novel_id);

    if let Ok(entries) = std::fs::read_dir(output_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            if entry.path().is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(&new_format_prefix) || name.starts_with(&old_format_prefix) {
                    console.info(&format!("Using existing folder: {}", name));
                    return Ok(name);
                }
            }
        }
    }

    // Create new folder with translated title
    console.step("Translating title for folder name...");
    let translated_title = translator
        .translate(original_title, true, None)
        .await
        .context("Failed to translate title for folder name")?;

    let safe_title = sanitize_filename(&translated_title);
    let folder_name = format!("[{}: {}] {}", module_name, novel_id, safe_title);

    console.success(&format!("Creating folder: {}", folder_name));

    Ok(folder_name)
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
