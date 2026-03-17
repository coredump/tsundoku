//! Pixiv (pixiv.net/novel) scraper implementation.
//!
//! Supports downloading novels from Pixiv's novel section, including
//! both individual novels and series.

use super::{ChapterInfo, ChapterList, NovelInfo, Scraper, rate_limit};
use crate::config::Config;
use crate::config::ScrapingConfig;
use crate::cookies::load_netscape_cookie_jar;
use crate::error::ScraperError;
use async_trait::async_trait;
use regex::Regex;
use reqwest::cookie::Jar;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use std::sync::LazyLock;

/// Regex for individual novel URLs.
static INDIVIDUAL_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://www\.pixiv\.net/novel/show\.php\?id=(\d+)").unwrap());

/// Regex for series URLs.
static SERIES_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://www\.pixiv\.net/novel/series/(\d+)").unwrap());

/// Regex for Unicode escape sequences.
static UNICODE_ESCAPE_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\\u([0-9a-fA-F]{4})").unwrap());

/// URL type for Pixiv.
#[derive(Debug, Clone, PartialEq)]
enum PixivUrlType {
    Individual(String), // novel_id
    Series(String),     // series_id
}

/// API response wrapper.
#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    error: bool,
    #[serde(default)]
    message: String,
    body: Option<T>,
}

/// Novel info from API.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct NovelBody {
    id: String,
    title: String,
    content: Option<String>,
    #[serde(default)]
    series_id: Option<String>,
}

/// Series info from API.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct SeriesBody {
    id: String,
    title: String,
}

/// Series content page from API.
#[derive(Debug, Deserialize)]
struct SeriesContentBody {
    page: SeriesPage,
}

/// Series page with contents.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeriesPage {
    series_contents: Vec<SeriesContent>,
}

/// Series metadata for ordering.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeriesMetadata {
    content_order: u32,
}

/// Individual content in a series.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SeriesContent {
    id: String,
    title: Option<String>,
    series: SeriesMetadata,
}

/// Pixiv scraper for pixiv.net/novel.
pub struct PixivScraper {
    client: reqwest::Client,
    config: ScrapingConfig,
}

impl PixivScraper {
    /// Creates a new Pixiv scraper with the given configuration.
    pub fn new(config: ScrapingConfig) -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(
            "Accept",
            HeaderValue::from_static("application/json, text/javascript, */*; q=0.01"),
        );
        headers.insert(
            "Accept-Encoding",
            HeaderValue::from_static("gzip, deflate, br"),
        );
        headers.insert(
            "Accept-Language",
            HeaderValue::from_static("en-US,en;q=0.9"),
        );
        headers.insert(
            "Referer",
            HeaderValue::from_static("https://www.pixiv.net/"),
        );
        headers.insert(
            "X-Requested-With",
            HeaderValue::from_static("XMLHttpRequest"),
        );

        let cookie_jar = match Config::config_dir() {
            Ok(config_dir) => match load_netscape_cookie_jar(&config_dir, &["pixiv"]) {
                Ok((jar, source)) => {
                    if config.debug {
                        if let Some(path) = source {
                            eprintln!("[Pixiv Debug] Loaded cookie file: {}", path.display());
                        } else {
                            eprintln!("[Pixiv Debug] No cookie file found for pixiv");
                        }
                    }
                    jar
                }
                Err(err) => {
                    if config.debug {
                        eprintln!("[Pixiv Debug] Failed to load cookies: {}", err);
                    }
                    Arc::new(Jar::default())
                }
            },
            Err(err) => {
                if config.debug {
                    eprintln!("[Pixiv Debug] Could not find config dir: {}", err);
                }
                Arc::new(Jar::default())
            }
        };

        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
            .default_headers(headers)
            .cookie_provider(cookie_jar)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");

