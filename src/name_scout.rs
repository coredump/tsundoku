//! Name Scout - Extract character names from Japanese text using LLM.
//!
//! Uses a secondary LLM pass to identify character names in Japanese text
//! and extract them with English translations.

use crate::config::{ApiConfig, NameScoutConfig};
use crate::console::Console;
use crate::error::TranslationError;
use crate::name_mapping::{NameEntry, NamePart};
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

/// Regex to extract JSON from markdown code fences.
static CODE_FENCE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)^```[a-zA-Z]*\s*(.*?)\s*```$").expect("Invalid CODE_FENCE_REGEX")
});

/// Refusal phrases that indicate the model declined to process.
static REFUSAL_PHRASES: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    vec![
        "i'm sorry",
        "i cannot",
        "i am unable",
        "as an ai",
        "my apologies",
        "i am not programmed",
        "i do not have the ability",
    ]
});

fn format_translation_error(error: &TranslationError) -> String {
    match error {
        TranslationError::HttpError(error) => describe_request_error(error),
        _ => error.to_string(),
    }
}

fn describe_request_error(error: &reqwest::Error) -> String {
    let mut details = Vec::new();

    if error.is_timeout() {
        details.push("timeout".to_string());
    }
    if error.is_connect() {
        details.push("connect".to_string());
    }
    if error.is_request() {
        details.push("request".to_string());
    }
    if error.is_body() {
        details.push("body".to_string());
    }
    if let Some(status) = error.status() {
        details.push(format!("status={}", status));
    }

    let mut message = if details.is_empty() {
        error.to_string()
    } else {
        format!("{} [{}]", error, details.join(", "))
    };

    if let Some(url) = error.url() {
        message.push_str(&format!("; url={}", url));
    }

    let mut sources = Vec::new();
    let mut source = error.source();
    while let Some(current) = source {
        sources.push(current.to_string());
        source = current.source();
    }

    if !sources.is_empty() {
        message.push_str(&format!("; caused by: {}", sources.join(" -> ")));
    }

    message
}

fn preview_text(text: &str, max_chars: usize) -> String {
    let mut preview = text
        .chars()
        .take(max_chars)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>();

    if text.chars().count() > max_chars {
        preview.push_str("...");
    }

    preview
}

/// Request body for the chat completions API.
#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
}

/// A message in the conversation.
#[derive(Debug, Serialize)]
struct Message {
    role: String,
    content: String,
}

/// Response from the chat completions API.
#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

/// A single choice in the response.
#[derive(Debug, Deserialize)]
struct Choice {
    message: Option<ResponseMessage>,
    text: Option<String>,
    finish_reason: Option<String>,
}

/// Message content in a response.
#[derive(Debug, Deserialize)]
struct ResponseMessage {
    content: Option<String>,
}

/// Parsed name entry from LLM response.
#[derive(Debug, Deserialize)]
struct ParsedNameEntry {
    original: Option<String>,
    english: Option<String>,
    part: Option<String>,
}

/// Parsed names response from LLM.
#[derive(Debug, Deserialize)]
struct ParsedNamesResponse {
    names: Vec<ParsedNameEntry>,
}

/// Name Scout for extracting character names from Japanese text.
pub struct NameScout {
    /// HTTP client for API requests.
    client: Client,
    /// API configuration.
    api_config: ApiConfig,
    /// Name scout behavior configuration.
    scout_config: NameScoutConfig,
    /// System prompt for name extraction.
    prompt: String,
    /// Console for output.
    console: Console,
}

/// Progress information for a name scout worker.
#[derive(Debug, Clone)]
pub struct ScoutProgressInfo {
    /// Current chapter number.
    pub chapter: u32,
    /// Worker slot currently scouting this chapter.
    pub worker: usize,
    /// Optional channel for centralized progress reporting.
    pub reporter: Option<UnboundedSender<NameScoutEvent>>,
}

/// Current state of a name scout worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameScoutStatus {
    /// The worker is preparing the chunk before sending it.
    Scouting,
    /// The worker has sent the prompt and is waiting for model processing.
    Waiting,
    /// The worker is retrying after an API, refusal, or JSON parse failure.
    Retrying,
    /// The worker is parsing a model response.
    Parsing,
}

