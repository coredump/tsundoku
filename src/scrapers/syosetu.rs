//! Syosetu (ncode.syosetu.com / novel18.syosetu.com) scraper implementation.
//!
//! Supports both general audience and 18+ content from the Syosetu platform.

use super::{ChapterInfo, ChapterList, NovelInfo, Scraper, create_http_client, rate_limit};
use crate::config::ScrapingConfig;
use crate::error::ScraperError;
use async_trait::async_trait;
use regex::Regex;
use scraper::{Html, Selector};
use std::sync::LazyLock;

/// Compiled regex patterns for Syosetu URLs.
static URL_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        // ncode.syosetu.com URLs (general audience)
        Regex::new(r"https?://ncode\.syosetu\.com/n\w+/?(\d+/?)?").unwrap(),
        // novel18.syosetu.com URLs (18+ content)
        Regex::new(r"https?://novel18\.syosetu\.com/n\w+/?(\d+/?)?").unwrap(),
    ]
});

/// Regex to extract novel ID from URL (n followed by alphanumerics in the path).
/// Matches patterns like /n1234ab/ but not the domain ncode.syosetu.com
static NOVEL_ID_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\.com/(n[a-z0-9]+)").unwrap());

/// Regex to extract base URL from full URL.
static BASE_URL_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(https://[\w.]+/n\w+)/?").unwrap());

/// CSS selectors used for parsing.
struct Selectors {
    /// Primary title selector (new layout).
    title_primary: Selector,
    /// Fallback title selector (old layout).
    title_fallback: Selector,
    /// Primary chapter link selector (new layout).
    chapter_primary: Selector,
    /// Fallback chapter link selector (old layout).
    chapter_fallback: Selector,
    /// Primary next page selector.
    next_page_primary: Selector,
    /// Primary content selector (new layout).
    content_primary: Selector,
    /// Fallback content selector (old layout).
    content_fallback: Selector,
    /// Paragraph selector.
    paragraph: Selector,
}

impl Selectors {
    fn new() -> Self {
        Self {
            title_primary: Selector::parse(".p-novel__title").unwrap(),
            title_fallback: Selector::parse("p.novel_title").unwrap(),
            chapter_primary: Selector::parse(".p-eplist__sublist > a").unwrap(),
            chapter_fallback: Selector::parse(".novel_sublist2 > dd > a").unwrap(),
            next_page_primary: Selector::parse(".c-pager__item--next").unwrap(),
            content_primary: Selector::parse(
                ".p-novel__text.js-novel-text:not(.p-novel__text--preface):not(.p-novel__text--afterword)",
            )
            .unwrap(),
            content_fallback: Selector::parse("#novel_honbun").unwrap(),
            paragraph: Selector::parse("p").unwrap(),
        }
    }
}

/// Syosetu scraper for ncode.syosetu.com and novel18.syosetu.com.
pub struct SyosetuScraper {
    client: reqwest::Client,
    config: ScrapingConfig,
    selectors: Selectors,
}

impl SyosetuScraper {
    /// Creates a new Syosetu scraper with the given configuration.
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

        // Build request with over18 cookie for adult content
        let response = self
            .client
            .get(url)
            .header("Cookie", "over18=yes")
            .send()
            .await?;

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
        // Try primary selector first
        if let Some(elem) = doc.select(&self.selectors.title_primary).next() {
            let title = elem.text().collect::<String>().trim().to_string();
            if !title.is_empty() {
                return Ok(title);
            }
        }

        // Try fallback selector
        if let Some(elem) = doc.select(&self.selectors.title_fallback).next() {
            let title = elem.text().collect::<String>().trim().to_string();
            if !title.is_empty() {
                return Ok(title);
            }
        }

        Err(ScraperError::ElementNotFound("novel title".to_string()))
    }

    /// Extracts the novel ID from a URL.
    fn extract_novel_id(url: &str) -> Result<String, ScraperError> {
        NOVEL_ID_REGEX
            .captures(url)
            .and_then(|caps| caps.get(1))
            .map(|m| m.as_str().to_string())
            .ok_or_else(|| ScraperError::InvalidUrl("Could not extract novel ID".to_string()))
    }

    /// Extracts the base URL from a full URL.
    fn extract_base_url(url: &str) -> Result<String, ScraperError> {
        BASE_URL_REGEX
            .captures(url)
            .and_then(|caps| caps.get(1))
            .map(|m| {
                let base = m.as_str();
                if base.ends_with('/') {
                    base.to_string()
                } else {
                    format!("{}/", base)
                }
            })
            .ok_or_else(|| ScraperError::InvalidUrl("Could not extract base URL".to_string()))
    }

    /// Checks if the page contains one-shot content (story on main page).
    fn is_oneshot(&self, doc: &Html) -> bool {
        doc.select(&self.selectors.content_primary).next().is_some()
            || doc
                .select(&self.selectors.content_fallback)
                .next()
                .is_some()
    }

    /// Extracts chapter links from a page.
    fn extract_chapter_links(&self, doc: &Html, base_url: &str) -> Vec<(String, String)> {
        // Try primary selector first
        let mut chapters: Vec<(String, String)> = doc
            .select(&self.selectors.chapter_primary)
            .filter_map(|elem| {
                let href = elem.value().attr("href")?;
                let title = elem.text().collect::<String>().trim().to_string();
                let full_url = resolve_url(base_url, href);
                Some((title, full_url))
            })
            .collect();

        // If no chapters found, try fallback
        if chapters.is_empty() {
            chapters = doc
                .select(&self.selectors.chapter_fallback)
                .filter_map(|elem| {
                    let href = elem.value().attr("href")?;
                    let title = elem.text().collect::<String>().trim().to_string();
                    let full_url = resolve_url(base_url, href);
                    Some((title, full_url))
                })
                .collect();
        }

        chapters
    }

    /// Finds the next page URL if pagination exists.
    fn find_next_page(&self, doc: &Html) -> Option<String> {
        // Try primary selector
        if let Some(elem) = doc.select(&self.selectors.next_page_primary).next()
            && let Some(href) = elem.value().attr("href")
        {
            return Some(href.to_string());
        }

        // Fallback: look for link with text containing "次へ" (next)
        let link_selector = Selector::parse("a").unwrap();
        for elem in doc.select(&link_selector) {
            let text = elem.text().collect::<String>();
            if (text.contains("次へ") || text.contains("次ページ"))
                && let Some(href) = elem.value().attr("href")
            {
                return Some(href.to_string());
            }
        }

        None
    }

    /// Extracts and cleans content from the page.
    fn extract_content(&self, doc: &Html) -> Result<String, ScraperError> {
        // Find content div
        let content_elem = doc
            .select(&self.selectors.content_primary)
            .next()
            .or_else(|| doc.select(&self.selectors.content_fallback).next())
            .ok_or_else(|| ScraperError::ElementNotFound("chapter content".to_string()))?;

        // Get the inner HTML so we can manipulate it
        let inner_html = content_elem.inner_html();

        // Parse the content to remove ruby annotations
        let content_doc = Html::parse_fragment(&inner_html);

        // Extract text from paragraphs, or all text if no paragraphs
        let paragraphs: Vec<String> = content_doc
            .select(&self.selectors.paragraph)
            .map(|p| {
                // Get text, excluding ruby readings and their fallback brackets
                extract_text_without_ruby(p)
            })
            .collect();

        let text = if paragraphs.is_empty() {
            // No paragraphs, get all text
            extract_text_without_ruby(content_elem)
        } else {
            paragraphs.join("\n")
        };

        Ok(text.trim().to_string())
    }
}

