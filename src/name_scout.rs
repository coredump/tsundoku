//! Mapping scout - Extract character names and terminology from Japanese text using an LLM.
//!
//! Uses a secondary LLM pass to identify character names and recurring
//! story-specific terms with canonical English renderings.

use crate::config::{ApiConfig, NameScoutConfig};
use crate::console::Console;
use crate::error::TranslationError;
use crate::name_mapping::{KnownMappings, NameEntry, NamePart, TermEntry};
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

/// Response shape appended to every custom mapping scout prompt.
const MAPPING_RESPONSE_CONTRACT: &str = r#"Return ONLY JSON with both arrays:
{"names":[{"original":"<exact name characters>","part":"family|given|unknown","english":"<best English rendering>"}],"terms":[{"original":"<exact Japanese term>","english":"<canonical English rendering>"}]}
Use empty arrays when no names or terms are found. Do not omit either key or add explanations.
`｜X《Y》` is ruby meaning X is read Y. Report `original` as X only and romanise from Y."#;

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

/// Parsed name entry from an LLM response.
#[derive(Debug, Deserialize)]
struct ParsedNameEntry {
    original: Option<String>,
    english: Option<String>,
    part: Option<String>,
}

/// Parsed terminology entry from an LLM response.
#[derive(Debug, Deserialize)]
struct ParsedTermEntry {
    original: Option<String>,
    english: Option<String>,
}

/// Parsed name and terminology response from an LLM.
#[derive(Debug, Deserialize)]
struct ParsedMappingsResponse {
    names: Vec<ParsedNameEntry>,
    terms: Vec<ParsedTermEntry>,
}

/// Names and terminology identified in one successfully processed text chunk.
#[derive(Debug, Default)]
pub struct ScoutChunk {
    /// Character name entries.
    pub names: Vec<NameEntry>,
    /// Terminology entries.
    pub terms: Vec<TermEntry>,
}

