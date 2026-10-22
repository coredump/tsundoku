//! Configuration management for Tsundoku.
//!
//! Handles loading, saving, and validating configuration from
//! platform-specific config directories.

use crate::error::ConfigError;
use crate::honorifics::HonorificPolicy;
use crate::qa::QaConfig;
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

    /// Post-translation quality checks and QA retries (`[qa]`).
    pub qa: QaConfig,

    /// Keys present in the config file that no setting reads, such as a misspelled
    /// section. Filled by [`Config::load_from`]; never written back to disk.
    #[serde(skip)]
    pub ignored_keys: Vec<String>,
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
            qa: QaConfig::default(),
            ignored_keys: Vec::new(),
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
    /// Maximum characters (not bytes) per translation chunk; chunks end at scene
    /// breaks where possible.
    pub chunk_size_chars: usize,

    /// Number of retry attempts for failed translations.
    pub retries: u32,

    /// Delay between API requests in seconds.
    pub delay_between_requests_sec: f64,

    /// Number of message pairs to retain in conversation history.
    pub history_length: usize,

    /// Maximum number of glossary entries included in one chapter translation prompt.
    ///
    /// Set to zero to disable glossary injection.
    pub glossary_max_entries: usize,

    /// Maximum number of characters listed (with gender and note) in one chapter
    /// translation prompt.
    ///
    /// Set to zero to disable the cast block.
    pub cast_max_entries: usize,

    /// Number of previous chapters whose scout synopses are added to a chapter's
    /// translation prompt as context.
    ///
    /// Set to zero to disable.
    pub summary_chapters: usize,

    /// Reply token limit per source character: each request sends
    /// `max_tokens = max(512, ceil(max_tokens_per_char × characters))`.
    ///
    /// Set to zero to send no `max_tokens`.
    pub max_tokens_per_char: f64,

    /// How honorifics are rendered: `keep`, `drop` or `names-only`. A novel's
    /// `book.json` `honorifics` overrides it.
    pub honorifics: HonorificPolicy,

    /// Number of chapters to translate concurrently.
    pub workers: usize,
}

impl Default for TranslationConfig {
    fn default() -> Self {
        Self {
            chunk_size_chars: 1500,
            retries: 3,
            delay_between_requests_sec: 1.0,
            history_length: 5,
            glossary_max_entries: 100,
            cast_max_entries: 30,
            summary_chapters: 2,
            max_tokens_per_char: 2.0,
            honorifics: HonorificPolicy::default(),
            workers: 1,
        }
    }
}

/// Name scout configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NameScoutConfig {
    /// Maximum characters (not bytes) per name scout chunk; chunks end at scene
    /// breaks where possible.
    pub chunk_size_chars: usize,

    /// Number of retry attempts.
    pub retries: u32,

    /// Delay between requests in seconds.
    pub delay_between_requests_sec: f64,

    /// Timeout for each API request in seconds.
    pub request_timeout_sec: u64,

    /// Number of JSON parsing retry attempts.
    pub json_retries: u32,

    /// Number of chapters to scout concurrently.
    pub workers: usize,

    /// After the mapping scout, summarise every chapter without a stored synopsis
    /// (one request per chapter, in chapter order), stored next to the mapping and
    /// used as cross-chapter context during translation. Off by default because it
    /// adds one scout request per chapter.
    pub summaries: bool,
}