/// Extracts text from an element, excluding ruby annotations.
///
/// Both the reading (`<rt>`) and the fallback brackets around it (`<rp>`, `《》` on
/// Syosetu) are dropped, so `宮城《きゅうじょう》` becomes `宮城`.
fn extract_text_without_ruby(elem: scraper::ElementRef) -> String {
    let mut text = String::new();

    for node in elem.descendants() {
        if let scraper::node::Node::Text(t) = node.value() {
            let in_annotation = node.ancestors().any(|ancestor| {
                ancestor
                    .value()
                    .as_element()
                    .is_some_and(|elem| matches!(elem.name(), "rt" | "rp"))
            });
            if !in_annotation {
                text.push_str(t);
            }
        }
    }

    text
}

/// Resolves a relative URL against a base URL.
fn resolve_url(base: &str, relative: &str) -> String {
    if relative.starts_with("http://") || relative.starts_with("https://") {
        return relative.to_string();
    }

    if relative.starts_with('/')
        && let Ok(base_url) = url::Url::parse(base)
        && let Ok(resolved) = base_url.join(relative)
    {
        return resolved.to_string();
    }

    // Simple join for relative paths
    let base = base.trim_end_matches('/');
    format!("{}/{}", base, relative.trim_start_matches('/'))
}

#[async_trait]
impl Scraper for SyosetuScraper {
    fn name(&self) -> &'static str {
        "Syosetu"
    }

    fn id(&self) -> &'static str {
        "syosetu"
    }

    fn can_handle(&self, url: &str) -> bool {
        URL_PATTERNS.iter().any(|pattern| pattern.is_match(url))
    }

    async fn get_novel_info(&self, url: &str) -> Result<NovelInfo, ScraperError> {
        if !self.can_handle(url) {
            return Err(ScraperError::UnsupportedUrl(url.to_string()));
        }

        let doc = self.fetch_page(url).await?;
        let title = self.extract_title(&doc)?;
        let novel_id = Self::extract_novel_id(url)?;
        let base_url = Self::extract_base_url(url)?;

        Ok(NovelInfo {
            title,
            base_url,
            novel_id,
        })
    }

    async fn get_chapter_list(&self, base_url: &str) -> Result<ChapterList, ScraperError> {
        let mut all_chapters = Vec::new();
        let mut current_url = base_url.to_string();
        let mut page_count = 0;
        const MAX_PAGES: u32 = 100; // Safety limit

        loop {
            page_count += 1;
            if page_count > MAX_PAGES {
                break;
            }

            let doc = self.fetch_page(&current_url).await?;

            // Extract chapters from this page
            let chapters = self.extract_chapter_links(&doc, base_url);

            // If no chapters found on first page, check for one-shot
            if chapters.is_empty() && page_count == 1 {
                if self.is_oneshot(&doc) {
                    return Ok(ChapterList::OneShot);
                }
                // No chapters and not a one-shot
                return Ok(ChapterList::Chapters(Vec::new()));
            }

            all_chapters.extend(chapters);

            // Check for next page
            if let Some(next_url) = self.find_next_page(&doc) {
                current_url = resolve_url(base_url, &next_url);
            } else {
                break;
            }
        }

        // Convert to ChapterInfo with numbers
        let chapter_infos: Vec<ChapterInfo> = all_chapters
            .into_iter()
            .enumerate()
            .map(|(idx, (title, url))| ChapterInfo {
                title,
                url,
                number: (idx + 1) as u32,
            })
            .collect();

        Ok(ChapterList::Chapters(chapter_infos))
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
    fn test_ruby_readings_and_brackets_are_dropped() {
        let html = Html::parse_fragment(
            "<p>先に<ruby>宮城<rp>《</rp><rt>きゅうじょう</rt><rp>》</rp></ruby>へ行った。</p>",
        );
        let paragraph = html.select(&Selector::parse("p").unwrap()).next().unwrap();

        assert_eq!(extract_text_without_ruby(paragraph), "先に宮城へ行った。");
    }

    #[test]
    fn test_url_patterns() {
        let scraper = SyosetuScraper::new(ScrapingConfig::default());

        // Valid URLs
        assert!(scraper.can_handle("https://ncode.syosetu.com/n1234ab/"));
        assert!(scraper.can_handle("https://ncode.syosetu.com/n1234ab/1/"));
        assert!(scraper.can_handle("https://novel18.syosetu.com/n5678cd/"));
        assert!(scraper.can_handle("http://ncode.syosetu.com/n1234ab"));

        // Invalid URLs
        assert!(!scraper.can_handle("https://example.com/"));
        assert!(!scraper.can_handle("https://kakuyomu.jp/works/123"));
    }

    #[test]
    fn test_extract_novel_id() {
        assert_eq!(
            SyosetuScraper::extract_novel_id("https://ncode.syosetu.com/n1234ab/").unwrap(),
            "n1234ab"
        );
        assert_eq!(
            SyosetuScraper::extract_novel_id("https://novel18.syosetu.com/n5678cd/1/").unwrap(),
            "n5678cd"
        );
    }

    #[test]
    fn test_extract_base_url() {
        assert_eq!(
            SyosetuScraper::extract_base_url("https://ncode.syosetu.com/n1234ab/1/").unwrap(),
            "https://ncode.syosetu.com/n1234ab/"
        );
        assert_eq!(
            SyosetuScraper::extract_base_url("https://ncode.syosetu.com/n1234ab").unwrap(),
            "https://ncode.syosetu.com/n1234ab/"
        );
    }

    #[test]
    fn test_resolve_url() {
        assert_eq!(
            resolve_url("https://ncode.syosetu.com/n1234ab/", "/n1234ab/2/"),
            "https://ncode.syosetu.com/n1234ab/2/"
        );
        assert_eq!(
            resolve_url("https://ncode.syosetu.com/n1234ab/", "2/"),
            "https://ncode.syosetu.com/n1234ab/2/"
        );
        assert_eq!(
            resolve_url(
                "https://ncode.syosetu.com/n1234ab/",
                "https://other.com/page"
            ),
            "https://other.com/page"
        );
    }
}
