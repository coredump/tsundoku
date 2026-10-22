//! Kakuyomu (kakuyomu.jp) scraper implementation.
//!
//! Supports downloading novels from Kadokawa's Kakuyomu platform.

use super::{ChapterInfo, ChapterList, NovelInfo, Scraper, create_http_client, rate_limit};
use crate::config::ScrapingConfig;
use crate::error::ScraperError;
use crate::ruby;
use async_trait::async_trait;
use regex::Regex;
use scraper::{Html, Selector};
use std::sync::LazyLock;

/// Compiled regex patterns for Kakuyomu URLs.
static URL_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        // Works URL (main novel page)
        Regex::new(r"https?://kakuyomu\.jp/works/(\d+)(?:/episodes/\d+)?/?").unwrap(),
    ]
});

/// Regex to extract work ID from URL.
static WORK_ID_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/works/(\d+)").unwrap());

/// Regex to strip episode suffix from URLs.
static EPISODE_SUFFIX_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/episodes/\d+/?$").unwrap());

/// CSS selectors used for parsing.
struct Selectors {
    /// Title selector (heading with link).
    title: Selector,
    /// Chapter link selector.
    chapter: Selector,
    /// Content selector.
    content: Selector,
    /// Paragraph selector.
    paragraph: Selector,
}

impl Selectors {
    fn new() -> Self {
        Self {
            // Kakuyomu uses dynamic class names, so we use attribute prefix selectors
            title: Selector::parse(r#"h1[class^="Heading_heading"] a"#).unwrap(),
            chapter: Selector::parse(r#"a[class^="WorkTocSection_link"]"#).unwrap(),
            content: Selector::parse("div.widget-episodeBody").unwrap(),
            paragraph: Selector::parse("p").unwrap(),
        }
    }
}

/// Kakuyomu scraper for kakuyomu.jp.
pub struct KakuyomuScraper {
    client: reqwest::Client,
    config: ScrapingConfig,
    selectors: Selectors,
}

impl KakuyomuScraper {
    /// Creates a new Kakuyomu scraper with the given configuration.
    pub fn new(config: ScrapingConfig) -> Self {
        let client = create_http_client().expect("Failed to create HTTP client");

        Self {
            client,
            config,
            selectors: Selectors::new(),
        }
    }

    /// Fetches a page and returns the HTML document.
    async fn fetch_page(&self, url: &str) -> Result<Html, ScraperError> {
        rate_limit(self.config.delay_between_requests_sec).await;

        let response = self.client.get(url).send().await?;

        if !response.status().is_success() {
            return Err(ScraperError::HttpError(
                response.error_for_status().unwrap_err(),
            ));
        }

        let text = response.text().await?;
        Ok(Html::parse_document(&text))
    }

    /// Extracts the novel title from the page.
    fn extract_title(&self, doc: &Html) -> Result<String, ScraperError> {
        if let Some(elem) = doc.select(&self.selectors.title).next() {
            // Try the title attribute first
            if let Some(title_attr) = elem.value().attr("title") {
                let title = title_attr.trim().to_string();
                if !title.is_empty() {
                    return Ok(title);
                }
            }

            // Fall back to text content
            let title = elem.text().collect::<String>().trim().to_string();
            if !title.is_empty() {
                return Ok(title);
            }
        }

        Err(ScraperError::ElementNotFound("novel title".to_string()))
    }

    /// Extracts the work ID from a URL.
    fn extract_work_id(url: &str) -> Result<String, ScraperError> {
        WORK_ID_REGEX
            .captures(url)
            .and_then(|caps| caps.get(1))
            .map(|m| m.as_str().to_string())
            .ok_or_else(|| ScraperError::InvalidUrl("Could not extract work ID".to_string()))
    }

    /// Gets the base URL (strips episode suffix if present).
    fn get_base_url(url: &str) -> String {
        let without_episode = EPISODE_SUFFIX_REGEX.replace(url, "");
        let base = without_episode.trim_end_matches('/');
        base.to_string()
    }

    /// Resolves a relative URL against Kakuyomu's base.
    fn resolve_url(relative: &str) -> String {
        if relative.starts_with("http://") || relative.starts_with("https://") {
            return relative.to_string();
        }

        format!("https://kakuyomu.jp{}", relative)
    }

    /// Extracts chapter text from an episode page.
    ///
    /// Ruby is kept as `｜base《reading》`, and blank paragraphs
    /// (`<p class="blank">`) are kept as empty lines. Authors use a single one as
    /// paragraph spacing; a run of three or more becomes a scene break through
    /// `mark_scene_breaks`.
    fn extract_content(&self, doc: &Html) -> Result<String, ScraperError> {
        let content_elem = doc
            .select(&self.selectors.content)
            .next()
            .ok_or_else(|| ScraperError::ElementNotFound("chapter content".to_string()))?;

        let paragraphs: Vec<String> = content_elem
            .select(&self.selectors.paragraph)
            .map(|p| ruby::html_text(p).trim().to_string())
            .collect();

        let text = if paragraphs.is_empty() {
            // Fall back to all text
            ruby::html_text(content_elem)
        } else {
            paragraphs.join("\n")
        };

        Ok(text.trim().to_string())
    }
}

#[async_trait]
impl Scraper for KakuyomuScraper {
    fn name(&self) -> &'static str {
        "Kakuyomu"
    }

    fn id(&self) -> &'static str {
        "kakuyomu"
    }