/// Progress event emitted while name scouting is running.
#[derive(Debug, Clone)]
pub struct NameScoutProgress {
    /// Worker slot emitting this update.
    pub worker: usize,
    /// Chapter currently being scouted.
    pub chapter: u32,
    /// Current chunk number.
    pub chunk: u32,
    /// Total chunk count for the chapter.
    pub total_chunks: u32,
    /// Characters in the current chunk.
    pub chars: usize,
    /// Seconds elapsed since the current chunk started.
    pub elapsed_secs: f64,
    /// Current worker status.
    pub status: NameScoutStatus,
}

/// Permanent name scout message to print above the live statusline.
#[derive(Debug, Clone)]
pub struct NameScoutNotice {
    /// Chapter currently being scouted.
    pub chapter: u32,
    /// Current chunk number.
    pub chunk: u32,
    /// Message text.
    pub message: String,
    /// True for fatal errors, false for retryable warnings.
    pub error: bool,
}

/// Event emitted while name scouting is running.
#[derive(Debug, Clone)]
pub enum NameScoutEvent {
    /// Live statusline progress.
    Progress(NameScoutProgress),
    /// Permanent warning or error output.
    Notice(NameScoutNotice),
}

impl NameScout {
    /// Create a new NameScout.
    pub fn new(api_config: ApiConfig, scout_config: NameScoutConfig, prompt: String) -> Self {
        Self {
            client: Client::new(),
            api_config,
            scout_config,
            prompt,
            console: Console::new(),
        }
    }

    /// Collect names from text, processing in chunks.
    ///
    /// Returns a vector of name entry vectors, one per successfully processed chunk.
    pub async fn collect_names(&self, text: &str) -> Result<Vec<Vec<NameEntry>>, TranslationError> {
        self.collect_names_with_progress(text, None).await
    }

