//! Translation system using OpenAI-compatible APIs.
//!
//! Provides text translation with streaming progress display,
//! message history management, and retry logic.

use crate::config::{ApiConfig, TranslationConfig};
use crate::console::Console;
use crate::error::TranslationError;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

/// Refusal phrases that indicate the model declined to translate.
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
        title_prompt: String,
        content_prompt: String,
    ) -> Self {
        Self {
            client: Client::new(),
            api_config,
            translation_config,
            title_prompt,
            content_prompt,
            console: Console::new(),
        }
    }

    /// Translate text to English.
    ///
    /// # Arguments
    /// * `text` - The Japanese text to translate.
    /// * `is_title` - Whether this is a title (uses different prompt, no chunking).
    /// * `progress_info` - Optional progress information for display.
    ///
    /// # Returns
    /// The translated English text.
    pub async fn translate(
        &self,
        text: &str,
        is_title: bool,
        progress_info: Option<ProgressInfo>,
    ) -> Result<String, TranslationError> {
        // Handle empty text
        if text.trim().is_empty() {
            return Ok(String::new());
        }

        if is_title {
            // Title translation: single chunk, no history needed
            let snippet = if text.chars().count() > 30 {
                let truncated: String = text.chars().take(30).collect();
                format!("{}...", truncated)
            } else {
                text.to_string()
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

            self.translate_single_chunk(text, &mut history, progress_info)
                .await
        } else {
            // Content translation: chunk and translate with history
            let chunks = self.split_text_into_chunks(text);
            let total_chunks = chunks.len() as u32;
            let mut results = Vec::new();
            let mut history = vec![Message {
                role: "system".to_string(),
                content: self.content_prompt.clone(),
            }];

            for (i, chunk) in chunks.iter().enumerate() {
                let chunk_num = (i + 1) as u32;
                let progress = progress_info.as_ref().map(|p| ProgressInfo {
                    chapter: p.chapter,
                    chunk: chunk_num,
                    total_chunks,
                    worker: p.worker,
                    reporter: p.reporter.clone(),
                });

                // Show "Preparing..." status before starting chunk (except first)
                if i > 0 {
                    self.display_preparing(progress.as_ref());
                }

                // Retry loop for this chunk
                let mut attempt = 0;
                let mut last_error: Option<TranslationError> = None;

                while attempt < self.translation_config.retries {
                    let translation_result = self
                        .translate_single_chunk(chunk, &mut history, progress.clone())
                        .await;

                    match translation_result {
                        Ok(translated) => {
                            results.push(translated);
                            last_error = None;
                            break;
                        }
                        Err(e) => {
                            if matches!(e, TranslationError::Refused(_)) {
                                history.truncate(1);
                                self.console.warning(
                                    "Model refused translation; cleared conversation history before retry.",
                                );
                            }
                            last_error = Some(e);
                            attempt += 1;
                            if attempt < self.translation_config.retries {
                                // Exponential backoff
                                let delay = Duration::from_secs(2u64.pow(attempt));
                                self.console.warning(&format!(
                                    "Translation failed, retrying in {:?} (attempt {}/{})",
                                    delay,
                                    attempt + 1,
                                    self.translation_config.retries
                                ));
                                tokio::time::sleep(delay).await;
                            }
                        }
                    }
                }

                if let Some(e) = last_error {
                    self.console
                        .error(&format!("Translation failed after all retries: {}", e));

                    if matches!(e, TranslationError::Refused(_)) {
                        results.push(format!("[TRANSLATION FAILED]\n{}", chunk));
                        continue;
                    }

                    return Err(TranslationError::RetriesExhausted {
                        attempts: self.translation_config.retries,
                    });
                }
            }

            if progress_info
                .as_ref()
                .and_then(|info| info.reporter.as_ref())
                .is_none()
            {
                print!("\r\x1b[2K");
                let _ = io::stdout().flush();
            }

            Ok(results.join("\n\n"))
        }
    }

    /// Split text into chunks that fit within the configured size limit.
    fn split_text_into_chunks(&self, text: &str) -> Vec<String> {
        let chunk_size = self.translation_config.chunk_size_chars;

        // Phase 1: Line-based chunking
        let chunks = crate::utils::split_text_into_line_chunks(text, chunk_size);

        // Phase 2: Word-based splitting for oversized chunks
        let mut final_chunks: Vec<String> = Vec::new();

        for chunk in chunks {
            if chunk.len() <= chunk_size {
                final_chunks.push(chunk);
            } else {
                // Split by whitespace (for Japanese, this mainly handles mixed content)
                let words: Vec<&str> = chunk.split_whitespace().collect();
                let mut current_chunk: Vec<&str> = Vec::new();
                let mut current_size: usize = 0;

                for word in words {
                    let word_size = word.len() + if current_chunk.is_empty() { 0 } else { 1 };

                    if current_size + word_size > chunk_size && !current_chunk.is_empty() {
                        final_chunks.push(current_chunk.join(" "));
                        current_chunk = vec![word];
                        current_size = word.len();
                    } else {
                        current_chunk.push(word);
                        current_size += word_size;
                    }
                }

                if !current_chunk.is_empty() {
                    final_chunks.push(current_chunk.join(" "));
                }
            }
        }

        final_chunks
    }

    /// Translate a single chunk of text.
    async fn translate_single_chunk(
        &self,
        chunk: &str,
        history: &mut Vec<Message>,
        progress_info: Option<ProgressInfo>,
    ) -> Result<String, TranslationError> {
        // Add user message to history for this request
        let mut messages = history.clone();
        messages.push(Message {
            role: "user".to_string(),
            content: chunk.to_string(),
        });

        // Build request
        let request = ChatRequest {
            model: self.api_config.model.clone(),
            messages,
            stream: true,
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

        // Check for refusal phrases
        let lower = trimmed.to_lowercase();
        for phrase in REFUSAL_PHRASES.iter() {
            if lower.starts_with(phrase) {
                return Err(TranslationError::Refused(format!(
                    "Response starts with refusal phrase: {}",
                    phrase
                )));
            }
        }

        // Update history
        history.push(Message {
            role: "user".to_string(),
            content: chunk.to_string(),
        });
        history.push(Message {
            role: "assistant".to_string(),
            content: trimmed.clone(),
        });

        // Trim history to configured length (keep system message + last N pairs)
        let max_messages = 1 + (self.translation_config.history_length * 2);
        if history.len() > max_messages {
            // Keep system message (index 0) and last N pairs
            let remove_count = history.len() - max_messages;
            history.drain(1..1 + remove_count);
        }

        // Delay before next request
        if self.translation_config.delay_between_requests_sec > 0.0 {
            tokio::time::sleep(Duration::from_secs_f64(
                self.translation_config.delay_between_requests_sec,
            ))
            .await;
        }

        Ok(trimmed)
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

/// Translate text without a persistent Translator instance (convenience function).
pub async fn translate_text(
    text: &str,
    is_title: bool,
    api_config: &ApiConfig,
    translation_config: &TranslationConfig,
    title_prompt: &str,
    content_prompt: &str,
    progress_info: Option<ProgressInfo>,
) -> Result<String, TranslationError> {
    let translator = Translator::new(
        api_config.clone(),
        translation_config.clone(),
        title_prompt.to_string(),
        content_prompt.to_string(),
    );
    translator.translate(text, is_title, progress_info).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_translator() -> Translator {
        Translator::new(
            ApiConfig::default(),
            TranslationConfig::default(),
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
    fn test_split_text_by_lines() {
        let config = TranslationConfig {
            chunk_size_chars: 20,
            ..Default::default()
        };

        let translator =
            Translator::new(ApiConfig::default(), config, String::new(), String::new());

        let text = "Line one here\nLine two here\nLine three here";
        let chunks = translator.split_text_into_chunks(text);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            // Each chunk should be at most chunk_size_chars (approximately, due to line boundaries)
            assert!(chunk.len() <= 30); // Allow some leeway for line-based splitting
        }
    }

    #[test]
    fn test_refusal_detection() {
        let phrases = vec![
            "I'm sorry, but I cannot",
            "As an AI, I must decline",
            "I cannot assist with that",
        ];

        for phrase in phrases {
            let lower = phrase.to_lowercase();
            let is_refusal = REFUSAL_PHRASES.iter().any(|p| lower.starts_with(p));
            assert!(is_refusal, "Should detect refusal: {}", phrase);
        }
    }

    #[test]
    fn test_non_refusal() {
        let phrases = vec![
            "The translation is...",
            "Here is the translated text",
            "私は学生です means I am a student",
        ];

        for phrase in phrases {
            let lower = phrase.to_lowercase();
            let is_refusal = REFUSAL_PHRASES.iter().any(|p| lower.starts_with(p));
            assert!(!is_refusal, "Should not detect refusal: {}", phrase);
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
}
