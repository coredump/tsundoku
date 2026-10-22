//! Mapping scout - Extract character names and terminology from Japanese text using an LLM.
//!
//! Uses a secondary LLM pass to identify character names and recurring
//! story-specific terms with canonical English renderings.

use crate::config::{ApiConfig, NameScoutConfig};
use crate::console::Console;
use crate::error::TranslationError;
use crate::name_mapping::{CastEntry, Gender, KnownMappings, NameEntry, NamePart, TermEntry};
use crate::summaries::{MAX_SYNOPSIS_WORDS, SYNOPSIS_WORDS, cap_words, chapter_synopsis};
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt::Write as _;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

/// Regex to extract JSON from markdown code fences.
static CODE_FENCE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)^```[a-zA-Z]*\s*(.*?)\s*```$").expect("Invalid CODE_FENCE_REGEX")
});

/// Response shape appended to every custom mapping scout prompt.
const MAPPING_RESPONSE_CONTRACT: &str = r#"Return ONLY JSON with both arrays:
{"names":[{"original":"<exact name characters>","part":"family|given|unknown","english":"<best English rendering>","gender":"male|female|unknown","evidence":"<exact Japanese words from the text showing the gender>","note":"<at most 8 English words on who this is>"}],"terms":[{"original":"<exact Japanese term>","english":"<canonical English rendering>"}]}
Use empty arrays when no names or terms are found. Do not omit either key or add explanations.
"gender", "evidence" and "note" are optional. Give a gender only when the text itself shows it, and copy the words that show it into "evidence" exactly as written (for example 彼, 彼女, 兄, 姉, 息子, 娘, お嬢さん, 王子, 王女); otherwise use "unknown" or omit it. "note" is a short role or relationship such as "Ruri's older brother" or "guild receptionist".
`｜X《Y》` is ruby meaning X is read Y. Report `original` as X only and romanise from Y."#;

/// Longest chapter payload, in characters, summarised in a single request. Longer
/// chapters are split at scene breaks and summarised piece by piece.
pub const SUMMARY_SPLIT_CHARS: usize = 12000;

/// System prompt for chapter summary requests; `{words}` is the word budget.
const SUMMARY_PROMPT: &str = r#"You summarize one chapter of a Japanese web novel for the novel's English translator. Your summary is the translator's only memory of this chapter when translating later chapters, so it must be accurate.
- Write at most {words} English words of plain prose: who does what to whom, and how things stand at the end.
- Write names exactly as given under "Characters" (`original → English: gender; note`) and use the English given under "Terms".
- The story may be narrated in first person (俺, 僕, 私, あたし, わたし, うち…). Work out who the narrator is from the character list, the previous chapter and the text, and call them by name; say "the narrator" only if their name is never given.
- Do not swap characters' roles or attributes: keep who says, does and is what exactly as in the text, and use the character notes to tell characters apart.
- The previous chapter and "Earlier in this chapter" are context only; summarize only the text under "Text".
- `｜X《Y》` is ruby: X is read Y.
Return ONLY JSON: {"summary":"<summary>"}. No explanations."#;

/// Characters listed in a summary request when the chapter mentions many.
pub const SUMMARY_CAST_MAX_ENTRIES: usize = 100;

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
    #[serde(default)]
    gender: Option<String>,
    #[serde(default)]
    evidence: Option<String>,
    #[serde(default)]
    note: Option<String>,
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

/// Parsed chapter summary response from an LLM.
#[derive(Debug, Deserialize)]
struct ParsedSummaryResponse {
    summary: String,
}

/// Names and terminology identified in one successfully processed text chunk.
#[derive(Debug, Default)]
pub struct ScoutChunk {
    /// Character name entries.
    pub names: Vec<NameEntry>,
    /// Terminology entries.
    pub terms: Vec<TermEntry>,
}

impl ScoutChunk {
    /// Returns whether the chunk found nothing at all.
    fn is_empty(&self) -> bool {
        self.names.is_empty() && self.terms.is_empty()
    }
}

/// A character listed in a chapter summary request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryCharacter {
    /// The name as written in the Japanese text.
    pub original: String,
    /// Selected English rendering.
    pub english: String,
    /// Selected gender, if any.
    pub gender: Option<Gender>,
    /// Short description of the character, if any.
    pub note: Option<String>,
}

