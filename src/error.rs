//! Error types for the Tsundoku application.
//!
//! Uses `thiserror` for structured error definitions that provide
//! clear context about what went wrong.

use thiserror::Error;

/// Main error type for scraping operations.
#[derive(Error, Debug)]
pub enum ScraperError {
    /// HTTP request failed
    #[error("HTTP request failed: {0}")]
    HttpError(#[from] reqwest::Error),

    /// Failed to parse HTML content
    #[error("Failed to parse HTML: {0}")]
    ParseError(String),

    /// The required element isn't found in HTML
    #[error("Element not found: {0}")]
    ElementNotFound(String),

    /// URL parsing or validation failed
    #[error("Invalid URL: {0}")]
    InvalidUrl(String),

    /// Novel or chapter not found
    #[error("Not found: {0}")]
    NotFound(String),

    /// Rate limit exceeded or server returned 429
    #[error("Rate limited: {0}")]
    RateLimited(String),

    /// The scraper doesn't support this URL
    #[error("Unsupported URL: {0}")]
    UnsupportedUrl(String),
}

/// Error type for configuration operations.
#[derive(Error, Debug)]
pub enum ConfigError {
    /// Failed to read config file
    #[error("Failed to read config file: {0}")]
    ReadError(#[from] std::io::Error),

    /// Failed to parse config file
    #[error("Failed to parse config: {0}")]
    ParseError(String),

    /// Missing required configuration value
    #[error("Missing required config value: {0}")]
    MissingValue(String),

    /// Invalid configuration value
    #[error("Invalid config value for '{key}': {message}")]
    InvalidValue { key: String, message: String },

    /// Config directory not found
    #[error("Could not determine config directory")]
    NoConfigDir,
}

/// Error type for translation operations.
#[derive(Error, Debug)]
pub enum TranslationError {
    /// HTTP request to API failed
    #[error("API request failed: {0}")]
    HttpError(#[from] reqwest::Error),

    /// API returned an error response
    #[error("API error: {0}")]
    ApiError(String),

    /// Failed to parse API response
    #[error("Failed to parse API response: {0}")]
    ParseError(String),

    /// Translation was refused by the model
    #[error("Translation refused: {0}")]
    Refused(String),

    /// All retry attempts exhausted
    #[error("All retries exhausted after {attempts} attempts")]
    RetriesExhausted { attempts: u32 },

    /// Invalid API configuration
    #[error("Invalid API configuration: {0}")]
    InvalidConfig(String),
}

/// Error type for name mapping operations.
#[derive(Error, Debug)]
pub enum NameMappingError {
    /// Failed to read mapping file
    #[error("Failed to read name mapping: {0}")]
    ReadError(#[from] std::io::Error),

    /// Failed to parse JSON
    #[error("Failed to parse name mapping JSON: {0}")]
    ParseError(#[from] serde_json::Error),

    /// Invalid mapping data structure
    #[error("Invalid name mapping structure: {0}")]
    InvalidStructure(String),

    /// Failed to write mapping file
    #[error("Failed to save name mapping: {0}")]
    WriteError(String),
}

/// Error type for the per-novel chapter summary file.
#[derive(Error, Debug)]
pub enum SummaryError {
    /// Failed to read the summary file.
    #[error("Failed to read chapter summaries: {0}")]
    ReadError(#[from] std::io::Error),

    /// Failed to parse the summary JSON.
    #[error("Failed to parse chapter summaries JSON: {0}")]
    ParseError(#[from] serde_json::Error),

    /// The file was written by a newer, incompatible version.
    #[error("Unsupported chapter summaries version {0} (expected {expected})", expected = crate::summaries::SUMMARIES_VERSION)]
    UnsupportedVersion(u32),

    /// Failed to write the summary file.
    #[error("Failed to save chapter summaries: {0}")]
    WriteError(String),
}

/// Error type for `book.json` metadata.
#[derive(Error, Debug)]
pub enum BookMetadataError {
    /// The metadata file exists but could not be read.
    #[error("Failed to read {}: {source}", path.display())]
    Read {
        /// Path of `book.json`.
        path: std::path::PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },

    /// The metadata file is not valid JSON for this format.
    #[error("Invalid {}: {source}", path.display())]
    Parse {
        /// Path of `book.json`.
        path: std::path::PathBuf,
        /// Underlying JSON error.
        source: serde_json::Error,
    },

    /// The metadata file could not be written.
    #[error("Failed to write {}: {source}", path.display())]
    Write {
        /// Path of `book.json`.
        path: std::path::PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },

    /// A `replacements` entry in the metadata file is invalid.
    #[error("Invalid {}: {source}", path.display())]
    Replacement {
        /// Path of `book.json`.
        path: std::path::PathBuf,
        /// The offending entry.
        source: ReplacementError,
    },
}

/// Error type for a `book.json` replacement entry that does not compile.
#[derive(Error, Debug)]
#[error("replacements[{index}] ({find:?}) is not a valid regex: {source}")]
pub struct ReplacementError {
    /// Zero-based index of the entry in `replacements`.
    pub index: usize,
    /// The entry's `find` pattern.
    pub find: String,
    /// Underlying regex error.
    pub source: regex::Error,
}

/// Error type for EPUB generation.
#[derive(Error, Debug)]
pub enum EpubError {
    /// Reading the novel folder or writing the output failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A chapter file could not be read.
    #[error("Failed to read chapter {}: {source}", path.display())]
    ReadChapter {
        /// Path of the chapter file.
        path: std::path::PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },

    /// The EPUB archive could not be written.
    #[error("Failed to write EPUB archive: {0}")]
    Zip(#[from] zip::result::ZipError),
}

/// Error type for per-chapter QA records (`.tsundoku/chunks/{chapter}.json`).
#[derive(Error, Debug)]
pub enum ChapterRecordError {
    /// The record could not be read, written, or its folder created.
    #[error("I/O error on {}: {source}", path.display())]
    Io {
        /// Path of the record (or its folder).
        path: std::path::PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },

    /// The record is not valid JSON for this format, or could not be serialized.
    #[error("Invalid chapter record {}: {source}", path.display())]
    Json {
        /// Path of the record.
        path: std::path::PathBuf,
        /// Underlying JSON error.
        source: serde_json::Error,
    },

    /// The record was written by a newer (or unknown) format version.
    #[error("Unsupported chapter record version {version} in {}", path.display())]
    UnsupportedVersion {
        /// Path of the record.
        path: std::path::PathBuf,
        /// Version found in the file.
        version: u32,
    },
}

/// Result type alias using anyhow for application-level error handling.
pub type Result<T> = anyhow::Result<T>;
