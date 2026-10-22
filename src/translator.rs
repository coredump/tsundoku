//! Translation system using OpenAI-compatible APIs.
//!
//! Provides text translation with streaming progress display,
//! message history management, and retry logic.

use crate::config::{ApiConfig, TranslationConfig};
use crate::console::Console;
use crate::error::TranslationError;
use crate::honorifics;
use crate::name_mapping::{CastEntry, TermEntry};
use crate::output_rules::OutputRules;
use crate::qa::{self, ChunkContext, Issue, IssueKind, QaConfig};
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::io::{self, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

/// Rule appended to every content translation prompt.
const ENGLISH_ONLY_RULE: &str = "Write the translation entirely in English. Never output Chinese or Japanese characters; translate every word, including terms you would otherwise gloss in parentheses.";

/// Rule explaining gloss ruby left in the source by [`crate::ruby::for_translation`].
const RUBY_RULE: &str = "`X《Y》` is ruby: Y is the author's intended word written with X; translate it as Y (聖剣《エクスカリバー》 → Excalibur).";

/// Marker that precedes the untranslated source of a chunk the model refused.
const FAILED_MARKER: &str = "[TRANSLATION FAILED]";

/// Smallest `max_tokens` sent with a request, so short titles and chunks are never cut.
const MIN_MAX_TOKENS: u32 = 512;

/// `finish_reason` of a reply that stopped at the token limit.
const FINISH_REASON_LENGTH: &str = "length";

/// Sections of the content system prompt, in rendering order.
#[derive(Debug, Clone, Copy)]
enum PromptSection {
    /// The configured content translation prompt.
    Base,
    /// The English-only and ruby rules.
    Rules,
    /// The novel's honorific policy line.
    Honorifics,
    /// Characters present in the chapter, with gender and note.
    Cast,
    /// Canonical terminology renderings.
    Glossary,
    /// Synopses of the previous chapters.
    Previously,
}

impl PromptSection {
    /// Rendering order of the system prompt sections.
    const ORDER: [PromptSection; 6] = [
        Self::Base,
        Self::Rules,
        Self::Honorifics,
        Self::Cast,
        Self::Glossary,
        Self::Previously,
    ];
}

/// Chapter-level context rendered into the content translation system prompt.
///
/// Build it once per chapter: the rendered prompt is then byte-identical for every
/// chunk, so the server can reuse its prompt cache.
#[derive(Debug, Clone, Default)]
pub struct PromptContext {
    /// Canonical terminology present in the chapter, already capped at
    /// `translation.glossary_max_entries`.
    pub glossary: Vec<TermEntry>,
    /// Characters present in the chapter that have a gender or a note, already
    /// capped at `translation.cast_max_entries`.
    pub cast: Vec<CastEntry>,
    /// Synopses of the previous chapters as `(chapter number, synopsis)`, oldest
    /// first, with names already normalised to their selected renderings.
    pub previously: Vec<(u32, String)>,
    /// The novel's output rules. Their honorific policy is rendered as the
    /// honorifics line, and replies are checked with [`qa::check_honorifics`]
    /// against them; `None` leaves out both.
    pub output_rules: Option<Arc<OutputRules>>,
}

impl PromptContext {
    /// Render the content system prompt: `base`, the English-only and ruby rules,
    /// the honorific policy, the cast, the glossary, then the previous chapters'
    /// synopses. Sections are separated by a blank line; empty ones are left out.
    pub fn system_prompt(&self, base: &str) -> String {
        let mut prompt = String::new();
        for section in PromptSection::ORDER {
            let section_start = prompt.len();
            if !prompt.is_empty() {
                prompt.push_str("\n\n");
            }
            let body_start = prompt.len();
            self.render_section(section, base, &mut prompt);
            if prompt.len() == body_start {
                prompt.truncate(section_start);
            }
        }
        prompt
    }

    /// Append one section's text to `out`; nothing when the section is empty.
    fn render_section(&self, section: PromptSection, base: &str, out: &mut String) {
        match section {
            PromptSection::Base => out.push_str(base),
            PromptSection::Rules => {
                out.push_str(ENGLISH_ONLY_RULE);
                out.push('\n');
                out.push_str(RUBY_RULE);
            }
            PromptSection::Honorifics => {
                if let Some(rules) = &self.output_rules {
                    out.push_str(honorifics::prompt_line(rules.policy()));
                }
            }
            PromptSection::Cast => {
                if self.cast.is_empty() {
                    return;
                }
                out.push_str(
                    "Characters in this chapter (use matching pronouns even where the Japanese omits the subject):",
                );
                for entry in &self.cast {
                    out.push_str("\n- ");
                    out.push_str(&entry.english);
                    out.push_str(": ");
                    if let Some(gender) = entry.gender {
                        out.push_str(gender.as_str());
                        if entry.note.is_some() {
                            out.push_str("; ");
                        }
                    }
                    if let Some(note) = &entry.note {
                        out.push_str(note);
                    }
                }
            }
            PromptSection::Glossary => {
                if self.glossary.is_empty() {
                    return;
                }
                out.push_str(
                    "Use these canonical terminology renderings exactly when the Japanese term appears:",
                );
                for entry in &self.glossary {
                    out.push_str("\n- ");
                    out.push_str(&entry.original);
                    out.push_str(" → ");
                    out.push_str(&entry.english);
                }
            }
            PromptSection::Previously => {
                if self.previously.is_empty() {
                    return;
                }
                out.push_str("Story so far (context only; do not translate):");
                for (chapter, synopsis) in &self.previously {
                    // Writing to a String cannot fail.
                    let _ = write!(out, "\n- Ch {chapter}: {synopsis}");
                }
            }
        }
    }
}

/// One translated chunk of chapter content.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkResult {
    /// The exact text sent to the model.
    pub source: String,
    /// The model's translation, or `[TRANSLATION FAILED]` and a newline followed by
    /// `source` when every attempt was refused.
    pub output: String,
    /// Problems the output was kept with.
    pub issues: Vec<Issue>,
}

/// One chunk of a chapter passed to [`Translator::translate_chunks`].
#[derive(Debug, Clone, PartialEq)]
pub enum ChunkPlan {
    /// Translate this text.
    Translate(String),
    /// Keep an earlier translation; it only feeds the conversation history.
    Keep {
        /// The text that was sent to the model.
        source: String,
        /// The model's output.
        output: String,
    },
}

/// A reply accepted by [`Translator::translate_with_retries`].
struct AcceptedReply {
    /// The translated text.
    text: String,
    /// Problems the reply was kept with on the last attempt.
    issues: Vec<Issue>,
}

/// A streamed reply that passed the empty-reply and refusal checks.
struct StreamedReply {
    /// The trimmed reply text.
    text: String,
    /// The last non-null `finish_reason` seen in the stream.
    finish_reason: Option<String>,
}