/// What the scout knows when summarising a chapter.
#[derive(Debug, Clone, Default)]
pub struct SummaryContext {
    /// Characters whose originals occur in the chapter: those with a gender or note
    /// first, in cast order, then the rest by original.
    pub characters: Vec<SummaryCharacter>,
    /// Glossary renderings whose originals occur in the chapter.
    pub terms: Vec<TermEntry>,
    /// Number and synopsis of the previous chapter, names already normalised.
    pub previous: Option<(u32, String)>,
}

impl SummaryContext {
    /// Combine the known renderings present in a chapter with its cast (from
    /// [`NameMappingStore::cast_for_text`](crate::name_mapping::NameMappingStore::cast_for_text))
    /// and the previous chapter's synopsis.
    ///
    /// A name gets the gender and note of the cast entry with the same English
    /// rendering, so every kana variant of a character carries its description.
    pub fn new(
        present: KnownMappings,
        cast: &[CastEntry],
        previous: Option<(u32, String)>,
    ) -> Self {
        let mut characters: Vec<(usize, SummaryCharacter)> = present
            .names
            .into_iter()
            .map(|name| {
                let described = cast.iter().position(|entry| entry.english == name.english);
                let entry = described.map(|index| &cast[index]);
                (
                    described.unwrap_or(usize::MAX),
                    SummaryCharacter {
                        gender: entry.and_then(|entry| entry.gender),
                        note: entry.and_then(|entry| entry.note.clone()),
                        original: name.original,
                        english: name.english,
                    },
                )
            })
            .collect();
        // Stable: names keep their order by original within the same cast position.
        characters.sort_by_key(|(position, _)| *position);

        Self {
            characters: characters
                .into_iter()
                .map(|(_, character)| character)
                .collect(),
            terms: present.terms,
            previous,
        }
    }
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
    /// Whether the chapter summary pass is enabled (`name_scout.summaries`).
    summaries: bool,
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
            summaries: false,
            console: Console::new(),
        }
    }

    /// Enable or disable the chapter summary pass (`name_scout.summaries`).
    ///
    /// The scout itself does not change; callers check [`Self::summaries_enabled`]
    /// before calling [`Self::summarize_chapter`].
    pub fn with_summaries(mut self, enabled: bool) -> Self {
        self.summaries = enabled;
        self
    }

    /// Returns whether the chapter summary pass is enabled.
    pub fn summaries_enabled(&self) -> bool {
        self.summaries
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
            let chunk_num = i + 1;
            let request_text = build_scout_user_message(chunk, &known.present_in(chunk));
            let entries = self
                .request_chunk(
                    chunk,
                    chunk_num,
                    total_chunks,
                    &self.prompt,
                    &request_text,
                    |raw| self.parse_response(raw, chunk),
                    progress_info.as_ref(),
                )
                .await?;
            if entries.is_empty() {
                continue;
            }
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

        Ok(results)
    }

    /// Summarize one chapter for the translator.
    ///
    /// `payload` is the chapter as built by [`build_chapter_payload`]. It is sent whole
    /// with `context` (characters, terms and the previous chapter's synopsis) and the
    /// response must be `{"summary": "…"}`. A payload longer than
    /// [`SUMMARY_SPLIT_CHARS`] characters is split at scene breaks; its pieces are
    /// summarised in order, each request also carrying the summaries of the earlier
    /// pieces, and each piece gets an equal share of the word budget.
    ///
    /// Returns the synopsis (piece summaries joined, capped at [`MAX_SYNOPSIS_WORDS`]
    /// words), empty when the model found nothing to summarise.
    pub async fn summarize_chapter(
        &self,
        payload: &str,
        context: &SummaryContext,
        progress_info: Option<ScoutProgressInfo>,
    ) -> Result<String, TranslationError> {
        let pieces = if payload.chars().count() > SUMMARY_SPLIT_CHARS {
            crate::utils::split_into_scene_chunks(payload, SUMMARY_SPLIT_CHARS)
        } else {
            vec![payload.to_string()]
        };
        let total_pieces = pieces.len();
        let words = (SYNOPSIS_WORDS / total_pieces).max(1);
        let max_words = (MAX_SYNOPSIS_WORDS / total_pieces).max(1);
        let system_prompt = summary_prompt(words);
        let mut summaries: Vec<String> = Vec::with_capacity(total_pieces);

        for (i, piece) in pieces.iter().enumerate() {
            let request_text = build_summary_user_message(piece, context, &summaries.join(" "));
            let summary = self
                .request_chunk(
                    piece,
                    i + 1,
                    total_pieces,
                    &system_prompt,
                    &request_text,
                    |raw| parse_summary_response(raw, max_words),
                    progress_info.as_ref(),
                )
                .await?;
            summaries.extend(summary);
        }

        Ok(chapter_synopsis(&summaries).unwrap_or_default())
    }

    /// Send one chunk request to the model and parse the response.
    ///
    /// Refusals and parse failures use the JSON retry budget, API failures the request
    /// retry budget; exhausting either aborts.
    #[allow(clippy::too_many_arguments)]
    async fn request_chunk<T>(
        &self,
        chunk: &str,
        chunk_num: usize,
        total_chunks: usize,
        system_prompt: &str,
        request_text: &str,
        parse: impl Fn(&str) -> Result<T, TranslationError>,
        progress_info: Option<&ScoutProgressInfo>,
    ) -> Result<T, TranslationError> {
        let chunk_started = Instant::now();
        self.report_progress(
            chunk_num,
            total_chunks,
            chunk.len(),
            chunk_started,
            NameScoutStatus::Scouting,
            progress_info,
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
        while attempt < self.scout_config.json_retries {
            // API failures use the dedicated retry budget and abort the phase when exhausted.
            let mut api_attempt = 0;
            let raw_response = loop {
                self.report_progress(
                    chunk_num,
                    total_chunks,
                    chunk.len(),
                    chunk_started,
                    NameScoutStatus::Waiting,
                    progress_info,
                );

                match self.call_model(system_prompt, request_text).await {
                    Ok(raw_response) => break raw_response,
                    Err(e) => {
                        api_attempt += 1;
                        if api_attempt >= self.scout_config.retries {
                            self.report_notice(
                                chunk_num,
                                true,
                                format!(
                                    "Name scout API failed for chapter {}, chunk {} after {} attempts",
                                    progress_info.map_or(0, |info| info.chapter),
                                    chunk_num,
                                    self.scout_config.retries
                                ),
                                progress_info,
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
                            progress_info,
                        );
                        self.report_notice(
                            chunk_num,
                            false,
                            format!(
                                "API error for chapter {}, chunk {}: {}; retrying in {:?} (attempt {}/{})",
                                progress_info.map_or(0, |info| info.chapter),
                                chunk_num,
                                format_translation_error(&e),
                                delay,
                                api_attempt + 1,
                                self.scout_config.retries
                            ),
                            progress_info,
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
                            progress_info,
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
                    progress_info,
                );
                self.report_notice(
                    chunk_num,
                    false,
                    format!(
                        "Model refused chapter {}, chunk {}; retrying",
                        progress_info.map_or(0, |info| info.chapter),
                        chunk_num
                    ),
                    progress_info,
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
                progress_info,
            );

            // Parse the response
            match parse(&raw_response) {
                Ok(parsed) => return Ok(parsed),
                Err(e) => {
                    self.report_progress(
                        chunk_num,
                        total_chunks,
                        chunk.len(),
                        chunk_started,
                        NameScoutStatus::Retrying,
                        progress_info,
                    );
                    self.report_notice(
                        chunk_num,
                        false,
                        format!(
                            "Failed to parse JSON from chapter {}, chunk {}: {}; response preview: {}; retrying",
                            progress_info.map_or(0, |info| info.chapter),
                            chunk_num,
                            e,
                            preview_text(&raw_response, 1200)
                        ),
                        progress_info,
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

        self.report_notice(
            chunk_num,
            true,
            format!(
                "Failed to process chapter {}, chunk {} after {} attempts",
                progress_info.map_or(0, |info| info.chapter),
                chunk_num,
                self.scout_config.json_retries
            ),
            progress_info,
        );
        if progress_info.is_none() {
            self.console.error(&format!(
                "Failed to process chunk {} after {} attempts",
                chunk_num, self.scout_config.json_retries
            ));
        }
        Err(TranslationError::RetriesExhausted {
            attempts: self.scout_config.json_retries,
        })
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

    /// Split text into chunks of at most `chunk_size_chars` characters, preferring
    /// scene breaks.
    fn split_into_chunks(&self, text: &str) -> Vec<String> {
        crate::utils::split_into_scene_chunks(text, self.scout_config.chunk_size_chars)
    }

    /// Send one chat request with `system_prompt` and `user_message`.
    async fn call_model(
        &self,
        system_prompt: &str,
        user_message: &str,
    ) -> Result<String, TranslationError> {
        let request = ChatRequest {
            model: self.api_config.model.clone(),
            messages: vec![
                Message {
                    role: "system".to_string(),
                    content: system_prompt.to_string(),
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
    ///
    /// `chunk` is the scouted text. A name's gender is kept only when it is not
    /// `unknown` and its evidence is non-empty and occurs verbatim in `chunk`.
    fn parse_response(&self, raw: &str, chunk: &str) -> Result<ScoutChunk, TranslationError> {
        let value = extract_json_object(raw)?;

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

                let evidence = entry.evidence.as_deref().map(str::trim).unwrap_or_default();
                let gender = entry
                    .gender
                    .as_deref()
                    .and_then(|gender| gender.parse::<Gender>().ok())
                    .filter(|gender| {
                        *gender != Gender::Unknown && !evidence.is_empty() && chunk.contains(evidence)
                    });
                let note = entry
                    .note
                    .map(|note| note.trim().to_string())
                    .filter(|note| !note.is_empty());

                Some(NameEntry {
                    original,
                    english,
                    part,
                    gender,
                    note,
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

/// Strip a markdown code fence and parse the first complete JSON object in `raw`.
///
/// Models sometimes wrap the JSON in prose or append a stray `}`.
fn extract_json_object(raw: &str) -> Result<serde_json::Value, TranslationError> {
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

    let start = json_str
        .find('{')
        .ok_or_else(|| TranslationError::ParseError("No valid JSON object found".to_string()))?;
    serde_json::Deserializer::from_str(&json_str[start..])
        .into_iter()
        .next()
        .ok_or_else(|| TranslationError::ParseError("No valid JSON object found".to_string()))?
        .map_err(|e| TranslationError::ParseError(format!("JSON parse error: {}", e)))
}

/// Parse a chapter summary response: `{"summary": "…"}`.
///
/// Whitespace is collapsed and the summary capped at `max_words` words. A missing
/// `summary` key is an error (the request is retried); an empty summary yields `None`.
fn parse_summary_response(raw: &str, max_words: usize) -> Result<Option<String>, TranslationError> {
    let parsed: ParsedSummaryResponse = serde_json::from_value(extract_json_object(raw)?)
        .map_err(|e| TranslationError::ParseError(format!("JSON parse error: {}", e)))?;
    let summary = cap_words(&parsed.summary, max_words);
    Ok((!summary.is_empty()).then_some(summary))
}

/// Instruction heading the known renderings in a mapping request.
const MAPPINGS_HEADER: &str = "Renderings already chosen in earlier chapters. Whenever you report one of these originals, use exactly the listed English. Still report new names and terms; if there are none, return both keys with empty arrays.\n";

/// Build a mapping request: [`MAPPINGS_HEADER`] and the known renderings for this
/// chunk, then the text. Without known renderings the message is the text alone.
fn build_scout_user_message(chunk: &str, known: &KnownMappings) -> String {
    if known.is_empty() {
        return chunk.to_string();
    }

    let mut message = String::from(MAPPINGS_HEADER);
    for (heading, entries) in [("Names", &known.names), ("Terms", &known.terms)] {
        if entries.is_empty() {
            continue;
        }
        message.push_str(heading);
        message.push_str(":\n");
        for entry in entries {
            push_rendering(&mut message, entry);
            message.push('\n');
        }
    }
    message.push_str("\nText:\n");
    message.push_str(chunk);
    message
}

/// Append `- {original} → {english}` to `message`.
fn push_rendering(message: &mut String, entry: &TermEntry) {
    message.push_str("- ");
    message.push_str(&entry.original);
    message.push_str(" → ");
    message.push_str(&entry.english);
}

/// System prompt for a summary request asking for at most `words` words.
fn summary_prompt(words: usize) -> String {
    SUMMARY_PROMPT.replace("{words}", &words.to_string())
}

/// Build a summary request: the characters (`- {original} → {english}: {gender};
/// {note}`), terms, previous chapter's synopsis and the summaries of earlier pieces
/// of the same chapter, each section only when non-empty, then the text.
fn build_summary_user_message(text: &str, context: &SummaryContext, earlier: &str) -> String {
    let mut message = String::new();
    if !context.characters.is_empty() {
        message.push_str("Characters:\n");
        for character in &context.characters {
            message.push_str("- ");
            message.push_str(&character.original);
            message.push_str(" → ");
            message.push_str(&character.english);
            for (i, detail) in character
                .gender
                .map(Gender::as_str)
                .into_iter()
                .chain(character.note.as_deref())
                .enumerate()
            {
                message.push_str(if i == 0 { ": " } else { "; " });
                message.push_str(detail);
            }
            message.push('\n');
        }
        message.push('\n');
    }
    if !context.terms.is_empty() {
        message.push_str("Terms:\n");
        for term in &context.terms {
            push_rendering(&mut message, term);
            message.push('\n');
        }
        message.push('\n');
    }
    if let Some((number, synopsis)) = &context.previous {
        let _ = write!(message, "Previous chapter ({number}): {synopsis}\n\n");
    }
    if !earlier.is_empty() {
        message.push_str("Earlier in this chapter: ");
        message.push_str(earlier);
        message.push_str("\n\n");
    }
    message.push_str("Text:\n");
    message.push_str(text);
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

        let result = scout.parse_response(json, "").unwrap();
        assert_eq!(result.names.len(), 1);
        assert!(result.terms.is_empty());
        assert_eq!(result.names[0].original, "田中");
        assert_eq!(result.names[0].english, "Tanaka");
        assert_eq!(result.names[0].part, NamePart::Family);
    }

    #[test]
    fn test_parse_gender_needs_evidence_from_the_chunk() {
        let scout = make_scout();
        let json = r#"{"names":[
            {"original":"タツヤ","english":"Tatsuya","part":"given","gender":"male","evidence":"兄のタツヤ","note":" Ruri's older brother "},
            {"original":"ルリ","english":"Ruri","part":"given","gender":"female","evidence":"彼女"},
            {"original":"モニカ","english":"Monika","part":"given","gender":"female","evidence":""},
            {"original":"ミナト","english":"Minato","part":"given","gender":"unknown","evidence":"ミナト"},
            {"original":"田中","english":"Tanaka","part":"family","gender":"Male ","evidence":"田中君"}
        ],"terms":[]}"#;

        let result = scout
            .parse_response(json, "兄のタツヤとルリ、モニカ、ミナト、田中君が来た。")
            .unwrap();

        let genders: Vec<_> = result
            .names
            .iter()
            .map(|entry| (entry.english.as_str(), entry.gender))
            .collect();
        assert_eq!(
            genders,
            [
                ("Tatsuya", Some(Gender::Male)),
                ("Ruri", None),
                ("Monika", None),
                ("Minato", None),
                ("Tanaka", Some(Gender::Male)),
            ]
        );
        assert_eq!(result.names[0].note.as_deref(), Some("Ruri's older brother"));
        assert_eq!(result.names[1].note, None);
    }

    #[test]
    fn test_parse_names_only_response_fails() {
        let scout = make_scout();
        let result = scout.parse_response(
            r#"{"names":[{"original":"田中","english":"Tanaka","part":"family"}]}"#,
            "",
        );

        assert!(result.is_err());
    }

    #[test]
    fn test_parse_empty_object_means_nothing_new() {
        let scout = make_scout();
        let result = scout.parse_response("{}", "").unwrap();

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
    fn test_mapping_response_ignores_stray_summary() {
        let scout = make_scout().with_summaries(true);
        assert!(scout.summaries_enabled());
        assert!(!scout.prompt.contains("summary"));
        assert!(
            scout
                .parse_response(r#"{"names":[],"terms":[],"summary":"Ruri sleeps."}"#, "")
                .unwrap()
                .is_empty()
        );
        assert!(
            scout
                .parse_response(r#"{"summary":"Ruri sleeps."}"#, "")
                .is_err()
        );
    }

    #[test]
    fn test_parse_summary_response() {
        assert_eq!(
            parse_summary_response("```json\n{\"summary\":\"  Ruri\\n sleeps. \"}\n```", 10)
                .unwrap(),
            Some("Ruri sleeps.".to_string())
        );
        assert_eq!(
            parse_summary_response(r#"{"summary":"one two three"}"#, 2).unwrap(),
            Some("one two…".to_string())
        );
        assert_eq!(
            parse_summary_response(r#"{"summary":" "}"#, 10).unwrap(),
            None
        );
        assert!(parse_summary_response("{}", 10).is_err());
        assert!(parse_summary_response(r#"{"names":[],"terms":[]}"#, 10).is_err());
    }

    #[test]
    fn test_summary_user_message_lists_characters_terms_and_context() {
        let rendering = |original: &str, english: &str| TermEntry {
            original: original.to_string(),
            english: english.to_string(),
        };
        let present = KnownMappings {
            names: vec![
                rendering("タツヤ", "Tatsuya"),
                rendering("モニカ", "Monika"),
                rendering("ルリ", "Ruri"),
                rendering("瑠璃", "Ruri"),
            ],
            terms: vec![rendering("魔石", "magic stone")],
        };
        let cast = [
            CastEntry {
                english: "Ruri".to_string(),
                gender: Some(Gender::Female),
                note: Some("homunculus Tatsuya created".to_string()),
            },
            CastEntry {
                english: "Tatsuya".to_string(),
                gender: Some(Gender::Male),
                note: None,
            },
        ];
        let context = SummaryContext::new(
            present,
            &cast,
            Some((1, "Tatsuya creates Ruri.".to_string())),
        );

        assert_eq!(
            build_summary_user_message("### Chapter 2 - 二\nルリは笑った。", &context, ""),
            "Characters:\n\
             - ルリ → Ruri: female; homunculus Tatsuya created\n\
             - 瑠璃 → Ruri: female; homunculus Tatsuya created\n\
             - タツヤ → Tatsuya: male\n\
             - モニカ → Monika\n\
             \n\
             Terms:\n\
             - 魔石 → magic stone\n\
             \n\
             Previous chapter (1): Tatsuya creates Ruri.\n\
             \n\
             Text:\n\
             ### Chapter 2 - 二\nルリは笑った。"
        );
        assert_eq!(
            build_summary_user_message("ルリは寝た。", &SummaryContext::default(), "Ruri laughs."),
            "Earlier in this chapter: Ruri laughs.\n\nText:\nルリは寝た。"
        );
        assert!(summary_prompt(50).contains("at most 50 English words"));
    }

    #[test]
    fn test_parse_terminology() {
        let scout = make_scout();
        let json = r#"{
            "names": [],
            "terms": [{"original":"魔石","english":"Magic Stone"}]
        }"#;

        let result = scout.parse_response(json, "").unwrap();
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

        let result = scout.parse_response(json, "").unwrap();
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

        let result = scout.parse_response(json, "").unwrap();
        assert_eq!(result.names.len(), 1);
        assert_eq!(result.names[0].original, "花子");
    }

    #[test]
    fn test_parse_json_with_stray_trailing_brace() {
        let scout = make_scout();
        let json = r#"{"names":[{"original":"ペーリス","part":"given","english":"Peris"}],"terms":[]}}"#;

        let result = scout.parse_response(json, "").unwrap();
        assert_eq!(result.names.len(), 1);
        assert_eq!(result.names[0].english, "Peris");
    }

    #[test]
    fn test_parse_empty_mappings() {
        let scout = make_scout();
        let json = r#"{"names":[],"terms":[]}"#;

        let result = scout.parse_response(json, "").unwrap();
        assert!(result.names.is_empty());
        assert!(result.terms.is_empty());
    }

    #[test]
    fn test_parse_missing_fields() {
        let scout = make_scout();
        let json =
            r#"{"names":[{"original":"田中"},{"english":"Smith"}],"terms":[{"original":"魔石"}]}"#;

        let result = scout.parse_response(json, "").unwrap();
        assert!(result.names.is_empty());
        assert!(result.terms.is_empty());
    }

    #[test]
    fn test_parse_invalid_json() {
        let scout = make_scout();
        let json = "This is not JSON at all";

        let result = scout.parse_response(json, "");
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
            assert!(chunk.chars().count() <= 50, "{chunk}");
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
