//! Utility functions for common operations.

use crate::error::TranslationError;
use regex::Regex;
use std::sync::LazyLock;

/// Regex for a leading chapter number such as `第三百九十話　　` or `第12章 `.
///
/// Katakana `ニ` is accepted alongside kanji `二` because authors mistype it.
static CHAPTER_NUMBER_PREFIX_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^第[0-9０-９〇零一二ニ三四五六七八九十百千]+[話章][\s:：・]*")
        .expect("Invalid CHAPTER_NUMBER_PREFIX_REGEX")
});

/// Remove a leading `第N話` or `第N章` from a Japanese chapter title.
///
/// The caller writes the chapter number itself; models otherwise render `第二話` as
/// "Chapter Two", "Episode 2", or "Chapter 2" at random, and authors' own numbers
/// sometimes contain typos.
pub fn strip_chapter_number(title: &str) -> &str {
    CHAPTER_NUMBER_PREFIX_REGEX
        .find(title)
        .map_or(title, |prefix| &title[prefix.end()..])
        .trim()
}

/// Count Japanese kana and CJK ideographs in `text`.
///
/// Translated English should contain none; any found indicates untranslated Japanese
/// or a model slipping into Chinese. The middle dot `・` is not counted: it is
/// punctuation (an ellipsis when repeated, a name separator when single), and the
/// EPUB builder converts it.
pub fn count_cjk_chars(text: &str) -> usize {
    text.chars()
        .filter(|&c| {
            c != '・'
                && matches!(
                    c,
                    '\u{3040}'..='\u{30FF}'
                        | '\u{3400}'..='\u{4DBF}'
                        | '\u{4E00}'..='\u{9FFF}'
                        | '\u{F900}'..='\u{FAFF}'
                )
        })
        .count()
}

/// Blank lines in a row that authors use to mark a scene change.
const SCENE_BREAK_MIN_BLANK_LINES: usize = 3;

/// Line that replaces a blank-line scene break before translation.
const SCENE_BREAK_MARKER: &str = "* * *";

/// Replace every run of three or more blank lines between text with a `* * *` line.
///
/// Web-novel authors mark scene changes with a wide gap, but models collapse blank
/// lines, so the break disappears from the translation. An explicit glyph line is
/// copied through, and the EPUB builder renders it as a scene break. Text without
/// such a gap is returned unchanged.
pub fn mark_scene_breaks(text: String) -> String {
    let is_blank = |line: &str| line.trim().is_empty();
    let has_gap = text
        .lines()
        .skip_while(|line| is_blank(line))
        .scan(0, |blanks, line| {
            if is_blank(line) {
                *blanks += 1;
                Some(false)
            } else {
                let gap = *blanks >= SCENE_BREAK_MIN_BLANK_LINES;
                *blanks = 0;
                Some(gap)
            }
        })
        .any(|gap| gap);
    if !has_gap {
        return text;
    }

    let mut out = String::with_capacity(text.len());
    let mut pending_blanks: Vec<&str> = Vec::new();
    let mut seen_text = false;
    for line in text.lines() {
        if is_blank(line) {
            pending_blanks.push(line);
            continue;
        }
        if seen_text && pending_blanks.len() >= SCENE_BREAK_MIN_BLANK_LINES {
            out.push('\n');
            out.push_str(SCENE_BREAK_MARKER);
            out.push_str("\n\n");
        } else {
            for blank in pending_blanks.iter() {
                out.push_str(blank);
                out.push('\n');
            }
        }
        pending_blanks.clear();
        out.push_str(line);
        out.push('\n');
        seen_text = true;
    }
    for blank in pending_blanks {
        out.push_str(blank);
        out.push('\n');
    }
    if !text.ends_with('\n') {
        out.pop();
    }
    out
}