    /// Collect names from text, reporting chunk progress when a reporter is provided.
    pub async fn collect_names_with_progress(
        &self,
        text: &str,
        progress_info: Option<ScoutProgressInfo>,
    ) -> Result<Vec<Vec<NameEntry>>, TranslationError> {
        let chunks = self.split_into_chunks(text);
        let total_chunks = chunks.len();
        let mut results = Vec::new();

        for (i, chunk) in chunks.iter().enumerate() {
            let chunk_num = i + 1;
            let chunk_started = Instant::now();
            self.report_progress(
                chunk_num,
                total_chunks,
                chunk.len(),
                chunk_started,
                NameScoutStatus::Scouting,
                progress_info.as_ref(),
            );

            if progress_info.is_none() {
                self.console.info(&format!(
                    "Name scout chunk {}/{} ({} chars)",
                    chunk_num,
                    total_chunks,
                    chunk.len()
                ));
            }

            // Refusal/JSON retry loop for this chunk.
            let mut attempt = 0;
            let mut success = false;

            while attempt < self.scout_config.json_retries && !success {
                // API failures use the dedicated retry budget and abort the phase when exhausted.
                let mut api_attempt = 0;
                let raw_response = loop {
                    self.report_progress(
                        chunk_num,
                        total_chunks,
                        chunk.len(),
                        chunk_started,
                        NameScoutStatus::Waiting,
                        progress_info.as_ref(),
                    );

                    match self.call_model(chunk).await {
                        Ok(raw_response) => break raw_response,
                        Err(e) => {
                            api_attempt += 1;
                            if api_attempt >= self.scout_config.retries {
                                self.report_notice(
                                    chunk_num,
                                    true,
                                    format!(
                                        "Name scout API failed for chapter {}, chunk {} after {} attempts",
                                        progress_info.as_ref().map_or(0, |info| info.chapter),
                                        chunk_num,
                                        self.scout_config.retries
                                    ),
                                    progress_info.as_ref(),
                                );
                                if progress_info.is_none() {
                                    self.console.error(&format!(
                                        "Name scout API failed for chunk {} after {} attempts",
                                        chunk_num, self.scout_config.retries
                                    ));
                                }
                                return Err(TranslationError::RetriesExhausted {
                                    attempts: self.scout_config.retries,
                                });
                            }

                            let delay = Duration::from_secs(2u64.pow(api_attempt));
                            self.report_progress(
                                chunk_num,
                                total_chunks,
                                chunk.len(),
                                chunk_started,
                                NameScoutStatus::Retrying,
                                progress_info.as_ref(),
                            );
                            self.report_notice(
                                chunk_num,
                                false,
                                format!(
                                    "API error for chapter {}, chunk {}: {}; retrying in {:?} (attempt {}/{})",
                                    progress_info.as_ref().map_or(0, |info| info.chapter),
                                    chunk_num,
                                    format_translation_error(&e),
                                    delay,
                                    api_attempt + 1,
                                    self.scout_config.retries
                                ),
                                progress_info.as_ref(),
                            );
                            if progress_info.is_none() {
                                self.console.warning(&format!(
                                    "API error for chunk {}: {}, retrying in {:?} (attempt {}/{})",
                                    chunk_num,
                                    e,
                                    delay,
                                    api_attempt + 1,
                                    self.scout_config.retries
                                ));
                            }
                            tokio::time::sleep(delay).await;
                            self.report_progress(
                                chunk_num,
                                total_chunks,
                                chunk.len(),
                                chunk_started,
                                NameScoutStatus::Scouting,
                                progress_info.as_ref(),
                            );
                        }
                    }
                };

                // Check for refusal
                let lower = raw_response.to_lowercase();
                if REFUSAL_PHRASES.iter().any(|p| lower.starts_with(p)) {
                    self.report_progress(
                        chunk_num,
                        total_chunks,
                        chunk.len(),
                        chunk_started,
                        NameScoutStatus::Retrying,
                        progress_info.as_ref(),
                    );
                    self.report_notice(
                        chunk_num,
                        false,
                        format!(
                            "Model refused chapter {}, chunk {}; retrying",
                            progress_info.as_ref().map_or(0, |info| info.chapter),
                            chunk_num
                        ),
                        progress_info.as_ref(),
                    );
                    if progress_info.is_none() {
                        self.console.warning(&format!(
                            "Model refused to process chunk {}, retrying...",
                            chunk_num
                        ));
                    }
                    attempt += 1;
                    tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
                    continue;
                }

                self.report_progress(
                    chunk_num,
                    total_chunks,
                    chunk.len(),
                    chunk_started,
                    NameScoutStatus::Parsing,
                    progress_info.as_ref(),
                );

                // Parse the response
                match self.parse_response(&raw_response) {
                    Ok(entries) => {
                        if !entries.is_empty() {
                            if progress_info.is_none() {
                                self.console.success(&format!(
                                    "Found {} names in chunk {}",
                                    entries.len(),
                                    chunk_num
                                ));
                            }
                            results.push(entries);
                        }
                        success = true;
                    }
                    Err(e) => {
                        self.report_progress(
                            chunk_num,
                            total_chunks,
                            chunk.len(),
                            chunk_started,
                            NameScoutStatus::Retrying,
                            progress_info.as_ref(),
                        );
                        self.report_notice(
                            chunk_num,
                            false,
                            format!(
                                "Failed to parse JSON from chapter {}, chunk {}: {}; response preview: {}; retrying",
                                progress_info.as_ref().map_or(0, |info| info.chapter),
                                chunk_num,
                                e,
                                preview_text(&raw_response, 1200)
                            ),
                            progress_info.as_ref(),
                        );
                        if progress_info.is_none() {
                            self.console.warning(&format!(
                                "Failed to parse JSON from chunk {}: {}, retrying...",
                                chunk_num, e
                            ));
                        }
                        attempt += 1;
                        tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
                    }
                }
            }

            if !success {
                self.report_notice(
                    chunk_num,
                    true,
                    format!(
                        "Failed to process chapter {}, chunk {} after {} attempts",
                        progress_info.as_ref().map_or(0, |info| info.chapter),
                        chunk_num,
                        self.scout_config.json_retries
                    ),
                    progress_info.as_ref(),
                );
                if progress_info.is_none() {
                    self.console.error(&format!(
                        "Failed to process chunk {} after {} attempts",
                        chunk_num, self.scout_config.json_retries
                    ));
                }
            }
        }

        Ok(results)
    }