/// Mapping scout for extracting character names and terminology from Japanese text.
pub struct NameScout {
    /// HTTP client for API requests.
    client: Client,
    /// API configuration.
    api_config: ApiConfig,
    /// Name scout behavior configuration.
    scout_config: NameScoutConfig,
    /// System prompt for name and terminology extraction.
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
            prompt: format!("{prompt}\n\n{MAPPING_RESPONSE_CONTRACT}"),
            console: Console::new(),
        }
    }

    /// Collect names and terminology from text, processing in chunks.
    ///
    /// Returns mapping entries for each successfully processed chunk.
    pub async fn collect_mappings(&self, text: &str) -> Result<Vec<ScoutChunk>, TranslationError> {
        self.collect_mappings_with_progress(text, &KnownMappings::default(), None)
            .await
    }

    /// Collect names and terminology from text, reporting chunk progress when a reporter is provided.
    ///
    /// Known renderings that occur in a chunk are sent with it so the model reuses them
    /// instead of romanizing the same original differently in every chapter.
    pub async fn collect_mappings_with_progress(
        &self,
        text: &str,
        known: &KnownMappings,
        progress_info: Option<ScoutProgressInfo>,
    ) -> Result<Vec<ScoutChunk>, TranslationError> {
        let chunks = self.split_into_chunks(text);
        let total_chunks = chunks.len();
        let mut results = Vec::new();

        for (i, chunk) in chunks.iter().enumerate() {
            let request_text = build_scout_user_message(chunk, &known.present_in(chunk));
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

                    match self.call_model(&request_text).await {
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
                if crate::utils::is_refusal(&raw_response) {
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
                    if attempt < self.scout_config.json_retries {
                        tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
                    }
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
                        if !entries.names.is_empty() || !entries.terms.is_empty() {
                            if progress_info.is_none() {
                                self.console.success(&format!(
                                    "Found {} names and {} terms in chunk {}",
                                    entries.names.len(),
                                    entries.terms.len(),
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
                        if attempt < self.scout_config.json_retries {
                            tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
                        }
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
                return Err(TranslationError::RetriesExhausted {
                    attempts: self.scout_config.json_retries,
                });
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
    async fn call_model(&self, user_message: &str) -> Result<String, TranslationError> {
        let request = ChatRequest {
            model: self.api_config.model.clone(),
            messages: vec![
                Message {
                    role: "system".to_string(),
                    content: self.prompt.clone(),
                },
                Message {
                    role: "user".to_string(),
                    content: user_message.to_string(),
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

    /// Parse the LLM response into name and terminology entries.
    fn parse_response(&self, raw: &str) -> Result<ScoutChunk, TranslationError> {
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

        // Parse the first complete JSON object; models sometimes append prose or a stray `}`.
        let start = json_str.find('{').ok_or_else(|| {
            TranslationError::ParseError("No valid JSON object found".to_string())
        })?;
        let value: serde_json::Value = serde_json::Deserializer::from_str(&json_str[start..])
            .into_iter()
            .next()
            .ok_or_else(|| TranslationError::ParseError("No valid JSON object found".to_string()))?
            .map_err(|e| TranslationError::ParseError(format!("JSON parse error: {}", e)))?;

        // When every name and term in a chunk is already known, models answer `{}`.
        // A response with only one of the two keys is still rejected as incomplete.
        if value.as_object().is_some_and(serde_json::Map::is_empty) {
            return Ok(ScoutChunk::default());
        }

        let parsed: ParsedMappingsResponse = serde_json::from_value(value)
            .map_err(|e| TranslationError::ParseError(format!("JSON parse error: {}", e)))?;

        let names = parsed
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
                    .and_then(|part| part.parse().ok())
                    .unwrap_or(NamePart::Unknown);

                Some(NameEntry {
                    original,
                    english,
                    part,
                })
            })
            .collect();

        let terms = parsed
            .terms
            .into_iter()
            .filter_map(|entry| {
                let original = entry.original?.trim().to_string();
                let english = entry.english?.trim().to_string();

                if original.is_empty() || english.is_empty() {
                    return None;
                }

                Some(TermEntry { original, english })
            })
            .collect();

        Ok(ScoutChunk { names, terms })
    }
}

/// Build the scout user message: known renderings for this chunk, then the text.
fn build_scout_user_message(chunk: &str, known: &KnownMappings) -> String {
    if known.is_empty() {
        return chunk.to_string();
    }

    let mut message = String::from(
        "Renderings already chosen in earlier chapters. Whenever you report one of these originals, use exactly the listed English. Still report new names and terms; if there are none, return both keys with empty arrays.\n",
    );
    for (heading, entries) in [("Names", &known.names), ("Terms", &known.terms)] {
        if entries.is_empty() {
            continue;
        }
        message.push_str(heading);
        message.push_str(":\n");
        for entry in entries {
            message.push_str("- ");
            message.push_str(&entry.original);
            message.push_str(" → ");
            message.push_str(&entry.english);
            message.push('\n');
        }
    }
    message.push_str("\nText:\n");
    message.push_str(chunk);
    message
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
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;

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
        let json =
            r#"{"names":[{"original":"田中","english":"Tanaka","part":"family"}],"terms":[]}"#;

        let result = scout.parse_response(json).unwrap();
        assert_eq!(result.names.len(), 1);
        assert!(result.terms.is_empty());
        assert_eq!(result.names[0].original, "田中");
        assert_eq!(result.names[0].english, "Tanaka");
        assert_eq!(result.names[0].part, NamePart::Family);
    }

    #[test]
    fn test_parse_names_only_response_fails() {
        let scout = make_scout();
        let result = scout.parse_response(
            r#"{"names":[{"original":"田中","english":"Tanaka","part":"family"}]}"#,
        );

        assert!(result.is_err());
    }

    #[test]
    fn test_parse_empty_object_means_nothing_new() {
        let scout = make_scout();
        let result = scout.parse_response("{}").unwrap();

        assert!(result.names.is_empty());
        assert!(result.terms.is_empty());
    }

    #[tokio::test]
    async fn test_names_only_response_cannot_complete_mapping_scout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }

            let response_body = r#"{"choices":[{"message":{"content":"{\"names\":[{\"original\":\"田中\",\"english\":\"Tanaka\",\"part\":\"family\"}]}"}}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            )
            .unwrap();
        });

        let scout = NameScout::new(
            ApiConfig {
                key: "test-key".to_string(),
                base_url: format!("http://{address}/v1"),
                model: "test-model".to_string(),
            },
            NameScoutConfig {
                retries: 1,
                json_retries: 1,
                delay_between_requests_sec: 0.0,
                ..Default::default()
            },
            "Extract mappings".to_string(),
        );

        let result = scout.collect_mappings("田中は魔石を持っていた。").await;
        server.join().unwrap();

        assert!(matches!(
            result,
            Err(TranslationError::RetriesExhausted { attempts: 1 })
        ));
    }

    #[test]
    fn test_parse_terminology() {
        let scout = make_scout();
        let json = r#"{
            "names": [],
            "terms": [{"original":"魔石","english":"Magic Stone"}]
        }"#;

        let result = scout.parse_response(json).unwrap();
        assert!(result.names.is_empty());
        assert_eq!(result.terms.len(), 1);
        assert_eq!(result.terms[0].original, "魔石");
        assert_eq!(result.terms[0].english, "Magic Stone");
    }

    #[test]
    fn test_parse_json_with_code_fence() {
        let scout = make_scout();
        let json = r#"```json
{"names":[{"original":"太郎","english":"Taro","part":"given"}],"terms":[]}
```"#;

        let result = scout.parse_response(json).unwrap();
        assert_eq!(result.names.len(), 1);
        assert_eq!(result.names[0].original, "太郎");
        assert_eq!(result.names[0].english, "Taro");
    }

    #[test]
    fn test_parse_json_with_surrounding_text() {
        let scout = make_scout();
        let json = r#"Here are the mappings I found:
{"names":[{"original":"花子","english":"Hanako","part":"given"}],"terms":[]}
I hope this helps!"#;

        let result = scout.parse_response(json).unwrap();
        assert_eq!(result.names.len(), 1);
        assert_eq!(result.names[0].original, "花子");
    }

    #[test]
    fn test_parse_json_with_stray_trailing_brace() {
        let scout = make_scout();
        let json = r#"{"names":[{"original":"ペーリス","part":"given","english":"Peris"}],"terms":[]}}"#;

        let result = scout.parse_response(json).unwrap();
        assert_eq!(result.names.len(), 1);
        assert_eq!(result.names[0].english, "Peris");
    }

    #[test]
    fn test_parse_empty_mappings() {
        let scout = make_scout();
        let json = r#"{"names":[],"terms":[]}"#;

        let result = scout.parse_response(json).unwrap();
        assert!(result.names.is_empty());
        assert!(result.terms.is_empty());
    }

    #[test]
    fn test_parse_missing_fields() {
        let scout = make_scout();
        let json =
            r#"{"names":[{"original":"田中"},{"english":"Smith"}],"terms":[{"original":"魔石"}]}"#;

        let result = scout.parse_response(json).unwrap();
        assert!(result.names.is_empty());
        assert!(result.terms.is_empty());
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
