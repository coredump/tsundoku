//! Utility functions for common operations.

use crate::error::TranslationError;
use regex::Regex;
use std::ops::Range;
use std::sync::LazyLock;

/// Regex for a leading chapter number such as `第三百九十話　　`, `第12章 `,
/// `●第1話●` or `12話　`.
///
/// Katakana `ニ` is accepted alongside kanji `二` because authors mistype it.
/// Without `第`, the number must be followed by `話` or `章` and a separator, so
/// `十話目の朝` stays intact.
static CHAPTER_NUMBER_PREFIX_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^[\s●○◆◇■□★☆◎【\[［]*",
        r"(?:第[0-9０-９〇零一二ニ三四五六七八九十百千]+[話章][\s:：・●○◆◇■□★☆◎】\]］]*",
        r"|[0-9０-９〇零一二ニ三四五六七八九十百千]+[話章](?:[\s:：・●○◆◇■□★☆◎】\]］]+|$))",
    ))
    .expect("Invalid CHAPTER_NUMBER_PREFIX_REGEX")
});

/// Remove a leading chapter number such as `第N話`, `第N章` or `●第N話●` from a
/// Japanese chapter title.
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

/// Openers that only a model talking about itself would produce.
const UNAMBIGUOUS_REFUSAL_OPENERS: [&str; 3] = [
    "as an ai",
    "i am not programmed",
    "i do not have the ability",
];

/// Openers that are also ordinary English (a title `ごめんね` → "I'm sorry").
const AMBIGUOUS_REFUSAL_OPENERS: [&str; 5] = [
    "i'm sorry",
    "my apologies",
    "i cannot",
    "i can't",
    "i am unable",
];

/// Phrases that make an ambiguous opener a reply about the request itself. They name
/// the task ("translate", "this request") rather than words a character could say to
/// another ("I cannot help you").
const REFUSAL_REQUEST_PHRASES: [&str; 13] = [
    "translat",
    "this request",
    "your request",
    "assist with",
    "help with that",
    "help with this",
    "fulfill",
    "comply with",
    "this content",
    "that content",
    "this text",
    "provide that",
    "as an ai",
];

/// How far into the reply to look for [`REFUSAL_REQUEST_PHRASES`]; a real refusal
/// says what it declines up front, and story text further in can use them.
const REFUSAL_WINDOW_CHARS: usize = 200;

/// Returns whether a model reply declines the request instead of answering it.
///
/// Apologies and "I cannot" are only refusals when they go on to talk about the
/// request, so a faithful translation of an apology is accepted.
pub fn is_refusal(reply: &str) -> bool {
    let head: String = reply
        .trim_start()
        .chars()
        .take(REFUSAL_WINDOW_CHARS)
        .collect::<String>()
        .to_lowercase()
        .replace('’', "'");
    UNAMBIGUOUS_REFUSAL_OPENERS
        .iter()
        .any(|opener| head.starts_with(opener))
        || (AMBIGUOUS_REFUSAL_OPENERS
            .iter()
            .any(|opener| head.starts_with(opener))
            && REFUSAL_REQUEST_PHRASES
                .iter()
                .any(|phrase| head.contains(phrase)))
}

/// Closing tag a reasoning model emits after its thinking.
const THINK_CLOSE_TAG: &str = "</think>";

/// Returns the reply after the last `</think>` tag, or the whole reply if none.
///
/// With reasoning disabled, Qwen models occasionally still write a draft and a
/// closing `</think>` before the real answer; keeping both doubles the chunk.
pub fn strip_reasoning(reply: &str) -> &str {
    reply
        .rfind(THINK_CLOSE_TAG)
        .map_or(reply, |pos| &reply[pos + THINK_CLOSE_TAG.len()..])
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

/// Glyphs that, alone on a line (with optional spaces), mark a scene change.
const SCENE_BREAK_GLYPHS: [char; 23] = [
    '◇', '◆', '○', '●', '□', '■', '☆', '★', '＊', '*', '※', '・', '―', '—', '－', 'ー', '〜', '~',
    '＝', '=', '#', '♢', '♦',
];

/// Characters that end a sentence; a line is split after a run of them.
const SENTENCE_END_CHARS: [char; 7] = ['。', '！', '？', '!', '?', '」', '』'];

/// Returns whether `line` is a scene-break line such as `◇◇◇`, `＊＊＊` or the
/// `* * *` marker from [`mark_scene_breaks`]: non-empty and made only of break glyphs
/// and spaces.
fn is_scene_break_line(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|c| c.is_whitespace() || SCENE_BREAK_GLYPHS.contains(&c))
}