        Self { client, config }
    }

    /// Parses a Pixiv URL to determine its type.
    fn parse_url(url: &str) -> Option<PixivUrlType> {
        if let Some(caps) = INDIVIDUAL_PATTERN.captures(url) {
            return Some(PixivUrlType::Individual(caps[1].to_string()));
        }
        if let Some(caps) = SERIES_PATTERN.captures(url) {
            return Some(PixivUrlType::Series(caps[1].to_string()));
        }
        None
    }

    /// Makes an AJAX request to Pixiv's API.
    async fn make_ajax_request<T: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
    ) -> Result<T, ScraperError> {
        rate_limit(self.config.delay_between_requests_sec).await;

        let response = self.client.get(url).send().await?;
        let status = response.status();
        let headers = response.headers().clone();

        if !status.is_success() {
            if self.config.debug {
                eprintln!(
                    "[Pixiv Debug] Non-success response: url={} status={}",
                    url,
                    status.as_u16()
                );
            }
            return Err(ScraperError::HttpError(
                response.error_for_status().unwrap_err(),
            ));
        }

        // Check content type
        let content_type = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let content_encoding = headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("(none)");

        let body_bytes = response.bytes().await.map_err(|e| {
            if self.config.debug {
                log_decode_failure(url, status.as_u16(), content_type, content_encoding, &[]);
            }
            ScraperError::ParseError(format!("Failed to read response body: {}", e))
        })?;

        if self.config.debug {
            if let Ok(json_value) = serde_json::from_slice::<JsonValue>(&body_bytes) {
                eprintln!("[Pixiv Debug] JSON key dump for {}", url);
                dump_json_keys(&json_value, "root");
            }
        }

        if !content_type.contains("application/json") {
            if self.config.debug {
                log_decode_failure(
                    url,
                    status.as_u16(),
                    content_type,
                    content_encoding,
                    &body_bytes,
                );
            }
            return Err(ScraperError::ParseError(format!(
                "Expected JSON but got: {}",
                content_type
            )));
        }

        let api_response: ApiResponse<T> = serde_json::from_slice(&body_bytes).map_err(|e| {
            if self.config.debug {
                log_decode_failure(
                    url,
                    status.as_u16(),
                    content_type,
                    content_encoding,
                    &body_bytes,
                );
            }
            ScraperError::ParseError(format!("Failed to parse API response: {}", e))
        })?;

        if api_response.error {
            return Err(ScraperError::NotFound(format!(
                "API error: {}",
                api_response.message
            )));
        }

        api_response
            .body
            .ok_or_else(|| ScraperError::ParseError("API response missing body".to_string()))
    }

    /// Gets all chapters in a series with pagination.
    async fn get_all_series_chapters(
        &self,
        series_id: &str,
    ) -> Result<Vec<ChapterInfo>, ScraperError> {
        let mut all_chapters = Vec::new();
        let mut last_order = 0u32;
        let limit = 30;

        loop {
            let url = format!(
                "https://www.pixiv.net/ajax/novel/series_content/{}?limit={}&last_order={}&order_by=asc",
                series_id, limit, last_order
            );

            let body: SeriesContentBody = match self.make_ajax_request(&url).await {
                Ok(b) => b,
                Err(e) => {
                    // If we already have some chapters, just return what we have
                    if !all_chapters.is_empty() {
                        break;
                    }
                    return Err(e);
                }
            };

            let contents = body.page.series_contents;
            if contents.is_empty() {
                break;
            }

            for content in &contents {
                let title = content.title.as_deref().unwrap_or("").trim().to_string();
                let title = if title.is_empty() {
                    format!("Chapter {}", content.series.content_order)
                } else {
                    unescape_unicode(&title)
                };

                all_chapters.push(ChapterInfo {
                    title,
                    url: content.id.clone(), // Store ID as URL for later retrieval
                    number: content.series.content_order,
                });
            }

            // Check if we got less than the limit (last page)
            if contents.len() < limit as usize {
                break;
            }

            // Update last_order for next page
            last_order = contents
                .last()
                .map(|c| c.series.content_order)
                .unwrap_or(last_order);
        }

        // Sort by order to ensure correct sequence
        all_chapters.sort_by_key(|c| c.number);

        // Renumber chapters sequentially (1-based)
        for (idx, chapter) in all_chapters.iter_mut().enumerate() {
            chapter.number = (idx + 1) as u32;
        }

        Ok(all_chapters)
    }
}

