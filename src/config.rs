//! Configuration management for Tsundoku.
//!
//! Handles loading, saving, and validating configuration from
//! platform-specific config directories.

use crate::error::ConfigError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Application name used for config directory.
const APP_NAME: &str = "Tsundoku";

/// Default config filename.
const CONFIG_FILENAME: &str = "config.toml";

/// Placeholder value for unconfigured API keys.
const API_KEY_PLACEHOLDER: &str = "YOUR_API_KEY_HERE";

/// Main configuration structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Main translation API configuration.
    pub api: ApiConfig,

    /// Separate API for name scouting.
    pub scout_api: Option<ApiConfig>,

    /// Translation behavior settings.
    pub translation: TranslationConfig,

    /// Name scout settings.
    pub name_scout: NameScoutConfig,

    /// Web scraping settings.
    pub scraping: ScrapingConfig,

    /// LLM prompts.
    pub prompts: PromptsConfig,

    /// File paths.
    pub paths: PathsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api: ApiConfig::default(),
            scout_api: Some(ApiConfig::default()),
            translation: TranslationConfig::default(),
            name_scout: NameScoutConfig::default(),
            scraping: ScrapingConfig::default(),
            prompts: PromptsConfig::default(),
            paths: PathsConfig::default(),
        }
    }
}

/// API configuration for LLM endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    /// API key (required).
    pub key: String,

    /// Base URL for the API.
    pub base_url: String,

    /// Model identifier.
    pub model: String,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            key: API_KEY_PLACEHOLDER.to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4o-mini".to_string(),
        }
    }
}

impl ApiConfig {
    /// Checks if the API key is configured (not placeholder).
    pub fn is_configured(&self) -> bool {
        !self.key.is_empty() && self.key != API_KEY_PLACEHOLDER
    }
}

/// Translation behavior configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TranslationConfig {
    /// Maximum characters per translation chunk.
    pub chunk_size_chars: usize,

    /// Number of retry attempts for failed translations.
    pub retries: u32,

    /// Delay between API requests in seconds.
    pub delay_between_requests_sec: f64,

    /// Number of message pairs to retain in conversation history.
    pub history_length: usize,

    /// Number of chapters to translate concurrently.
    pub workers: usize,
}

impl Default for TranslationConfig {
    fn default() -> Self {
        Self {
            chunk_size_chars: 4000,
            retries: 3,
            delay_between_requests_sec: 1.0,
            history_length: 5,
            workers: 1,
        }
    }
}

/// Name scout configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NameScoutConfig {
    /// Maximum characters per name scout chunk.
    pub chunk_size_chars: usize,

    /// Number of retry attempts.
    pub retries: u32,

    /// Delay between requests in seconds.
    pub delay_between_requests_sec: f64,

    /// Number of JSON parsing retry attempts.
    pub json_retries: u32,

    /// Number of chapters to scout concurrently.
    pub workers: usize,
}

impl Default for NameScoutConfig {
    fn default() -> Self {
        Self {
            chunk_size_chars: 2500,
            retries: 3,
            delay_between_requests_sec: 1.0,
            json_retries: 3,
            workers: 1,
        }
    }
}

/// Web scraping configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScrapingConfig {
    /// Delay between web requests in seconds.
    pub delay_between_requests_sec: f64,
    /// Enable scraper debug logging.
    pub debug: bool,
}

impl Default for ScrapingConfig {
    fn default() -> Self {
        Self {
            delay_between_requests_sec: 1.0,
            debug: false,
        }
    }
}

/// LLM system prompts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptsConfig {
    /// Prompt for title translation.
    pub title_translation: String,

    /// Prompt for content translation.
    pub content_translation: String,

    /// Prompt for name extraction.
    pub name_scout: String,
}

impl Default for PromptsConfig {
    fn default() -> Self {
        Self {
            title_translation: "You are a Japanese to English translator. Translate the following Japanese novel title to English. Provide only the translated title, nothing else.".to_string(),
            content_translation: "You are a Japanese to English translator specializing in web novels. Translate the following Japanese text to natural English, preserving the author's style and tone. Character names have already been converted to English - do not change them.".to_string(),
            name_scout: r#"You read Japanese fiction text and extract character name parts.
Return ONLY JSON with this shape:
{"names":[{"original":"<exact name characters>","part":"family|given|unknown","english":"<best English rendering>"}]}
Treat given and family names separately. Use romaji or common English equivalents. No explanations."#.to_string(),
        }
    }
}

/// File path configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PathsConfig {
    /// Directory for translated novels.
    pub output_directory: PathBuf,

    /// Directory for name mapping cache.
    pub names_directory: Option<PathBuf>,

    /// Command to open editor for reviewing name mappings.
    /// If not set, will try to auto-detect a suitable editor.
    /// Examples: "kate", "vim", "nano", "code", "notepad"
    pub editor_command: Option<String>,
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            output_directory: PathBuf::from("."),
            names_directory: None,
            editor_command: None,
        }
    }
}