/// Characters in `lens.len()` lines of the given lengths once joined with `"\n"`.
fn joined_len(lens: &[usize]) -> usize {
    lens.iter().sum::<usize>() + lens.len().saturating_sub(1)
}

/// A piece of text that [`split_into_scene_chunks`] packs into chunks.
enum ChunkUnit {
    /// Whole lines: a scene, or part of a scene too long for one chunk.
    Lines {
        range: Range<usize>,
        starts_scene: bool,
        ends_scene: bool,
    },
    /// Part of a single line too long for one chunk; always a chunk of its own.
    LinePiece(String),
}

/// Splits text into chunks of at most `max_chars` characters, preferring scene breaks.
///
/// The text is cut into scenes at scene-break lines (the break line opens the next
/// scene, so it is translated in context), and whole scenes are packed greedily into
/// chunks. A scene longer than `max_chars` is split into the fewest pieces of roughly
/// equal size at line boundaries; its first and last pieces may share a chunk with
/// the neighbouring scenes. A single line longer than `max_chars` is split the same
/// way at sentence ends, or at character boundaries if it has none.
///
/// Size is counted in characters, not bytes. Joining the chunks with `"\n"` gives back
/// the input lines, except that an over-long line is spread over several chunks.
pub fn split_into_scene_chunks(text: &str, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(1);
    let lines: Vec<&str> = text.lines().collect();
    let lens: Vec<usize> = lines.iter().map(|line| line.chars().count()).collect();

    let mut scenes: Vec<Range<usize>> = Vec::new();
    let mut start = 0;
    for (i, line) in lines.iter().enumerate().skip(1) {
        if is_scene_break_line(line) {
            scenes.push(start..i);
            start = i;
        }
    }
    if !lines.is_empty() {
        scenes.push(start..lines.len());
    }

    let mut units = Vec::new();
    for scene in scenes {
        if joined_len(&lens[scene.clone()]) > max_chars {
            split_oversized_scene(&lines, &lens, scene, max_chars, &mut units);
        } else {
            units.push(ChunkUnit::Lines {
                range: scene,
                starts_scene: true,
                ends_scene: true,
            });
        }
    }

    // Lines packed so far, and whether they end at a scene boundary.
    let mut packed: Option<(Range<usize>, bool)> = None;
    let mut chunks = Vec::new();
    for unit in units {
        match unit {
            ChunkUnit::Lines {
                range,
                starts_scene,
                ends_scene,
            } => match packed.as_mut() {
                Some((open, open_ends_scene))
                    if (starts_scene || *open_ends_scene)
                        && joined_len(&lens[open.start..range.end]) <= max_chars =>
                {
                    open.end = range.end;
                    *open_ends_scene = ends_scene;
                }
                _ => {
                    if let Some((open, _)) = packed.replace((range, ends_scene)) {
                        chunks.push(lines[open].join("\n"));
                    }
                }
            },
            ChunkUnit::LinePiece(piece) => {
                if let Some((open, _)) = packed.take() {
                    chunks.push(lines[open].join("\n"));
                }
                chunks.push(piece);
            }
        }
    }
    if let Some((open, _)) = packed {
        chunks.push(lines[open].join("\n"));
    }
    chunks
}

/// Splits the lines in `scene`, longer than `max_chars` in total, into balanced units
/// at line boundaries; lines that alone exceed `max_chars` are split at sentence ends.
fn split_oversized_scene(
    lines: &[&str],
    lens: &[usize],
    scene: Range<usize>,
    max_chars: usize,
    units: &mut Vec<ChunkUnit>,
) {
    let push_run = |run: Range<usize>, units: &mut Vec<ChunkUnit>| {
        for part in balanced_partition(&lens[run.clone()], 1, max_chars) {
            let range = run.start + part.start..run.start + part.end;
            units.push(ChunkUnit::Lines {
                starts_scene: range.start == scene.start,
                ends_scene: range.end == scene.end,
                range,
            });
        }
    };
    let mut run_start = scene.start;
    for i in scene.clone() {
        if lens[i] > max_chars {
            push_run(run_start..i, units);
            split_long_line(lines[i], max_chars, units);
            run_start = i + 1;
        }
    }
    push_run(run_start..scene.end, units);
}