    fn report_progress(
        &self,
        chunk: usize,
        total_chunks: usize,
        chars: usize,
        started: Instant,
        status: NameScoutStatus,
        progress_info: Option<&ScoutProgressInfo>,
    ) {
        if let Some(info) = progress_info
            && let Some(reporter) = info.reporter.as_ref()
        {
            let _ = reporter.send(NameScoutEvent::Progress(NameScoutProgress {
                worker: info.worker,
                chapter: info.chapter,
                chunk: chunk as u32,
                total_chunks: total_chunks as u32,
                chars,
                elapsed_secs: started.elapsed().as_secs_f64(),
                status,
            }));
        }
    }

    fn report_notice(
        &self,
        chunk: usize,
        error: bool,
        message: String,
        progress_info: Option<&ScoutProgressInfo>,
    ) {
        if let Some(info) = progress_info
            && let Some(reporter) = info.reporter.as_ref()
        {
            let _ = reporter.send(NameScoutEvent::Notice(NameScoutNotice {
                chapter: info.chapter,
                chunk: chunk as u32,
                message,
                error,
            }));
        }
    }

    /// Split text into chunks for processing.
    fn split_into_chunks(&self, text: &str) -> Vec<String> {
        let chunk_size = self.scout_config.chunk_size_chars;
        crate::utils::split_text_into_line_chunks(text, chunk_size)
    }

    /// Call the LLM model to extract names.
    async fn call_model(&self, chunk: &str) -> Result<String, TranslationError> {
        let request = ChatRequest {
            model: self.api_config.model.clone(),
            messages: vec![
                Message {
                    role: "system".to_string(),
                    content: self.prompt.clone(),
                },
                Message {
                    role: "user".to_string(),
                    content: chunk.to_string(),
                },
            ],
        };

        // Apply rate limiting delay
        if self.scout_config.delay_between_requests_sec > 0.0 {
            tokio::time::sleep(Duration::from_secs_f64(
                self.scout_config.delay_between_requests_sec,
            ))
            .await;
        }

        let url = format!("{}/chat/completions", self.api_config.base_url);
        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_config.key))
            .header("Content-Type", "application/json")
            .timeout(Duration::from_secs(self.scout_config.request_timeout_sec))
            .json(&request)
            .send()
            .await?;

        let response = crate::utils::check_response_status(response).await?;
        let response_text = response.text().await.map_err(TranslationError::HttpError)?;

        let response_body: ChatResponse = serde_json::from_str(&response_text).map_err(|e| {
            TranslationError::ParseError(format!(
                "Failed to parse API response JSON: {}; response preview: {}",
                e,
                preview_text(&response_text, 1200)
            ))
        })?;

        if response_body.choices.is_empty() {
            return Err(TranslationError::ParseError(format!(
                "No choices in API response; response preview: {}",
                preview_text(&response_text, 1200)
            )));
        }

        let choice = &response_body.choices[0];
        let content = choice
            .message
            .as_ref()
            .and_then(|message| message.content.as_deref())
            .or(choice.text.as_deref())
            .ok_or_else(|| {
                TranslationError::ParseError(format!(
                    "No message.content or text in first API choice; finish_reason={:?}; response preview: {}",
                    choice.finish_reason,
                    preview_text(&response_text, 1200)
                ))
            })?;

        let content = content.trim();
        if content.is_empty() {
            return Err(TranslationError::ParseError(format!(
                "API choice content was empty; finish_reason={:?}; response preview: {}",
                choice.finish_reason,
                preview_text(&response_text, 1200)
            )));
        }

        Ok(content.to_string())
    }

    /// Parse the LLM response into name entries.
    fn parse_response(&self, raw: &str) -> Result<Vec<NameEntry>, TranslationError> {
        let trimmed = raw.trim();

        // Remove markdown code fence if present
        let json_str = if trimmed.starts_with("```") {
            if let Some(captures) = CODE_FENCE_REGEX.captures(trimmed) {
                captures.get(1).map(|m| m.as_str()).unwrap_or(trimmed)
            } else {
                // Try to manually strip
                let without_start = trimmed
                    .trim_start_matches("```json")
                    .trim_start_matches("```");
                without_start.trim_end_matches("```").trim()
            }
        } else {
            trimmed
        };

        // Find JSON object boundaries
        let start = json_str.find('{');
        let end = json_str.rfind('}');

        let json_content = match (start, end) {
            (Some(s), Some(e)) if s < e => &json_str[s..=e],
            _ => {
                return Err(TranslationError::ParseError(
                    "No valid JSON object found".to_string(),
                ));
            }
        };

        // Parse JSON
        let parsed: ParsedNamesResponse = serde_json::from_str(json_content)
            .map_err(|e| TranslationError::ParseError(format!("JSON parse error: {}", e)))?;

        // Convert to NameEntry
        let entries: Vec<NameEntry> = parsed
            .names
            .into_iter()
            .filter_map(|entry| {
                let original = entry.original?.trim().to_string();
                let english = entry.english?.trim().to_string();

                if original.is_empty() || english.is_empty() {
                    return None;
                }

                let part = entry
                    .part
                    .as_deref()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(NamePart::Unknown);

                Some(NameEntry {
                    original,
                    english,
                    part,
                })
            })
            .collect();

        Ok(entries)
    }
}