impl Default for NameScoutConfig {
    fn default() -> Self {
        Self {
            chunk_size_chars: 900,
            retries: 3,
            delay_between_requests_sec: 1.0,
            request_timeout_sec: 300,
            json_retries: 3,
            workers: 1,
            summaries: false,
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

    /// Prompt for name and terminology extraction.
    pub name_scout: String,
}

impl Default for PromptsConfig {
    fn default() -> Self {
        Self {
            title_translation: "You are a Japanese to English translator. Translate the following Japanese novel title to English. Provide only the translated title, nothing else.".to_string(),
            content_translation: "You are a Japanese to English translator specializing in web novels. Translate the following Japanese text to natural English, preserving the author's style and tone. Character names have already been converted to English - do not change them.".to_string(),
            name_scout: r#"You read Japanese fiction text and extract character name parts and recurring story-specific terminology.
Treat given and family names separately and report bare names without suffixes such as 家, 王, or 国. Names are personal names only; never list titles, ranks, kinship terms, or common nouns (such as 父, 帝, 将軍, 公爵) as names. For terms, include distinctive items, skills, magic systems, ranks, organizations, places, and fictional concepts; exclude person names, ordinary vocabulary, and prose. Preserve each original exactly and use concise canonical English renderings. No explanations."#.to_string(),
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
        let deserializer =
            toml::Deserializer::parse(&content).map_err(|e| ConfigError::ParseError(e.to_string()))?;
        let mut ignored_keys = Vec::new();
        let mut config: Config = serde_ignored::deserialize(deserializer, |key| {
            ignored_keys.push(key.to_string());
        })
        .map_err(|e| ConfigError::ParseError(e.to_string()))?;
        config.ignored_keys = ignored_keys;

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
        self.validate_for_workflow(true, true)
    }

    /// Validates the configuration with optional name scout requirements.
    pub fn validate_with_options(&self, require_scout_api: bool) -> Result<(), ConfigError> {
        self.validate_for_workflow(true, require_scout_api)
    }

    /// Validates the configuration for a workflow's required APIs.
    pub fn validate_for_workflow(
        &self,
        require_translation_api: bool,
        require_scout_api: bool,
    ) -> Result<(), ConfigError> {
        if require_translation_api && !self.api.is_configured() {
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

        let max_tokens_per_char = self.translation.max_tokens_per_char;
        if !max_tokens_per_char.is_finite() || max_tokens_per_char < 0.0 {
            return Err(ConfigError::InvalidValue {
                key: "translation.max_tokens_per_char".to_string(),
                message: "must be a finite number of at least 0 (0 disables max_tokens)"
                    .to_string(),
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

        self.qa
            .validate()
            .map_err(|message| ConfigError::InvalidValue {
                key: "qa".to_string(),
                message,
            })?;

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
        assert_eq!(config.translation.chunk_size_chars, 1500);
        assert_eq!(config.name_scout.chunk_size_chars, 900);
        assert_eq!(config.translation.workers, 1);
        assert_eq!(config.name_scout.workers, 1);
        assert_eq!(config.name_scout.request_timeout_sec, 300);
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
    fn test_unknown_config_keys_are_reported_and_known_ones_are_not() {
        let file = NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            "[prompts]\ncontent_translation = \"Custom\"\n\n[Prompts]\ncontent_translation_prompt = \"Lost\"\n\n[translation]\nworkres = 4\n",
        )
        .unwrap();

        let loaded = Config::load_from(file.path()).unwrap();

        assert_eq!(loaded.prompts.content_translation, "Custom");
        assert_eq!(loaded.ignored_keys, ["Prompts", "translation.workres"]);
    }

    #[test]
    fn test_saved_default_config_has_no_unknown_keys() {
        let file = NamedTempFile::new().unwrap();
        Config::default().save_to(file.path()).unwrap();

        assert!(Config::load_from(file.path()).unwrap().ignored_keys.is_empty());
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
    fn test_max_tokens_per_char_validation() {
        let mut config = Config::default();
        config.api.key = "real-key".to_string();
        assert_eq!(config.translation.max_tokens_per_char, 2.0);

        config.translation.max_tokens_per_char = 0.0;
        assert!(config.validate_with_options(false).is_ok());

        for invalid in [-1.0, f64::NAN, f64::INFINITY] {
            config.translation.max_tokens_per_char = invalid;
            assert!(config.validate_with_options(false).is_err(), "{invalid}");
        }
    }

    #[test]
    fn test_qa_section_loads_and_is_validated() {
        let file = NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            "[api]\nkey = \"real-key\"\n\n[qa]\nmax_retries = 2\nretry_on = [\"glossary\"]\nlength_ratio = [3.0, 2.0]\n",
        )
        .unwrap();

        let mut config = Config::load_from(file.path()).unwrap();

        assert!(config.ignored_keys.is_empty(), "{:?}", config.ignored_keys);
        assert_eq!(config.qa.max_retries, 2);
        assert_eq!(config.qa.retry_on, [crate::qa::IssueClass::Glossary]);
        assert_eq!(
            config.qa.paragraph_ratio,
            QaConfig::default().paragraph_ratio
        );
        assert!(config.validate_with_options(false).is_err());

        config.qa.length_ratio = [1.2, 4.0];
        assert!(config.validate_with_options(false).is_ok());
    }

    #[test]
    fn test_honorifics_setting_loads_and_defaults_to_keep() {
        assert_eq!(
            Config::default().translation.honorifics,
            HonorificPolicy::Keep
        );

        let file = NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "[translation]\nhonorifics = \"names-only\"\n").unwrap();
        let config = Config::load_from(file.path()).unwrap();

        assert!(config.ignored_keys.is_empty(), "{:?}", config.ignored_keys);
        assert_eq!(config.translation.honorifics, HonorificPolicy::NamesOnly);

        std::fs::write(file.path(), "[translation]\nhonorifics = \"polite\"\n").unwrap();
        assert!(Config::load_from(file.path()).is_err());
    }

    #[test]
    fn test_scout_only_validation_does_not_require_translation_api() {
        let mut config = Config::default();
        config.scout_api.as_mut().unwrap().key = "scout-key".to_string();

        assert!(config.validate_for_workflow(false, true).is_ok());
    }

    #[test]
    fn test_scout_api_required() {
        let config = Config::default();
        assert!(config.scout_api_config().is_err());
    }
}