/// Splits a line longer than `max_chars` into balanced pieces at sentence ends,
/// hard-splitting at character boundaries any sentence that is itself too long.
fn split_long_line(line: &str, max_chars: usize, units: &mut Vec<ChunkUnit>) {
    // Byte ranges of the pieces, each at most `max_chars` characters.
    let mut pieces: Vec<Range<usize>> = Vec::new();
    let mut lens: Vec<usize> = Vec::new();
    let mut push_sentence = |sentence: Range<usize>| {
        let text = &line[sentence.clone()];
        let count = text.chars().count();
        let parts = count.div_ceil(max_chars);
        let mut char_starts: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
        char_starts.push(text.len());
        for k in 0..parts {
            let from = k * count / parts;
            let to = (k + 1) * count / parts;
            pieces.push(sentence.start + char_starts[from]..sentence.start + char_starts[to]);
            lens.push(to - from);
        }
    };

    let mut sentence_start = 0;
    let mut chars = line.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if let Some(&(next_index, next)) = chars.peek()
            && SENTENCE_END_CHARS.contains(&c)
            && !SENTENCE_END_CHARS.contains(&next)
        {
            push_sentence(sentence_start..next_index);
            sentence_start = next_index;
        }
    }
    push_sentence(sentence_start..line.len());

    for part in balanced_partition(&lens, 0, max_chars) {
        let bytes = pieces[part.start].start..pieces[part.end - 1].end;
        units.push(ChunkUnit::LinePiece(line[bytes].to_string()));
    }
}

