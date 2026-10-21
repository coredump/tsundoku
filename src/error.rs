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

    /// The translation still contains Chinese or Japanese characters.
    #[error("Translation contains {count} Chinese or Japanese characters")]
    ForeignScript {
        /// The translated text as returned by the model.
        text: String,
        /// Number of kana and CJK ideographs found.
        count: usize,
    },

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

/// Result type alias using anyhow for application-level error handling.
pub type Result<T> = anyhow::Result<T>;
