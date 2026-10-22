//! Tsundoku - Japanese web novel downloader and translator.
//!
//! This library provides functionality for:
//! - Scraping novels from Japanese web novel platforms (Syosetu, Kakuyomu, Pixiv)
//! - Extracting and managing character name mappings
//! - Translating content using OpenAI-compatible APIs
//! - Building EPUB volumes from translated chapters

pub mod book;
pub mod config;
pub mod console;
mod cookies;
pub mod epub;
pub mod error;
pub mod honorifics;
pub mod name_mapping;
pub mod name_scout;
pub mod output_rules;
pub mod ruby;
pub mod scrapers;
pub mod translator;
pub mod utils;

// Re-export commonly used types
pub use config::Config;
pub use console::Console;
pub use error::{
    BookMetadataError, ConfigError, EpubError, NameMappingError, ScraperError, TranslationError,
};
pub use name_mapping::{NameEntry, NameMappingStore, NamePart, TermEntry};
pub use name_scout::NameScout;
pub use scrapers::{ChapterInfo, ChapterList, NovelInfo, Scraper, ScraperRegistry};
pub use translator::{ProgressInfo, Translator};