/// Groups consecutive items into the fewest parts of roughly equal size whose joined
/// length (items plus `sep` characters between neighbours) is at most `max_chars`.
///
/// Every item must be at most `max_chars` long. Each cut goes at the item boundary
/// nearest an equal share of the total, without overfilling the part it closes.
fn balanced_partition(lens: &[usize], sep: usize, max_chars: usize) -> Vec<Range<usize>> {
    let count = lens.len();
    if count == 0 {
        return Vec::new();
    }
    // `offsets[i]` is where item `i` starts if every item is followed by `sep`, so a
    // part `i..j` holds `offsets[j] - offsets[i] - sep` characters.
    let mut offsets = Vec::with_capacity(count + 1);
    offsets.push(0);
    for &len in lens {
        offsets.push(offsets[offsets.len() - 1] + len + sep);
    }
    let span = offsets[count];
    let limit = max_chars + sep;

    for parts in span.div_ceil(limit).max(1)..=count {
        let mut bounds = vec![0];
        for k in 1..parts {
            let prev = bounds[k - 1];
            let lo = prev + 1;
            let fits = offsets.partition_point(|&offset| offset <= offsets[prev] + limit) - 1;
            let hi = (count - (parts - k)).min(fits).max(lo);
            let target = k * span / parts;
            let after = offsets
                .partition_point(|&offset| offset < target)
                .clamp(lo, hi);
            let before = after.saturating_sub(1).clamp(lo, hi);
            let bound = if target.abs_diff(offsets[before]) <= target.abs_diff(offsets[after]) {
                before
            } else {
                after
            };
            bounds.push(bound);
        }
        bounds.push(count);
        if bounds
            .windows(2)
            .all(|pair| offsets[pair[1]] - offsets[pair[0]] <= limit)
        {
            return bounds.windows(2).map(|pair| pair[0]..pair[1]).collect();
        }
    }
    (0..count).map(|i| i..i + 1).collect()
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
    fn test_strip_reasoning_keeps_only_the_answer() {
        assert_eq!(
            strip_reasoning("Draft line.\n</think>\n\nFinal line."),
            "\n\nFinal line."
        );
        assert_eq!(strip_reasoning("<think>a</think>b</think>c"), "c");
        assert_eq!(strip_reasoning("No tags here."), "No tags here.");
    }

    #[test]
    fn test_refusals_are_detected() {
        for reply in [
            "I'm sorry, but I can't help with translating this content.",
            "I’m sorry, I cannot fulfill this request.",
            "I cannot assist with that.",
            "My apologies, but I am unable to provide a translation.",
            "As an AI, I must decline.",
        ] {
            assert!(is_refusal(reply), "should be a refusal: {reply}");
        }
    }

    #[test]
    fn test_translated_apologies_are_not_refusals() {
        let long_chapter = format!(
            "I cannot believe it. The village was quiet.\n{}\nShe asked for my help.",
            "The wind blew over the fields. ".repeat(10)
        );
        for reply in [
            "I'm sorry",
            "I'm Sorry",
            "My Apologies, Lady Monika",
            "I Cannot Go Back",
            "I'm sorry, I cannot help you. Please leave the village.",
            "I can't help it, Kyoka. Apples are my weakness.",
            long_chapter.as_str(),
        ] {
            assert!(!is_refusal(reply), "should not be a refusal: {reply}");
        }
    }

    #[test]
    fn test_strip_chapter_number() {
        let cases = [
            ("第二話　　奴隷", "奴隷"),
            ("第三百九十話　　お怒り", "お怒り"),
            ("第七百四十ニ話　メイ、閃く", "メイ、閃く"),
            ("第１２話 Title", "Title"),
            ("第3章：始まり", "始まり"),
            ("第二話", ""),
            ("●第6話●無属性のクズ魔石", "無属性のクズ魔石"),
            ("【第10話】出発", "出発"),
            ("12話　孤島に追放", "孤島に追放"),
            ("閑話　休日", "閑話　休日"),
            ("十話目の朝", "十話目の朝"),
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

    fn char_len(chunk: &str) -> usize {
        chunk.chars().count()
    }

    #[test]
    fn test_scene_chunks_empty_and_small_text() {
        assert!(split_into_scene_chunks("", 100).is_empty());
        let text = "Line 1\n\nLine 3";
        assert_eq!(split_into_scene_chunks(text, 100), vec![text]);
    }

    #[test]
    fn test_scene_break_lines_are_recognised() {
        for line in ["◇◇◇", "＊＊＊", "* * *", "　◆　◆　◆", "――――", "※"]
        {
            assert!(is_scene_break_line(line), "should be a break: {line}");
        }
        for line in ["「――」", "", "　", "◇彼は言った", "……"] {
            assert!(!is_scene_break_line(line), "should not be a break: {line}");
        }
    }

    #[test]
    fn test_scene_chunks_pack_whole_scenes() {
        let scene_one = "AAAAAAAAAA\nBBBBBBBBBB";
        let scene_two = "◇◇◇\nCCCCCCCCCC";
        let scene_three = "＊＊＊\nDDDDD";
        let text = format!("{scene_one}\n{scene_two}\n{scene_three}");
        assert_eq!(
            split_into_scene_chunks(&text, 30),
            vec![scene_one.to_string(), format!("{scene_two}\n{scene_three}")]
        );
    }

    #[test]
    fn test_oversized_scene_split_balanced_at_paragraphs() {
        let lines: Vec<String> = (0..10).map(|i| format!("line{i:05}")).collect();
        let chunks = split_into_scene_chunks(&lines.join("\n"), 60);
        // Greedy filling would give 6 + 4 lines.
        assert_eq!(chunks, vec![lines[..5].join("\n"), lines[5..].join("\n")]);
    }

    #[test]
    fn test_short_scene_shares_chunk_with_split_neighbour() {
        let lines: Vec<String> = (0..6).map(|i| format!("line{i:05}")).collect();
        let text = format!("short\n◇◇◇\n{}", lines.join("\n"));
        assert_eq!(
            split_into_scene_chunks(&text, 40),
            vec![
                format!("short\n◇◇◇\n{}", lines[..3].join("\n")),
                lines[3..].join("\n"),
            ]
        );
    }

    #[test]
    fn test_oversized_line_split_at_sentence_ends() {
        let line = "「はい。」と言った。".repeat(4);
        let chunks = split_into_scene_chunks(&line, 25);
        assert_eq!(chunks, vec!["「はい。」と言った。".repeat(2); 2]);

        let line = "あいうえお。".repeat(5);
        let chunks = split_into_scene_chunks(&line, 20);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks.concat(), line);
        for chunk in &chunks {
            assert!(char_len(chunk) <= 20 && chunk.ends_with('。'), "{chunk}");
        }
    }

    #[test]
    fn test_line_without_sentence_end_is_hard_split() {
        let line = "あ".repeat(25);
        let chunks = split_into_scene_chunks(&line, 10);
        let sizes: Vec<usize> = chunks.iter().map(|chunk| char_len(chunk)).collect();
        assert_eq!(sizes, vec![8, 8, 9]);
        assert_eq!(chunks.concat(), line);
    }

    #[test]
    fn test_scene_chunks_count_characters_not_bytes() {
        let text = format!("{}\n{}", "あ".repeat(10), "い".repeat(10));
        assert_eq!(split_into_scene_chunks(&text, 21), vec![text.clone()]);
        assert_eq!(split_into_scene_chunks(&text, 20).len(), 2);
    }

    #[test]
    fn test_scene_chunks_round_trip_lines() {
        let mut text = String::from("\n冒頭の一文。\n");
        for scene in 0..6 {
            text.push_str("\n* * *\n\n");
            for paragraph in 0..(scene * 3 + 1) {
                text.push_str(&format!("「場面{scene}の段落{paragraph}です。」\n\n"));
            }
        }
        for max in [15, 40, 100, 1000] {
            let chunks = split_into_scene_chunks(&text, max);
            assert_eq!(
                chunks.join("\n"),
                text.lines().collect::<Vec<_>>().join("\n")
            );
            for chunk in &chunks {
                assert!(char_len(chunk) <= max, "{max}: {chunk}");
            }
        }
    }
}