fn log_decode_failure(
    url: &str,
    status: u16,
    content_type: &str,
    content_encoding: &str,
    body: &[u8],
) {
    let preview_len = body.len().min(512);
    let preview = String::from_utf8_lossy(&body[..preview_len]);
    let json_hint = serde_json::from_slice::<JsonValue>(body).ok();

    eprintln!("[Pixiv Debug] Failed to decode API response");
    eprintln!("[Pixiv Debug] URL: {}", url);
    eprintln!("[Pixiv Debug] Status: {}", status);
    eprintln!("[Pixiv Debug] Content-Type: {}", content_type);
    eprintln!("[Pixiv Debug] Content-Encoding: {}", content_encoding);
    eprintln!("[Pixiv Debug] Body length: {} bytes", body.len());
    eprintln!("[Pixiv Debug] Body preview: {}", preview);

    if let Some(value) = json_hint {
        if let Ok(pretty) = serde_json::to_string_pretty(&value) {
            eprintln!("[Pixiv Debug] Parsed JSON preview: {}", pretty);
        }
    }
}

fn dump_json_keys(value: &JsonValue, path: &str) {
    match value {
        JsonValue::Object(map) => {
            eprintln!("[Pixiv Debug] {}: object ({} keys)", path, map.len());
            for (key, child) in map {
                let next_path = format!("{}.{}", path, key);
                dump_json_keys(child, &next_path);
            }
        }
        JsonValue::Array(items) => {
            eprintln!("[Pixiv Debug] {}: array ({} items)", path, items.len());
            for (idx, child) in items.iter().enumerate() {
                let next_path = format!("{}[{}]", path, idx);
                dump_json_keys(child, &next_path);
            }
        }
        JsonValue::String(s) => {
            eprintln!("[Pixiv Debug] {}: string (len={})", path, s.chars().count());
        }
        JsonValue::Number(_) => {
            eprintln!("[Pixiv Debug] {}: number", path);
        }
        JsonValue::Bool(_) => {
            eprintln!("[Pixiv Debug] {}: bool", path);
        }
        JsonValue::Null => {
            eprintln!("[Pixiv Debug] {}: null", path);
        }
    }
}

/// Unescapes Unicode escape sequences like \u3042 to actual characters.
fn unescape_unicode(text: &str) -> String {
    if text.is_empty() || !text.contains("\\u") {
        return text.to_string();
    }

    UNICODE_ESCAPE_REGEX
        .replace_all(text, |caps: &regex::Captures| {
            let hex = &caps[1];
            u32::from_str_radix(hex, 16)
                .ok()
                .and_then(char::from_u32)
                .map(|c| c.to_string())
                .unwrap_or_else(|| caps[0].to_string())
        })
        .to_string()
}