impl Config {
    /// Returns the platform-specific config directory path.
    pub fn config_dir() -> Result<PathBuf, ConfigError> {
        dirs::config_dir()
            .map(|p| p.join(APP_NAME))
            .ok_or(ConfigError::NoConfigDir)
    }

    /// Returns the full path to the config file.
    pub fn config_path() -> Result<PathBuf, ConfigError> {
        Ok(Self::config_dir()?.join(CONFIG_FILENAME))
    }

    /// Loads configuration from the default location.
    ///
    /// If the config file doesn't exist, creates a default one.
    pub fn load() -> Result<Self, ConfigError> {
        let path = Self::config_path()?;
        Self::load_from(&path)
    }

    /// Loads configuration from a specific path.
    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            // Create default config
            let config = Config::default();
            config.save_to(path)?;
            return Ok(config);
        }

        let content = std::fs::read_to_string(path)?;
        let config: Config =
            toml::from_str(&content).map_err(|e| ConfigError::ParseError(e.to_string()))?;

        Ok(config)
    }

    /// Saves configuration to the default location.
    pub fn save(&self) -> Result<(), ConfigError> {
        let path = Self::config_path()?;
        self.save_to(&path)
    }

    /// Saves configuration to a specific path.
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content =
            toml::to_string_pretty(self).map_err(|e| ConfigError::ParseError(e.to_string()))?;

        std::fs::write(path, content)?;
        Ok(())
    }

    /// Validates the configuration.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_with_options(true)
    }

    /// Validates the configuration with optional name scout requirements.
    pub fn validate_with_options(&self, require_scout_api: bool) -> Result<(), ConfigError> {
        if !self.api.is_configured() {
            return Err(ConfigError::MissingValue(
                "api.key (set your API key in config file)".to_string(),
            ));
        }

        if require_scout_api {
            match self.scout_api.as_ref().filter(|api| api.is_configured()) {
                Some(_) => {}
                None => {
                    return Err(ConfigError::MissingValue(
                        "scout_api.key (set your name scout API key in config file)".to_string(),
                    ));
                }
            }
        }

        if self.translation.chunk_size_chars == 0 {
            return Err(ConfigError::InvalidValue {
                key: "translation.chunk_size_chars".to_string(),
                message: "must be greater than 0".to_string(),
            });
        }

        if self.translation.workers == 0 {
            return Err(ConfigError::InvalidValue {
                key: "translation.workers".to_string(),
                message: "must be greater than 0".to_string(),
            });
        }

        if self.name_scout.chunk_size_chars == 0 {
            return Err(ConfigError::InvalidValue {
                key: "name_scout.chunk_size_chars".to_string(),
                message: "must be greater than 0".to_string(),
            });
        }

        if self.name_scout.workers == 0 {
            return Err(ConfigError::InvalidValue {
                key: "name_scout.workers".to_string(),
                message: "must be greater than 0".to_string(),
            });
        }

        Ok(())
    }

    /// Returns the effective names directory, using config or default.
    pub fn names_dir(&self) -> Result<PathBuf, ConfigError> {
        if let Some(ref dir) = self.paths.names_directory {
            Ok(dir.clone())
        } else {
            Ok(Self::config_dir()?.join("names"))
        }
    }

    /// Returns the API config to use for name scouting.
    pub fn scout_api_config(&self) -> Result<&ApiConfig, ConfigError> {
        self.scout_api
            .as_ref()
            .filter(|api| api.is_configured())
            .ok_or_else(|| {
                ConfigError::MissingValue(
                    "scout_api.key (set your name scout API key in config file)".to_string(),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert!(!config.api.is_configured());
        assert!(config.scout_api.is_some());
        assert!(!config.scout_api.as_ref().unwrap().is_configured());
        assert_eq!(config.translation.chunk_size_chars, 4000);
        assert_eq!(config.translation.workers, 1);
        assert_eq!(config.name_scout.workers, 1);
        assert_eq!(config.scraping.delay_between_requests_sec, 1.0);
    }

    #[test]
    fn test_api_configured_check() {
        let mut api = ApiConfig::default();
        assert!(!api.is_configured());

        api.key = "sk-real-key".to_string();
        assert!(api.is_configured());
    }

    #[test]
    fn test_config_round_trip() {
        let config = Config::default();
        let file = NamedTempFile::new().unwrap();

        config.save_to(file.path()).unwrap();

        let loaded = Config::load_from(file.path()).unwrap();
        assert_eq!(loaded.api.model, config.api.model);
        assert_eq!(
            loaded.translation.chunk_size_chars,
            config.translation.chunk_size_chars
        );
    }

    #[test]
    fn test_config_validation() {
        let config = Config::default();
        assert!(config.validate().is_err()); // API key not set

        let mut config = Config::default();
        config.api.key = "real-key".to_string();
        config.scout_api.as_mut().unwrap().key = "scout-key".to_string();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_validation_without_scout() {
        let mut config = Config::default();
        config.api.key = "real-key".to_string();
        assert!(config.validate_with_options(false).is_ok());
    }

    #[test]
    fn test_scout_api_required() {
        let config = Config::default();
        assert!(config.scout_api_config().is_err());
    }
}