impl StreamedReply {
    /// Problems that make this reply worth another attempt: truncation at the token
    /// limit and leftover Chinese/Japanese characters.
    fn issues(&self, max_tokens: Option<u32>) -> Vec<Issue> {
        let mut issues = Vec::new();
        if self.finish_reason.as_deref() == Some(FINISH_REASON_LENGTH) {
            let detail = match max_tokens {
                Some(limit) => format!("reply stopped at the {limit}-token limit"),
                None => "reply stopped at the server's token limit".to_string(),
            };
            issues.push(Issue {
                kind: IssueKind::Truncated,
                detail,
            });
        }
        let count = crate::utils::count_cjk_chars(&self.text);
        if count > 0 {
            issues.push(Issue {
                kind: IssueKind::ForeignScript,
                detail: format!("{count} Chinese/Japanese characters in the output"),
            });
        }
        issues
    }
}

/// Join issue details for a one-line console message.
fn describe_issues(issues: &[Issue]) -> String {
    issues
        .iter()
        .map(|issue| issue.detail.as_str())
        .collect::<Vec<_>>()
        .join("; ")
}

/// Progress information for display during translation.
#[derive(Debug, Clone)]
pub struct ProgressInfo {
    /// Current chapter number (1-based).
    pub chapter: u32,
    /// Current chunk number (1-based).
    pub chunk: u32,
    /// Total number of chunks.
    pub total_chunks: u32,
    /// Optional worker slot currently translating this chapter.
    pub worker: Option<usize>,
    /// Optional channel for centralized progress reporting.
    pub reporter: Option<UnboundedSender<TranslationProgress>>,
}

/// Stage currently being reported for a translation worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranslationStage {
    /// Worker is not currently processing a chapter.
    Idle,
    /// Worker is translating a chapter title.
    Title,
    /// Worker is between content chunks.
    Preparing,
    /// Worker is translating chapter content.
    Content,
}

/// Progress event emitted while a translation stream is running.
#[derive(Debug, Clone)]
pub struct TranslationProgress {
    /// Worker slot emitting this update.
    pub worker: usize,
    /// Chapter currently being translated.
    pub chapter: u32,
    /// Current chunk number.
    pub chunk: u32,
    /// Total chunk count for the chapter.
    pub total_chunks: u32,
    /// Number of translated characters received for the current chunk.
    pub chars: usize,
    /// Seconds elapsed since the current chunk started.
    pub elapsed_secs: f64,
    /// Current translation stage.
    pub stage: TranslationStage,
}

/// A message in the conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Role: "system", "user", or "assistant".
    pub role: String,
    /// Content of the message.
    pub content: String,
}

/// Request body for the chat completions API.
#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    stream: bool,
    /// Reply token limit; omitted when `translation.max_tokens_per_char` is 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

/// Response from the chat completions API (non-streaming).
/// Used for non-streaming API calls.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ChatResponse {
    choices: Vec<Choice>,
}

/// A single choice in the response.
#[derive(Debug, Deserialize)]
struct Choice {
    #[allow(dead_code)]
    message: Option<ResponseMessage>,
    delta: Option<Delta>,
    #[allow(dead_code)]
    index: u32,
    /// Why generation stopped (`"stop"`, `"length"`); null until the last chunk.
    finish_reason: Option<String>,
}

/// Message content in a non-streaming response.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ResponseMessage {
    role: String,
    content: String,
}

/// Delta content in a streaming response.
#[derive(Debug, Deserialize)]
struct Delta {
    content: Option<String>,
}

/// Streaming chunk from the API.
#[derive(Debug, Deserialize)]
struct StreamChunk {
    choices: Vec<Choice>,
}

/// Translator for converting Japanese text to English.
pub struct Translator {
    /// HTTP client for API requests.
    client: Client,
    /// API configuration.
    api_config: ApiConfig,
    /// Translation behavior configuration.
    translation_config: TranslationConfig,
    /// Post-translation checks and QA retry settings for content chunks.
    qa_config: QaConfig,
    /// System prompt for title translation.
    title_prompt: String,
    /// System prompt for content translation.
    content_prompt: String,
    /// Console for output.
    console: Console,
}

impl Translator {
    /// Create a new Translator.
    pub fn new(
        api_config: ApiConfig,
        translation_config: TranslationConfig,
        qa_config: QaConfig,
        title_prompt: String,
        content_prompt: String,
    ) -> Self {
        Self {
            client: Client::new(),
            api_config,
            translation_config,
            qa_config,
            title_prompt,
            content_prompt,
            console: Console::new(),
        }
    }

    /// Translate a title with the title prompt, as one request without history.
    ///
    /// A reply still truncated or containing Chinese/Japanese characters after the
    /// last attempt is kept with a warning. Returns an empty string for blank input.
    pub async fn translate_title(
        &self,
        title: &str,
        progress_info: Option<ProgressInfo>,
    ) -> Result<String, TranslationError> {
        if title.trim().is_empty() {
            return Ok(String::new());
        }

        let snippet = if title.chars().count() > 30 {
            let truncated: String = title.chars().take(30).collect();
            format!("{}...", truncated)
        } else {
            title.to_string()
        };
        self.report_title_progress(progress_info.as_ref());
        if progress_info
            .as_ref()
            .and_then(|info| info.reporter.as_ref())
            .is_none()
        {
            self.console
                .info(&format!("Translating title 「{}」", snippet));
        }

        let mut history = vec![Message {
            role: "system".to_string(),
            content: self.title_prompt.clone(),
        }];

        let label = format!(
            "Chapter {} title",
            progress_info.as_ref().map_or(0, |p| p.chapter)
        );
        let reply = self
            .translate_with_retries(title, &mut history, progress_info, &label, None)
            .await?;
        if !reply.issues.is_empty() {
            self.console.warning(&format!(
                "{}: kept translation: {}",
                label,
                describe_issues(&reply.issues)
            ));
        }
        Ok(reply.text)
    }

    /// Translate chapter content chunk by chunk, keeping the last `history_length`
    /// exchanges as context.
    ///
    /// The system prompt is rendered once from `ctx`, so it is identical for every
    /// chunk. Each reply is checked with [`qa::check_chunk`] against the chunk, the
    /// glossary in `ctx` and `known_names` (English name renderings); see
    /// [`Translator::translate_chunks`]. A chunk refused on every attempt becomes
    /// `[TRANSLATION FAILED]` and its source, with an [`IssueKind::Failed`] issue, so
    /// the rest of the chapter survives; other errors fail the whole call. Returns no
    /// chunks for blank input.
    pub async fn translate_content(
        &self,
        text: &str,
        ctx: &PromptContext,
        known_names: &[String],
        progress_info: Option<ProgressInfo>,
    ) -> Result<Vec<ChunkResult>, TranslationError> {
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }

        let plan = self
            .split_text_into_chunks(text)
            .into_iter()
            .map(ChunkPlan::Translate)
            .collect();
        let results = self
            .translate_chunks(plan, ctx, known_names, progress_info)
            .await?;
        Ok(results.into_iter().map(|(_, result)| result).collect())
    }

    /// Translate the [`ChunkPlan::Translate`] chunks of a chapter in order.
    ///
    /// [`ChunkPlan::Keep`] chunks are not sent; they enter the history as if just
    /// translated, so a re-translated chunk sees the same preceding context as in a
    /// full run (a kept `[TRANSLATION FAILED]` chunk clears the history, as a refusal
    /// does). Returns `(index in plan, result)` for each translated chunk.
    ///
    /// After each reply, QA issues are merged with the translator's own; when one
    /// of them is in `qa.retry_on` and `qa.max_retries` allows, the chunk is sent
    /// again with [`qa::requirements`] in the user turn, and the attempt with the
    /// lowest [`qa::severity`] is kept.
    pub async fn translate_chunks(
        &self,
        plan: Vec<ChunkPlan>,
        ctx: &PromptContext,
        known_names: &[String],
        progress_info: Option<ProgressInfo>,
    ) -> Result<Vec<(usize, ChunkResult)>, TranslationError> {
        let total_chunks = plan.len() as u32;
        let mut results = Vec::new();
        let mut history = vec![Message {
            role: "system".to_string(),
            content: ctx.system_prompt(&self.content_prompt),
        }];

        for (i, chunk) in plan.into_iter().enumerate() {
            let chunk = match chunk {
                ChunkPlan::Translate(source) => source,
                ChunkPlan::Keep { source, output } => {
                    if output.starts_with(FAILED_MARKER) {
                        history.truncate(1);
                    } else {
                        self.record_exchange(&mut history, &source, &output);
                    }
                    continue;
                }
            };
            let chunk_num = (i + 1) as u32;
            let progress = progress_info.as_ref().map(|p| ProgressInfo {
                chapter: p.chapter,
                chunk: chunk_num,
                total_chunks,
                worker: p.worker,
                reporter: p.reporter.clone(),
            });

            // Show "Preparing..." status before starting a chunk (except the first sent)
            if !results.is_empty() {
                self.display_preparing(progress.as_ref());
            }

            let label = format!(
                "Chapter {}, chunk {}",
                progress.as_ref().map_or(0, |p| p.chapter),
                chunk_num
            );
            let qa_ctx = ChunkContext {
                source: &chunk,
                glossary: &ctx.glossary,
                known_names,
                output_rules: ctx.output_rules.as_deref(),
            };
            let result = match self
                .translate_with_retries(&chunk, &mut history, progress, &label, Some(&qa_ctx))
                .await
            {
                Ok(reply) => ChunkResult {
                    source: chunk,
                    output: reply.text,
                    issues: reply.issues,
                },
                Err(TranslationError::Refused(reason)) => ChunkResult {
                    output: format!("{FAILED_MARKER}\n{chunk}"),
                    source: chunk,
                    issues: vec![Issue {
                        kind: IssueKind::Failed,
                        detail: reason,
                    }],
                },
                Err(e) => return Err(e),
            };
            results.push((i, result));
        }

        if progress_info
            .as_ref()
            .and_then(|info| info.reporter.as_ref())
            .is_none()
        {
            print!("\r\x1b[2K");
            let _ = io::stdout().flush();
        }

        Ok(results)
    }

    /// `max_tokens` for a request translating `text`: `max_tokens_per_char` × its
    /// character count, at least [`MIN_MAX_TOKENS`]; `None` when the factor is 0.
    fn max_tokens_for(&self, text: &str) -> Option<u32> {
        let per_char = self.translation_config.max_tokens_per_char;
        if per_char <= 0.0 {
            return None;
        }
        // `as` saturates, so an absurd factor cannot wrap around.
        let tokens = (per_char * text.chars().count() as f64).ceil() as u32;
        Some(tokens.max(MIN_MAX_TOKENS))
    }

    /// Split content into the chunks [`Translator::translate_content`] sends: at most
    /// `chunk_size_chars` characters each, preferring scene breaks.
    pub fn split_text_into_chunks(&self, text: &str) -> Vec<String> {
        crate::utils::split_into_scene_chunks(text, self.translation_config.chunk_size_chars)
    }

    /// Translate one request and add the reply to the history.
    ///
    /// Without `qa_ctx` (titles) this is [`Translator::request_with_retries`]. With it,
    /// the reply's QA issues ([`qa::check_chunk`]) are merged with the translator's
    /// own (a kind the translator already reported is not repeated). While the latest
    /// attempt has an issue in `qa.retry_on` and `qa.max_retries` allows, the text is
    /// sent again with [`qa::requirements`] prepended to the user turn
    /// (`<requirements>\n\nPassage:\n<text>`); the failed attempt never enters the
    /// history. The attempt with the lowest [`qa::severity`] is kept (the later one on
    /// a tie); a QA retry that fails outright keeps the best earlier attempt. The
    /// history records `text` itself, without requirements.
    async fn translate_with_retries(
        &self,
        text: &str,
        history: &mut Vec<Message>,
        progress: Option<ProgressInfo>,
        label: &str,
        qa_ctx: Option<&ChunkContext<'_>>,
    ) -> Result<AcceptedReply, TranslationError> {
        let mut best = self
            .request_with_retries(text, text, history, progress.clone(), label)
            .await?;
        if let Some(qa_ctx) = qa_ctx {
            self.add_qa_issues(qa_ctx, &mut best);
            let mut latest_issues = best.issues.clone();
            let max_retries = self.qa_config.max_retries;
            for qa_attempt in 1..=max_retries {
                if !latest_issues
                    .iter()
                    .any(|issue| self.qa_config.retries_on(issue.kind))
                {
                    break;
                }
                let user_turn = match qa::requirements(qa_ctx, &latest_issues) {
                    Some(requirements) => format!("{requirements}\n\nPassage:\n{text}"),
                    None => text.to_string(),
                };
                self.console.warning(&format!(
                    "{}: {}; QA retry {}/{}",
                    label,
                    describe_issues(&latest_issues),
                    qa_attempt,
                    max_retries
                ));
                match self
                    .request_with_retries(text, &user_turn, history, progress.clone(), label)
                    .await
                {
                    Ok(mut reply) => {
                        self.add_qa_issues(qa_ctx, &mut reply);
                        latest_issues = reply.issues.clone();
                        if qa::severity(&reply.issues) <= qa::severity(&best.issues) {
                            best = reply;
                        }
                    }
                    Err(error) => {
                        self.console.warning(&format!(
                            "{label}: QA retry failed ({error}); keeping the earlier attempt"
                        ));
                        break;
                    }
                }
            }
        }
        self.record_exchange(history, text, &best.text);
        Ok(best)
    }

    /// Append [`qa::check_chunk`] and [`qa::check_honorifics`] issues to a reply's,
    /// skipping kinds it already has.
    fn add_qa_issues(&self, qa_ctx: &ChunkContext<'_>, reply: &mut AcceptedReply) {
        let own: Vec<IssueKind> = reply.issues.iter().map(|issue| issue.kind).collect();
        let mut issues = qa::check_chunk(qa_ctx, &reply.text, &self.qa_config);
        if let Some(rules) = qa_ctx.output_rules {
            issues.extend(qa::check_honorifics(&reply.text, rules));
        }
        reply.issues.extend(
            issues
                .into_iter()
                .filter(|issue| !own.contains(&issue.kind)),
        );
    }

    /// Send `user_turn` after `history`, retrying up to `translation.retries`
    /// attempts; `text` is the passage it translates and sizes `max_tokens`.
    ///
    /// A reply cut at the token limit or containing Chinese/Japanese characters is
    /// retried at once; on the last attempt it is kept and its problems are returned
    /// as issues. The history is not extended. A refusal (including an empty reply)
    /// clears the conversation history before the next attempt and is returned as-is
    /// once attempts run out; any other failure becomes
    /// [`TranslationError::RetriesExhausted`].
    async fn request_with_retries(
        &self,
        text: &str,
        user_turn: &str,
        history: &mut Vec<Message>,
        progress: Option<ProgressInfo>,
        label: &str,
    ) -> Result<AcceptedReply, TranslationError> {
        let attempts = self.translation_config.retries.max(1);
        let max_tokens = self.max_tokens_for(text);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let error = match self
                .translate_single_chunk(user_turn, history, max_tokens, progress.clone())
                .await
            {
                Ok(reply) => {
                    let issues = reply.issues(max_tokens);
                    if !issues.is_empty() && attempt < attempts {
                        self.console.warning(&format!(
                            "{}: {}; retrying (attempt {}/{})",
                            label,
                            describe_issues(&issues),
                            attempt + 1,
                            attempts
                        ));
                        continue;
                    }
                    // Keep the last attempt rather than abort the chapter over a
                    // stray word or a cut-off ending; the issues let callers flag it.
                    self.delay_between_requests().await;
                    return Ok(AcceptedReply {
                        text: reply.text,
                        issues,
                    });
                }
                Err(e) => e,
            };

            let refused = matches!(error, TranslationError::Refused(_));
            if refused {
                history.truncate(1);
            }

            if attempt >= attempts {
                self.console.error(&format!(
                    "{}: translation failed after all retries: {}",
                    label, error
                ));
                return Err(if refused {
                    error
                } else {
                    TranslationError::RetriesExhausted { attempts }
                });
            }

            // Exponential backoff
            let delay = Duration::from_secs(2u64.pow(attempt));
            self.console.warning(&format!(
                "{}: {}; retrying in {:?} (attempt {}/{})",
                label,
                error,
                delay,
                attempt + 1,
                attempts
            ));
            tokio::time::sleep(delay).await;
        }
    }

    /// Send one request and stream the reply.
    ///
    /// Empty replies and refusals become [`TranslationError::Refused`]; the history is
    /// not modified.
    async fn translate_single_chunk(
        &self,
        chunk: &str,
        history: &[Message],
        max_tokens: Option<u32>,
        progress_info: Option<ProgressInfo>,
    ) -> Result<StreamedReply, TranslationError> {
        // Add user message to history for this request
        let mut messages = history.to_vec();
        messages.push(Message {
            role: "user".to_string(),
            content: chunk.to_string(),
        });

        // Build request
        let request = ChatRequest {
            model: self.api_config.model.clone(),
            messages,
            stream: true,
            max_tokens,
        };

        // Make streaming request
        let url = format!("{}/chat/completions", self.api_config.base_url);
        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_config.key))
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await?;

        let response = crate::utils::check_response_status(response).await?;

        // Stream and accumulate response
        let mut full_response = String::new();
        let start_time = Instant::now();
        let mut last_update = Instant::now();

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut done = false;
        let mut finish_reason = None;

        'streaming: loop {
            let chunk_result = stream.next().await;
            let Some(chunk_result) = chunk_result else {
                break;
            };
            let bytes = chunk_result?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            // Parse SSE data lines (handle partial lines across chunks)
            while let Some(pos) = buffer.find('\n') {
                let mut line = buffer[..pos].to_string();
                buffer = buffer[pos + 1..].to_string();

                if line.ends_with('\r') {
                    line.pop();
                }

                let data = if let Some(data) = line.strip_prefix("data: ") {
                    Some(data)
                } else if let Some(data) = line.strip_prefix("data:") {
                    Some(data.trim_start())
                } else {
                    None
                };

                let Some(data) = data else {
                    continue;
                };

                if data.trim() == "[DONE]" {
                    done = true;
                    break;
                }

                // Try to parse as JSON
                if let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) {
                    for choice in chunk.choices {
                        if choice.finish_reason.is_some() {
                            finish_reason = choice.finish_reason;
                        }
                        if let Some(delta) = choice.delta
                            && let Some(content) = delta.content
                        {
                            full_response.push_str(&content);

                            // Update progress display every second
                            if last_update.elapsed() >= Duration::from_secs(1) {
                                self.report_progress(
                                    &full_response,
                                    start_time.elapsed(),
                                    progress_info.as_ref(),
                                );
                                last_update = Instant::now();
                            }
                        }
                    }
                }
            }

            if done {
                break 'streaming;
            }
        }

        if !full_response.is_empty() {
            self.report_progress(&full_response, start_time.elapsed(), progress_info.as_ref());
        }

        // Note: Progress line is NOT cleared here to maintain continuity.
        // The next chunk's "Preparing..." message will replace it, or
        // the caller will clear it when all chunks are done.

        // Validate response
        let trimmed = full_response.trim().to_string();

        if trimmed.is_empty() {
            return Err(TranslationError::Refused("Empty response".to_string()));
        }

        if crate::utils::is_refusal(&trimmed) {
            return Err(TranslationError::Refused(
                "Response is a refusal instead of a translation".to_string(),
            ));
        }

        Ok(StreamedReply {
            text: trimmed,
            finish_reason,
        })
    }

    /// Wait `delay_between_requests_sec` after an accepted reply.
    async fn delay_between_requests(&self) {
        if self.translation_config.delay_between_requests_sec > 0.0 {
            tokio::time::sleep(Duration::from_secs_f64(
                self.translation_config.delay_between_requests_sec,
            ))
            .await;
        }
    }

    /// Append a translated chunk to the history, keeping the system message and
    /// the last `history_length` exchanges.
    fn record_exchange(&self, history: &mut Vec<Message>, chunk: &str, translated: &str) {
        history.push(Message {
            role: "user".to_string(),
            content: chunk.to_string(),
        });
        history.push(Message {
            role: "assistant".to_string(),
            content: translated.to_string(),
        });

        let max_messages = 1 + (self.translation_config.history_length * 2);
        if history.len() > max_messages {
            let remove_count = history.len() - max_messages;
            history.drain(1..1 + remove_count);
        }
    }

    fn report_progress(
        &self,
        response: &str,
        elapsed: Duration,
        progress_info: Option<&ProgressInfo>,
    ) {
        if let Some(info) = progress_info
            && let (Some(worker), Some(reporter)) = (info.worker, info.reporter.as_ref())
        {
            let _ = reporter.send(TranslationProgress {
                worker,
                chapter: info.chapter,
                chunk: info.chunk,
                total_chunks: info.total_chunks,
                chars: response.chars().count(),
                elapsed_secs: elapsed.as_secs_f64(),
                stage: if info.total_chunks == 0 {
                    TranslationStage::Title
                } else {
                    TranslationStage::Content
                },
            });
            return;
        }

        self.display_progress(response, elapsed, progress_info);
    }

    fn report_title_progress(&self, progress_info: Option<&ProgressInfo>) {
        if let Some(info) = progress_info
            && let (Some(worker), Some(reporter)) = (info.worker, info.reporter.as_ref())
        {
            let _ = reporter.send(TranslationProgress {
                worker,
                chapter: info.chapter,
                chunk: 0,
                total_chunks: 0,
                chars: 0,
                elapsed_secs: 0.0,
                stage: TranslationStage::Title,
            });
        }
    }

    /// Display progress during streaming.
    fn display_progress(
        &self,
        response: &str,
        elapsed: Duration,
        progress_info: Option<&ProgressInfo>,
    ) {
        let char_count = response.chars().count();
        let speed = if elapsed.as_secs_f64() > 0.0 {
            (char_count as f64 / elapsed.as_secs_f64()) as u32
        } else {
            0
        };

        let preview: String = response
            .chars()
            .rev()
            .take(50)
            .collect::<String>()
            .chars()
            .rev()
            .map(|c| if c == '\n' { ' ' } else { c })
            .collect();

        let progress_prefix = if let Some(info) = progress_info {
            format!(
                "\x1b[1;36m[Chapter {}, Chunk {}/{}]\x1b[0m ",
                info.chapter, info.chunk, info.total_chunks
            )
        } else {
            String::new()
        };

        print!(
            "\r\x1b[2K{}Progress: \x1b[1;32m{}\x1b[0m chars at \x1b[1;33m{}/sec\x1b[0m. \x1b[90m{}...\x1b[0m",
            progress_prefix, char_count, speed, preview
        );
        let _ = io::stdout().flush();
    }

    /// Display "Preparing..." status between chunks.
    fn display_preparing(&self, progress_info: Option<&ProgressInfo>) {
        if let Some(info) = progress_info
            && let (Some(worker), Some(reporter)) = (info.worker, info.reporter.as_ref())
        {
            let _ = reporter.send(TranslationProgress {
                worker,
                chapter: info.chapter,
                chunk: info.chunk,
                total_chunks: info.total_chunks,
                chars: 0,
                elapsed_secs: 0.0,
                stage: TranslationStage::Preparing,
            });
            return;
        }

        let progress_prefix = if let Some(info) = progress_info {
            format!(
                "\x1b[1;36m[Chapter {}, Chunk {}/{}]\x1b[0m ",
                info.chapter, info.chunk, info.total_chunks
            )
        } else {
            String::new()
        };

        print!(
            "\r\x1b[2K{}Progress: \x1b[1;33mPreparing to translate chunk {}/{}\x1b[0m",
            progress_prefix,
            progress_info.map(|p| p.chunk).unwrap_or(1),
            progress_info.map(|p| p.total_chunks).unwrap_or(1)
        );
        let _ = io::stdout().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::honorifics::HonorificPolicy;
    use crate::name_mapping::Gender;

    fn make_translator() -> Translator {
        Translator::new(
            ApiConfig::default(),
            TranslationConfig::default(),
            QaConfig::default(),
            "Translate this title".to_string(),
            "Translate this content".to_string(),
        )
    }

    #[test]
    fn test_split_text_simple() {
        let translator = make_translator();
        let text = "Line 1\nLine 2\nLine 3";
        let chunks = translator.split_text_into_chunks(text);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], text);
    }

    #[test]
    fn test_system_prompt_orders_sections_and_includes_glossary() {
        let ctx = PromptContext {
            glossary: vec![TermEntry {
                original: "魔石".to_string(),
                english: "Magic Stone".to_string(),
            }],
            cast: vec![CastEntry {
                english: "Kirigaya".to_string(),
                gender: Some(Gender::Male),
                note: Some("Timer Association liaison".to_string()),
            }],
            previously: vec![
                (3, "Ruri arrives.".to_string()),
                (4, "Tatsuya leaves.".to_string()),
            ],
            output_rules: Some(Arc::new(names_only_rules())),
        };

        let prompt = ctx.system_prompt("Translate this content");

        let rules = prompt.find(ENGLISH_ONLY_RULE).unwrap();
        let ruby = prompt.find(RUBY_RULE).unwrap();
        let honorifics = prompt
            .find(honorifics::prompt_line(HonorificPolicy::NamesOnly))
            .unwrap();
        let cast = prompt.find("Characters in this chapter").unwrap();
        let glossary = prompt
            .find("Use these canonical terminology renderings exactly")
            .unwrap();
        let previously = prompt.find("Story so far").unwrap();
        assert!(prompt.starts_with("Translate this content\n\n"));
        assert!(
            rules < ruby
                && ruby < honorifics
                && honorifics < cast
                && cast < glossary
                && glossary < previously
        );
        assert!(prompt.contains(&format!(
            "{RUBY_RULE}\n\n{}\n\nCharacters in this chapter",
            honorifics::prompt_line(HonorificPolicy::NamesOnly)
        )));
        assert!(prompt.contains("- 魔石 → Magic Stone\n\nStory so far"));
        assert!(prompt.ends_with(
            "Story so far (context only; do not translate):\n- Ch 3: Ruri arrives.\n- Ch 4: Tatsuya leaves."
        ));
    }

    fn names_only_rules() -> OutputRules {
        OutputRules::new(HonorificPolicy::NamesOnly, ["Ruri"], &[]).unwrap()
    }

    #[test]
    fn test_system_prompt_renders_each_honorific_policy_after_rules() {
        for policy in [
            HonorificPolicy::Keep,
            HonorificPolicy::Drop,
            HonorificPolicy::NamesOnly,
        ] {
            let ctx = PromptContext {
                output_rules: Some(Arc::new(
                    OutputRules::new(policy, Vec::<String>::new(), &[]).unwrap(),
                )),
                ..PromptContext::default()
            };
            assert_eq!(
                ctx.system_prompt("Base"),
                format!(
                    "Base\n\n{ENGLISH_ONLY_RULE}\n{RUBY_RULE}\n\n{}",
                    honorifics::prompt_line(policy)
                )
            );
        }
    }

    #[test]
    fn test_system_prompt_renders_cast_entries_in_order() {
        let entry = |english: &str, gender, note: Option<&str>| CastEntry {
            english: english.to_string(),
            gender,
            note: note.map(str::to_string),
        };
        let ctx = PromptContext {
            cast: vec![
                entry("Kirigaya", Some(Gender::Male), Some("Timer Association liaison")),
                entry("Ruri", Some(Gender::Female), None),
                entry("Monika", None, Some("village auditor")),
            ],
            ..PromptContext::default()
        };

        assert_eq!(
            ctx.system_prompt("Base"),
            format!(
                "Base\n\n{ENGLISH_ONLY_RULE}\n{RUBY_RULE}\n\n\
                 Characters in this chapter (use matching pronouns even where the Japanese omits the subject):\n\
                 - Kirigaya: male; Timer Association liaison\n\
                 - Ruri: female\n\
                 - Monika: village auditor"
            )
        );
    }

    #[test]
    fn test_system_prompt_without_glossary_ends_with_rules() {
        let prompt = PromptContext::default().system_prompt("Base");

        assert_eq!(prompt, format!("Base\n\n{ENGLISH_ONLY_RULE}\n{RUBY_RULE}"));
    }

    #[test]
    fn test_split_text_by_lines() {
        let config = TranslationConfig {
            chunk_size_chars: 20,
            ..Default::default()
        };

        let translator = Translator::new(
            ApiConfig::default(),
            config,
            QaConfig::default(),
            String::new(),
            String::new(),
        );

        let text = "Line one here\nLine two here\nLine three here";
        let chunks = translator.split_text_into_chunks(text);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 20, "{chunk}");
        }
    }

    #[test]
    fn test_message_history_structure() {
        let msg = Message {
            role: "user".to_string(),
            content: "Hello".to_string(),
        };

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\""));
        assert!(json.contains("\"content\""));
    }

    #[test]
    fn test_progress_info() {
        let info = ProgressInfo {
            chapter: 1,
            chunk: 2,
            total_chunks: 5,
            worker: None,
            reporter: None,
        };

        assert_eq!(info.chapter, 1);
        assert_eq!(info.chunk, 2);
        assert_eq!(info.total_chunks, 5);
    }

    #[test]
    fn test_title_truncation_with_multibyte_chars() {
        // This test verifies that title truncation handles UTF-8 characters correctly
        // The panic was: "byte index 30 is not a char boundary; it is inside 'の' (bytes 29..32)"
        // This happened because the old code checked text.len() (bytes) but sliced assuming ASCII

        // Japanese text with multi-byte characters
        // This specific title: "３２．KeisukeとRika、恋の行方"
        // Has 20 characters but 38 bytes (some chars are 3 bytes each)
        let title = "３２．KeisukeとRika、恋の行方";

        let char_count = title.chars().count();
        let byte_count = title.len();

        // This title has more BYTES than 30, but fewer CHARACTERS
        assert_eq!(char_count, 20);
        assert!(byte_count > 30, "Title byte length: {}", byte_count);

        // The OLD buggy code would do:
        // if text.len() > 30 { &text[..30] }  <- PANIC! Slices at byte 30 (inside 'の')
        //
        // The NEW fixed code does:
        // if text.chars().count() > 30 { text.chars().take(30).collect() }  <- OK!

        // Since this title has only 20 chars, the fixed code won't truncate it
        let snippet = if title.chars().count() > 30 {
            let truncated: String = title.chars().take(30).collect();
            format!("{}...", truncated)
        } else {
            title.to_string()
        };

        // Should NOT be truncated (only 20 chars)
        assert_eq!(snippet, title);
        assert!(!snippet.ends_with("..."));

        // Now test with a title that's actually longer than 30 characters
        let long_title =
            "これは非常に長いタイトルでテストのために使用されます。三十文字以上あります。";
        assert!(long_title.chars().count() > 30);

        let snippet_long = if long_title.chars().count() > 30 {
            let truncated: String = long_title.chars().take(30).collect();
            format!("{}...", truncated)
        } else {
            long_title.to_string()
        };

        assert!(snippet_long.ends_with("..."));
        assert_eq!(snippet_long.chars().count(), 33); // 30 chars + "..."
    }

    /// Serve one streamed completion per connection, returning the request bodies.
    ///
    /// Each reply is sent as a content event followed, when given, by a final event
    /// carrying only its `finish_reason`, as OpenAI-compatible servers stream it.
    fn serve_replies(
        replies: Vec<(&'static str, Option<&'static str>)>,
    ) -> (String, std::thread::JoinHandle<Vec<serde_json::Value>>) {
        use std::io::{BufRead, BufReader, Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut bodies = Vec::new();
            for (reply, finish_reason) in replies {
                let (mut socket, _) = listener.accept().unwrap();
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
                bodies.push(serde_json::from_slice(&body).unwrap());

                let content = serde_json::json!({ "choices": [{ "index": 0, "delta": { "content": reply }, "finish_reason": null }] });
                let mut events = format!("data: {content}\n\n");
                if let Some(finish_reason) = finish_reason {
                    let last = serde_json::json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": finish_reason }] });
                    events.push_str(&format!("data: {last}\n\n"));
                }
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{events}data: [DONE]\n\n"
                )
                .unwrap();
            }
            bodies
        });
        (base_url, server)
    }

    /// Serve replies that each finish normally (`"stop"`).
    fn serve_streamed_replies(
        replies: &'static [&'static str],
    ) -> (String, std::thread::JoinHandle<Vec<serde_json::Value>>) {
        serve_replies(replies.iter().map(|reply| (*reply, Some("stop"))).collect())
    }

    /// A translator for the stub server with QA retries off, so tests of the
    /// translator's own retries see exactly their replies.
    fn translator_with(base_url: String, translation_config: TranslationConfig) -> Translator {
        translator_with_qa(
            base_url,
            translation_config,
            QaConfig {
                max_retries: 0,
                ..QaConfig::default()
            },
        )
    }

    fn translator_with_qa(
        base_url: String,
        translation_config: TranslationConfig,
        qa_config: QaConfig,
    ) -> Translator {
        Translator::new(
            ApiConfig {
                base_url,
                ..ApiConfig::default()
            },
            TranslationConfig {
                delay_between_requests_sec: 0.0,
                ..translation_config
            },
            qa_config,
            "Translate this title".to_string(),
            "Translate this content".to_string(),
        )
    }

    fn translator_for(base_url: String, retries: u32) -> Translator {
        translator_with(
            base_url,
            TranslationConfig {
                retries,
                ..TranslationConfig::default()
            },
        )
    }

    fn roles(body: &serde_json::Value) -> Vec<&str> {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["role"].as_str().unwrap())
            .collect()
    }

    fn kinds(chunk: &ChunkResult) -> Vec<IssueKind> {
        chunk.issues.iter().map(|issue| issue.kind).collect()
    }

    #[tokio::test]
    async fn test_chinese_leak_is_retried_without_polluting_history() {
        let (base_url, server) =
            serve_streamed_replies(&["He was a 杀人狂 (murderer).", "He was a murderer."]);
        let translator = translator_for(base_url, 3);

        let chunks = translator
            .translate_content("彼は殺人鬼だった。", &PromptContext::default(), &[], None)
            .await
            .unwrap();
        let bodies = server.join().unwrap();

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].source, "彼は殺人鬼だった。");
        assert_eq!(chunks[0].output, "He was a murderer.");
        assert!(chunks[0].issues.is_empty());
        assert_eq!(bodies.len(), 2);
        assert_eq!(roles(&bodies[1]), ["system", "user"]);
    }

    #[tokio::test]
    async fn test_persistent_chinese_leak_keeps_last_attempt() {
        let (base_url, server) = serve_streamed_replies(&["A 杀人狂.", "Another 杀人狂."]);
        let translator = translator_for(base_url, 2);

        let chunks = translator
            .translate_content("殺人鬼。", &PromptContext::default(), &[], None)
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(chunks[0].output, "Another 杀人狂.");
        assert_eq!(kinds(&chunks[0]), [IssueKind::ForeignScript]);
    }

    #[tokio::test]
    async fn test_empty_title_reply_is_retried() {
        let (base_url, server) = serve_streamed_replies(&["", "Starting to Move"]);
        let translator = translator_for(base_url, 3);

        let translated = translator.translate_title("動き出す", None).await.unwrap();
        let bodies = server.join().unwrap();

        assert_eq!(translated, "Starting to Move");
        assert_eq!(bodies.len(), 2);
    }

    #[tokio::test]
    async fn test_refused_chunk_keeps_source_with_failed_issue() {
        let (base_url, server) = serve_streamed_replies(&[""]);
        let translator = translator_for(base_url, 1);

        let chunks = translator
            .translate_content("原文。", &PromptContext::default(), &[], None)
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(chunks[0].output, "[TRANSLATION FAILED]\n原文。");
        assert_eq!(kinds(&chunks[0]), [IssueKind::Failed]);
    }

    #[tokio::test]
    async fn test_requests_carry_max_tokens_from_source_length() {
        let (base_url, server) = serve_streamed_replies(&["A title", "Ah."]);
        let translator = translator_for(base_url, 1);
        let content = "あ".repeat(300);

        translator.translate_title("題", None).await.unwrap();
        translator
            .translate_content(&content, &PromptContext::default(), &[], None)
            .await
            .unwrap();
        let bodies = server.join().unwrap();

        // Titles get the floor; 300 chars × 2.0 = 600 tokens.
        assert_eq!(bodies[0]["max_tokens"], MIN_MAX_TOKENS);
        assert_eq!(bodies[1]["max_tokens"], 600);
    }

    #[tokio::test]
    async fn test_zero_max_tokens_per_char_omits_max_tokens() {
        let (base_url, server) = serve_streamed_replies(&["Ah."]);
        let translator = translator_with(
            base_url,
            TranslationConfig {
                max_tokens_per_char: 0.0,
                ..TranslationConfig::default()
            },
        );

        translator
            .translate_content("あ", &PromptContext::default(), &[], None)
            .await
            .unwrap();
        let bodies = server.join().unwrap();

        assert!(bodies[0].get("max_tokens").is_none());
    }

    #[tokio::test]
    async fn test_truncated_reply_is_retried_without_polluting_history() {
        let (base_url, server) = serve_replies(vec![
            ("He was", Some("length")),
            ("He was a murderer.", Some("stop")),
        ]);
        let translator = translator_for(base_url, 3);

        let chunks = translator
            .translate_content("彼は殺人鬼だった。", &PromptContext::default(), &[], None)
            .await
            .unwrap();
        let bodies = server.join().unwrap();

        assert_eq!(chunks[0].output, "He was a murderer.");
        assert!(chunks[0].issues.is_empty());
        assert_eq!(bodies.len(), 2);
        assert_eq!(roles(&bodies[1]), ["system", "user"]);
    }

    #[tokio::test]
    async fn test_persistent_truncation_is_kept_as_truncated_issue() {
        let (base_url, server) =
            serve_replies(vec![("He", Some("length")), ("He was", Some("length"))]);
        let translator = translator_for(base_url, 2);

        let chunks = translator
            .translate_content("彼は殺人鬼だった。", &PromptContext::default(), &[], None)
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(chunks[0].output, "He was");
        // QA adds the short output's length ratio to the translator's own issue.
        assert_eq!(
            kinds(&chunks[0]),
            [IssueKind::Truncated, IssueKind::LengthRatio]
        );
    }

    #[tokio::test]
    async fn test_system_prompt_is_identical_across_chunks() {
        let text = "一二三四五六七八\n九十一二三四五六";
        let (base_url, server) = serve_streamed_replies(&["One.", "Two."]);
        let translator = translator_with(
            base_url,
            TranslationConfig {
                chunk_size_chars: 10,
                ..TranslationConfig::default()
            },
        );
        assert_eq!(translator.split_text_into_chunks(text).len(), 2);
        let ctx = PromptContext {
            glossary: vec![TermEntry {
                original: "魔石".to_string(),
                english: "Magic Stone".to_string(),
            }],
            cast: vec![CastEntry {
                english: "Kirigaya".to_string(),
                gender: Some(Gender::Male),
                note: None,
            }],
            previously: vec![(1, "Kirigaya meets Ruri.".to_string())],
            output_rules: Some(Arc::new(names_only_rules())),
        };

        let chunks = translator
            .translate_content(text, &ctx, &[], None)
            .await
            .unwrap();
        let bodies = server.join().unwrap();

        assert_eq!(chunks.len(), 2);
        assert_eq!(roles(&bodies[1]), ["system", "user", "assistant", "user"]);
        assert_eq!(bodies[0]["messages"][0], bodies[1]["messages"][0]);
        assert_eq!(
            bodies[0]["messages"][0]["content"],
            ctx.system_prompt("Translate this content")
        );
    }

    fn magic_stone_context() -> PromptContext {
        PromptContext {
            glossary: vec![TermEntry {
                original: "魔石".to_string(),
                english: "Magic Stone".to_string(),
            }],
            ..PromptContext::default()
        }
    }

    #[tokio::test]
    async fn test_glossary_miss_triggers_one_qa_retry_with_requirements_in_user_turn() {
        let text = "魔石を拾った。\n次の日。";
        let (base_url, server) = serve_streamed_replies(&[
            "He picked up a stone.",
            "He picked up a Magic Stone.",
            "The next day.",
        ]);
        let translator = translator_with_qa(
            base_url,
            TranslationConfig {
                chunk_size_chars: 8,
                ..TranslationConfig::default()
            },
            QaConfig::default(),
        );
        let ctx = magic_stone_context();

        let chunks = translator
            .translate_content(text, &ctx, &[], None)
            .await
            .unwrap();
        let bodies = server.join().unwrap();

        assert_eq!(bodies.len(), 3);
        assert_eq!(chunks[0].output, "He picked up a Magic Stone.");
        assert!(chunks[0].issues.is_empty(), "{:?}", chunks[0].issues);
        // The retry carries the requirements in the user turn, not in history.
        assert_eq!(roles(&bodies[1]), ["system", "user"]);
        assert_eq!(
            bodies[1]["messages"][1]["content"],
            "Requirements for this passage: render 魔石 as \"Magic Stone\".\n\nPassage:\n魔石を拾った。"
        );
        // The next chunk sees the plain chunk and the accepted reply only.
        assert_eq!(roles(&bodies[2]), ["system", "user", "assistant", "user"]);
        assert_eq!(bodies[2]["messages"][1]["content"], "魔石を拾った。");
        assert_eq!(
            bodies[2]["messages"][2]["content"],
            "He picked up a Magic Stone."
        );
        assert_eq!(bodies[0]["messages"][0], bodies[1]["messages"][0]);
    }

    #[tokio::test]
    async fn test_exhausted_qa_budget_keeps_best_attempt_and_its_issue() {
        let (base_url, server) =
            serve_streamed_replies(&["He picked up a stone.", "He picked up a 石."]);
        let translator = translator_with_qa(
            base_url,
            TranslationConfig {
                retries: 1,
                ..TranslationConfig::default()
            },
            QaConfig::default(),
        );

        let chunks = translator
            .translate_content("魔石を拾った。", &magic_stone_context(), &[], None)
            .await
            .unwrap();
        let bodies = server.join().unwrap();

        assert_eq!(bodies.len(), 2);
        assert_eq!(chunks[0].output, "He picked up a stone.");
        assert_eq!(kinds(&chunks[0]), [IssueKind::GlossaryMissing]);
    }

    #[tokio::test]
    async fn test_honorific_on_noun_is_recorded_without_a_retry() {
        let (base_url, server) = serve_streamed_replies(&["Ruri-chan fed the cat-san."]);
        let translator = translator_with_qa(
            base_url,
            TranslationConfig::default(),
            QaConfig {
                retry_on: crate::qa::IssueClass::ALL.to_vec(),
                ..QaConfig::default()
            },
        );
        let ctx = PromptContext {
            output_rules: Some(Arc::new(names_only_rules())),
            ..PromptContext::default()
        };

        let chunks = translator
            .translate_content("Ruri-chanは猫さんに餌をやった。", &ctx, &[], None)
            .await
            .unwrap();
        let bodies = server.join().unwrap();

        // Record-only: one request, the raw reply kept, the issue recorded.
        assert_eq!(bodies.len(), 1);
        assert_eq!(chunks[0].output, "Ruri-chan fed the cat-san.");
        assert_eq!(kinds(&chunks[0]), [IssueKind::HonorificOnNoun]);
        assert_eq!(chunks[0].issues[0].detail, "cat-san");
    }

    #[tokio::test]
    async fn test_name_near_miss_requirement_uses_known_names() {
        let (base_url, server) =
            serve_streamed_replies(&["Tatyuya smiled at us.", "Tatsuya smiled at us."]);
        let translator =
            translator_with_qa(base_url, TranslationConfig::default(), QaConfig::default());
        let known_names = ["Tatsuya".to_string()];

        let chunks = translator
            .translate_content(
                "Tatsuyaは笑った。",
                &PromptContext::default(),
                &known_names,
                None,
            )
            .await
            .unwrap();
        let bodies = server.join().unwrap();

        assert_eq!(chunks[0].output, "Tatsuya smiled at us.");
        assert!(chunks[0].issues.is_empty(), "{:?}", chunks[0].issues);
        assert!(
            bodies[1]["messages"][1]["content"]
                .as_str()
                .unwrap()
                .starts_with("Requirements for this passage: the name is spelled \"Tatsuya\".")
        );
    }

    #[tokio::test]
    async fn test_translate_chunks_sends_only_planned_chunks_with_kept_history() {
        let (base_url, server) = serve_streamed_replies(&["Second, again."]);
        let translator = translator_for(base_url, 1);
        let plan = vec![
            ChunkPlan::Keep {
                source: "一つ目。".to_string(),
                output: "First.".to_string(),
            },
            ChunkPlan::Translate("二つ目。".to_string()),
            ChunkPlan::Keep {
                source: "三つ目。".to_string(),
                output: "Third.".to_string(),
            },
        ];

        let results = translator
            .translate_chunks(plan, &PromptContext::default(), &[], None)
            .await
            .unwrap();
        let bodies = server.join().unwrap();

        assert_eq!(bodies.len(), 1);
        assert_eq!(roles(&bodies[0]), ["system", "user", "assistant", "user"]);
        assert_eq!(bodies[0]["messages"][1]["content"], "一つ目。");
        assert_eq!(bodies[0]["messages"][2]["content"], "First.");
        assert_eq!(bodies[0]["messages"][3]["content"], "二つ目。");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 1);
        assert_eq!(results[0].1.output, "Second, again.");
    }

    #[tokio::test]
    async fn test_kept_failed_chunk_clears_history() {
        let (base_url, server) = serve_streamed_replies(&["Second."]);
        let translator = translator_for(base_url, 1);
        let plan = vec![
            ChunkPlan::Keep {
                source: "一つ目。".to_string(),
                output: "First.".to_string(),
            },
            ChunkPlan::Keep {
                source: "拒否。".to_string(),
                output: format!("{FAILED_MARKER}\n拒否。"),
            },
            ChunkPlan::Translate("二つ目。".to_string()),
        ];

        translator
            .translate_chunks(plan, &PromptContext::default(), &[], None)
            .await
            .unwrap();
        let bodies = server.join().unwrap();

        assert_eq!(roles(&bodies[0]), ["system", "user"]);
    }
}