#[async_trait]
impl Scraper for PixivScraper {
    fn name(&self) -> &'static str {
        "Pixiv"
    }

    fn id(&self) -> &'static str {
        "pixiv"
    }

    fn can_handle(&self, url: &str) -> bool {
        Self::parse_url(url).is_some()
    }

    async fn get_novel_info(&self, url: &str) -> Result<NovelInfo, ScraperError> {
        let url_type =
            Self::parse_url(url).ok_or_else(|| ScraperError::UnsupportedUrl(url.to_string()))?;

        match url_type {
            PixivUrlType::Individual(novel_id) => {
                let api_url = format!("https://www.pixiv.net/ajax/novel/{}", novel_id);
                let body: NovelBody = self.make_ajax_request(&api_url).await?;

                Ok(NovelInfo {
                    title: unescape_unicode(&body.title),
                    base_url: url.to_string(),
                    novel_id,
                })
            }
            PixivUrlType::Series(series_id) => {
                let api_url = format!("https://www.pixiv.net/ajax/novel/series/{}", series_id);
                let body: SeriesBody = self.make_ajax_request(&api_url).await?;

                Ok(NovelInfo {
                    title: unescape_unicode(&body.title),
                    base_url: url.to_string(),
                    novel_id: series_id,
                })
            }
        }
    }

    async fn get_chapter_list(&self, base_url: &str) -> Result<ChapterList, ScraperError> {
        let url_type = Self::parse_url(base_url)
            .ok_or_else(|| ScraperError::UnsupportedUrl(base_url.to_string()))?;

        match url_type {
            PixivUrlType::Individual(_) => {
                // Individual novels are one-shots
                Ok(ChapterList::OneShot)
            }
            PixivUrlType::Series(series_id) => {
                let chapters = self.get_all_series_chapters(&series_id).await?;
                Ok(ChapterList::Chapters(chapters))
            }
        }
    }

    async fn download_chapter(&self, chapter_url: &str) -> Result<String, ScraperError> {
        // chapter_url is either a full URL or just a novel ID
        let novel_id = if chapter_url.starts_with("http") {
            // Extract ID from URL
            INDIVIDUAL_PATTERN
                .captures(chapter_url)
                .and_then(|caps| caps.get(1))
                .map(|m| m.as_str().to_string())
                .ok_or_else(|| ScraperError::InvalidUrl("Could not extract novel ID".to_string()))?
        } else {
            // Already an ID
            chapter_url.to_string()
        };

        let api_url = format!("https://www.pixiv.net/ajax/novel/{}", novel_id);
        if self.config.debug {
            eprintln!(
                "[Pixiv Debug] Downloading chapter: chapter_url={} novel_id={} api_url={}",
                chapter_url, novel_id, api_url
            );
        }
        let body: NovelBody = self.make_ajax_request(&api_url).await?;
        if self.config.debug {
            eprintln!(
                "[Pixiv Debug] Novel body: id={} title={} series_id={} content_present={} content_len={}",
                body.id,
                body.title,
                body.series_id.as_deref().unwrap_or("(none)"),
                body.content.is_some(),
                body.content.as_ref().map(|c| c.len()).unwrap_or(0)
            );
        }

        let content = body
            .content
            .ok_or_else(|| ScraperError::NotFound("Novel content not found".to_string()))?;

        Ok(unescape_unicode(&content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_patterns() {
        let scraper = PixivScraper::new(ScrapingConfig::default());

        // Valid URLs
        assert!(scraper.can_handle("https://www.pixiv.net/novel/show.php?id=12345"));
        assert!(scraper.can_handle("https://www.pixiv.net/novel/series/67890"));

        // Invalid URLs
        assert!(!scraper.can_handle("https://www.pixiv.net/"));
        assert!(!scraper.can_handle("https://www.pixiv.net/artworks/12345"));
        assert!(!scraper.can_handle("https://kakuyomu.jp/works/123"));
    }

    #[test]
    fn test_parse_url() {
        assert_eq!(
            PixivScraper::parse_url("https://www.pixiv.net/novel/show.php?id=12345"),
            Some(PixivUrlType::Individual("12345".to_string()))
        );
        assert_eq!(
            PixivScraper::parse_url("https://www.pixiv.net/novel/series/67890"),
            Some(PixivUrlType::Series("67890".to_string()))
        );
        assert_eq!(PixivScraper::parse_url("https://example.com"), None);
    }

    #[test]
    fn test_unescape_unicode() {
        assert_eq!(unescape_unicode("Hello"), "Hello");
        assert_eq!(unescape_unicode("\\u3042\\u3044\\u3046"), "あいう");
        assert_eq!(unescape_unicode("Test\\u0041Test"), "TestATest");
        assert_eq!(unescape_unicode(""), "");
        assert_eq!(unescape_unicode("No escapes here"), "No escapes here");
    }

    #[test]
    fn test_unescape_unicode_mixed() {
        // Test mixed content
        let input = "\\u7b2c\\u4e00\\u7ae0 - Chapter 1";
        let expected = "第一章 - Chapter 1";
        assert_eq!(unescape_unicode(input), expected);
    }

    #[test]
    fn test_unescape_unicode_invalid() {
        // Invalid sequences should be preserved
        assert_eq!(unescape_unicode("\\uZZZZ"), "\\uZZZZ");
    }
}