/// Splits text into chunks by lines, respecting a maximum chunk size.
///
/// This function splits text into chunks where each chunk is at most `chunk_size`
/// characters. It prefers to split on line boundaries to maintain context.
///
/// # Arguments
/// * `text` - The text to split
/// * `chunk_size` - Maximum size of each chunk in characters
///
/// # Returns
/// A vector of text chunks, each no larger than `chunk_size` (unless a single line
/// exceeds the limit, in which case that line becomes its own chunk).
pub fn split_text_into_line_chunks(text: &str, chunk_size: usize) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut chunks: Vec<String> = Vec::new();
    let mut current_chunk: Vec<&str> = Vec::new();
    let mut current_size: usize = 0;

    for line in lines {
        let line_size = line.len() + if current_chunk.is_empty() { 0 } else { 1 };

        if current_size + line_size > chunk_size && !current_chunk.is_empty() {
            // Push current chunk and start new one
            chunks.push(current_chunk.join("\n"));
            current_chunk = vec![line];
            current_size = line.len();
        } else {
            current_chunk.push(line);
            current_size += line_size;
        }
    }

    // Remember the last chunk
    if !current_chunk.is_empty() {
        chunks.push(current_chunk.join("\n"));
    }

    chunks
}

/// Checks if an HTTP response is successful, and if not, returns a detailed error.
///
/// This helper extracts both the status code and response body for better error messages.
///
/// # Arguments
/// * `response` - The reqwest Response to check
///
/// # Returns
/// Ok(response) if successful, or Err(TranslationError) with details if not
pub async fn check_response_status(
    response: reqwest::Response,
) -> Result<reqwest::Response, TranslationError> {
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(TranslationError::ApiError(format!(
            "HTTP {}: {}",
            status, text
        )));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_chapter_number() {
        let cases = [
            ("第二話　　奴隷", "奴隷"),
            ("第三百九十話　　お怒り", "お怒り"),
            ("第七百四十ニ話　メイ、閃く", "メイ、閃く"),
            ("第１２話 Title", "Title"),
            ("第3章：始まり", "始まり"),
            ("第二話", ""),
            ("閑話　休日", "閑話　休日"),
            ("第一王子の話", "第一王子の話"),
        ];
        for (title, expected) in cases {
            assert_eq!(strip_chapter_number(title), expected, "{title}");
        }
    }

    #[test]
    fn test_mark_scene_breaks_replaces_wide_gaps_only() {
        let text = "\n\n\nOpening.\nNext.\n\nAfter one.\n\n\nAfter two.\n\n\n\nAfter three.\n　\n \n\n\n\nAfter five.\n\n\n\n";
        assert_eq!(
            mark_scene_breaks(text.to_string()),
            "\n\n\nOpening.\nNext.\n\nAfter one.\n\n\nAfter two.\n\n* * *\n\nAfter three.\n\n* * *\n\nAfter five.\n\n\n\n"
        );

        let unchanged = "First.\n\n\nSecond.";
        assert_eq!(mark_scene_breaks(unchanged.to_string()), unchanged);
        assert_eq!(
            mark_scene_breaks("First.\n\n\n\nSecond.".to_string()),
            "First.\n\n* * *\n\nSecond."
        );
    }

    #[test]
    fn test_split_empty_text() {
        let chunks = split_text_into_line_chunks("", 100);
        assert_eq!(chunks.len(), 0);
    }

    #[test]
    fn test_split_single_line() {
        let text = "Hello world";
        let chunks = split_text_into_line_chunks(text, 100);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], text);
    }

    #[test]
    fn test_split_multiple_lines_fits() {
        let text = "Line 1\nLine 2\nLine 3";
        let chunks = split_text_into_line_chunks(text, 100);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], text);
    }

    #[test]
    fn test_split_multiple_chunks_needed() {
        let text = "Line 1\nLine 2\nLine 3\nLine 4";
        let chunks = split_text_into_line_chunks(text, 15);
        // "Line 1\nLine 2" = 13 chars
        // "Line 3\nLine 4" = 13 chars
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "Line 1\nLine 2");
        assert_eq!(chunks[1], "Line 3\nLine 4");
    }

    #[test]
    fn test_split_single_long_line() {
        let text = "This is a very long line that exceeds the chunk size limit";
        let chunks = split_text_into_line_chunks(text, 20);
        // Should keep the whole line as one chunk even though it exceeds limit
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], text);
    }

    #[test]
    fn test_split_with_empty_lines() {
        let text = "Line 1\n\nLine 3";
        let chunks = split_text_into_line_chunks(text, 100);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], text);
    }
}