/// Build a chapter payload for name scouting.
///
/// Format:
/// ```text
/// ### Chapter {number} - {title}
/// {content}
/// ```
pub fn build_chapter_payload(chapter_number: u32, title: &str, content: &str) -> String {
    format!("### Chapter {} - {}\n{}", chapter_number, title, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_scout() -> NameScout {
        NameScout::new(
            ApiConfig::default(),
            NameScoutConfig::default(),
            "Extract names".to_string(),
        )
    }

    #[test]
    fn test_parse_valid_json() {
        let scout = make_scout();
        let json = r#"{"names":[{"original":"田中","english":"Tanaka","part":"family"}]}"#;

        let result = scout.parse_response(json).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].original, "田中");
        assert_eq!(result[0].english, "Tanaka");
        assert_eq!(result[0].part, NamePart::Family);
    }

    #[test]
    fn test_parse_json_with_code_fence() {
        let scout = make_scout();
        let json = r#"```json
{"names":[{"original":"太郎","english":"Taro","part":"given"}]}
```"#;

        let result = scout.parse_response(json).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].original, "太郎");
        assert_eq!(result[0].english, "Taro");
    }

    #[test]
    fn test_parse_json_with_surrounding_text() {
        let scout = make_scout();
        let json = r#"Here are the names I found:
{"names":[{"original":"花子","english":"Hanako","part":"given"}]}
I hope this helps!"#;

        let result = scout.parse_response(json).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].original, "花子");
    }

    #[test]
    fn test_parse_empty_names() {
        let scout = make_scout();
        let json = r#"{"names":[]}"#;

        let result = scout.parse_response(json).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_missing_fields() {
        let scout = make_scout();
        let json = r#"{"names":[{"original":"田中"},{"english":"Smith"}]}"#;

        let result = scout.parse_response(json).unwrap();
        // Both entries should be filtered out due to missing required fields
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_invalid_json() {
        let scout = make_scout();
        let json = "This is not JSON at all";

        let result = scout.parse_response(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_split_into_chunks() {
        let config = NameScoutConfig {
            chunk_size_chars: 50,
            ..Default::default()
        };

        let scout = NameScout::new(ApiConfig::default(), config, String::new());

        let text = "Line one is here\nLine two is also here\nLine three continues\nLine four ends";
        let chunks = scout.split_into_chunks(text);

        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.len() <= 60); // Allow some leeway for line boundaries
        }
    }

    #[test]
    fn test_build_chapter_payload() {
        let payload = build_chapter_payload(5, "The Beginning", "Once upon a time...");
        assert_eq!(
            payload,
            "### Chapter 5 - The Beginning\nOnce upon a time..."
        );
    }

    use crate::config::ApiConfig;
    use crate::config::NameScoutConfig;
}