    fn can_handle(&self, url: &str) -> bool {
        URL_PATTERNS.iter().any(|pattern| pattern.is_match(url))
    }

    async fn get_novel_info(&self, url: &str) -> Result<NovelInfo, ScraperError> {
        if !self.can_handle(url) {
            return Err(ScraperError::UnsupportedUrl(url.to_string()));
        }

        let base_url = Self::get_base_url(url);
        let doc = self.fetch_page(&base_url).await?;
        let title = self.extract_title(&doc)?;
        let novel_id = Self::extract_work_id(url)?;

        Ok(NovelInfo {
            title,
            base_url,
            novel_id,
        })
    }

    async fn get_chapter_list(&self, base_url: &str) -> Result<ChapterList, ScraperError> {
        let doc = self.fetch_page(base_url).await?;

        let chapters: Vec<ChapterInfo> = doc
            .select(&self.selectors.chapter)
            .enumerate()
            .filter_map(|(idx, elem)| {
                let href = elem.value().attr("href")?;
                let title = elem.text().collect::<String>().trim().to_string();
                let full_url = Self::resolve_url(href).trim_end_matches('/').to_string();

                Some(ChapterInfo {
                    title,
                    url: full_url,
                    number: (idx + 1) as u32,
                })
            })
            .collect();

        if chapters.is_empty() {
            // Kakuyomu doesn't really have one-shots in the same way
            // If no chapters found, return empty list
            return Ok(ChapterList::Chapters(Vec::new()));
        }

        Ok(ChapterList::Chapters(chapters))
    }

    async fn download_chapter(&self, chapter_url: &str) -> Result<String, ScraperError> {
        let doc = self.fetch_page(chapter_url).await?;
        self.extract_content(&doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_content_keeps_ruby_and_blank_lines() {
        let scraper = KakuyomuScraper::new(ScrapingConfig::default());
        let doc = Html::parse_document(concat!(
            r#"<html><body><div class="widget-episodeBody js-episode-body">"#,
            r#"<p id="p1">　<ruby><rb>こんな奴</rb><rp>（</rp><rt>ルビのサンプル</rt><rp>）</rp></ruby>がいた。</p>"#,
            r#"<p id="p2" class="blank"><br /></p>"#,
            r#"<p id="p3">　次の場面。</p>"#,
            r#"</div></body></html>"#,
        ));

        assert_eq!(
            scraper.extract_content(&doc).unwrap(),
            "｜こんな奴《ルビのサンプル》がいた。\n\n次の場面。"
        );
    }

    #[test]
    fn test_url_patterns() {
        let scraper = KakuyomuScraper::new(ScrapingConfig::default());

        // Valid URLs
        assert!(scraper.can_handle("https://kakuyomu.jp/works/1234567890"));
        assert!(scraper.can_handle("https://kakuyomu.jp/works/1234567890/"));
        assert!(scraper.can_handle("https://kakuyomu.jp/works/1234567890/episodes/9876543210"));

        // Invalid URLs
        assert!(!scraper.can_handle("https://example.com/"));
        assert!(!scraper.can_handle("https://ncode.syosetu.com/n1234ab/"));
        assert!(!scraper.can_handle("https://kakuyomu.jp/users/123"));
    }

    #[test]
    fn test_extract_work_id() {
        assert_eq!(
            KakuyomuScraper::extract_work_id("https://kakuyomu.jp/works/1234567890").unwrap(),
            "1234567890"
        );
        assert_eq!(
            KakuyomuScraper::extract_work_id("https://kakuyomu.jp/works/9876543210/episodes/111")
                .unwrap(),
            "9876543210"
        );
    }

    #[test]
    fn test_get_base_url() {
        assert_eq!(
            KakuyomuScraper::get_base_url("https://kakuyomu.jp/works/1234567890/episodes/111/"),
            "https://kakuyomu.jp/works/1234567890"
        );
        assert_eq!(
            KakuyomuScraper::get_base_url("https://kakuyomu.jp/works/1234567890"),
            "https://kakuyomu.jp/works/1234567890"
        );
    }

    #[test]
    fn test_resolve_url() {
        assert_eq!(
            KakuyomuScraper::resolve_url("/works/123/episodes/456"),
            "https://kakuyomu.jp/works/123/episodes/456"
        );
        assert_eq!(
            KakuyomuScraper::resolve_url("https://kakuyomu.jp/works/123"),
            "https://kakuyomu.jp/works/123"
        );
    }
}
