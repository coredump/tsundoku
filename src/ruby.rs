//! Ruby (furigana) and site markup normalisation.
//!
//! Scrapers store every ruby annotation in the authors' own Syosetu/Kakuyomu
//! notation, `｜base《reading》`, so `Original/*.txt` keeps the readings. Pixiv
//! tags are converted to the same notation by [`canonicalize_markup`]. Callers
//! then pick a rendering: [`for_translation`] keeps only what the translator
//! needs, [`for_scout`] keeps every reading next to its base.

use regex::{Captures, Regex};
use scraper::ElementRef;
use scraper::node::Node;
use std::borrow::Cow;
use std::sync::LazyLock;

/// Pixiv markup tags handled by [`canonicalize_markup`].
static PIXIV_TAG_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\[\[rb:(?P<rb_base>[^\]\n>]*)>(?P<rb_reading>[^\]\n]*)\]\]",
        r"|\[\[jumpuri:(?P<uri_text>[^\]\n>]*)>[^\]\n]*\]\]",
        r"|\[chapter:(?P<chapter>[^\]\n]*)\]",
        r"|(?P<newpage>\[newpage\])",
        r"|\[(?:jump:\d+|pixivimage:[^\]\n]*|uploadedimage:[^\]\n]*)\]",
    ))
    .expect("Invalid PIXIV_TAG_REGEX")
});

/// Canonical ruby: `｜base《reading》`. The base may be empty (blank-base ruby).
static RUBY_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"｜(?P<base>[^｜《》\n]*)《(?P<reading>[^｜《》\n]+)》")
        .expect("Invalid RUBY_REGEX")
});

/// Converts site-specific markup to plain text with canonical ruby.
///
/// - Pixiv `[[rb:B > R]]` → `｜B《R》`
/// - `[[jumpuri:T > U]]` → `T`
/// - `[chapter:T]` → `T` on its own line
/// - `[newpage]` → an empty line
/// - `[jump:N]`, `[pixivimage:…]`, `[uploadedimage:…]` → removed
/// - `\r\n` → `\n`
///
/// Only these exact tag shapes are matched, so this is safe on text from any
/// source, and it is idempotent.
pub fn canonicalize_markup(text: &str) -> Cow<'_, str> {
    let text: Cow<'_, str> = if text.contains("\r\n") {
        Cow::Owned(text.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(text)
    };
    if !PIXIV_TAG_REGEX.is_match(&text) {
        return text;
    }

    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for caps in PIXIV_TAG_REGEX.captures_iter(&text) {
        let Some(whole) = caps.get(0) else {
            continue;
        };
        out.push_str(&text[last..whole.start()]);
        last = whole.end();

        if let (Some(base), Some(reading)) = (caps.name("rb_base"), caps.name("rb_reading")) {
            let (base, reading) = (base.as_str().trim(), reading.as_str().trim());
            if reading.is_empty() {
                out.push_str(base);
            } else {
                out.push('｜');
                out.push_str(base);
                out.push('《');
                out.push_str(reading);
                out.push('》');
            }
        } else if let Some(uri_text) = caps.name("uri_text") {
            out.push_str(uri_text.as_str().trim());
        } else if let Some(chapter) = caps.name("chapter") {
            push_own_line(&mut out, chapter.as_str().trim(), &text[last..]);
        } else if caps.name("newpage").is_some() {
            push_own_line(&mut out, "", &text[last..]);
        }
        // Jump and image tags are dropped.
    }
    out.push_str(&text[last..]);
    Cow::Owned(out)
}

/// Appends `line` so that it stands on its own line, given the text that follows.
fn push_own_line(out: &mut String, line: &str, rest: &str) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(line);
    if !rest.is_empty() && !rest.starts_with('\n') {
        out.push('\n');
    }
}

/// Renders text for the translator.
///
/// Runs [`canonicalize_markup`], then renders each `｜B《R》`:
///
/// | Ruby kind | Rule | Rendered as |
/// |---|---|---|
/// | Blank base | B is empty or whitespace | `R` |
/// | Phonetic | R == B, or R is all hiragana and B contains kanji | `B` |
/// | Emphasis | R made only of `・﹅﹆ヽ` | `B` |
/// | Gloss | anything else | `B《R》` |
///
/// Bare `《…》` without `｜` is left alone, because authors also use it as brackets.
pub fn for_translation(text: &str) -> String {
    let text = canonicalize_markup(text);
    RUBY_REGEX
        .replace_all(&text, |caps: &Captures| {
            render_ruby(&caps["base"], &caps["reading"])
        })
        .into_owned()
}

/// Renders a single ruby annotation for translation.
fn render_ruby(base: &str, reading: &str) -> String {
    if base.trim().is_empty() {
        return reading.to_string();
    }
    let phonetic =
        reading == base || (reading.chars().all(is_hiragana) && base.chars().any(is_kanji));
    let emphasis = reading
        .chars()
        .all(|c| matches!(c, '・' | '﹅' | '﹆' | 'ヽ'));
    if phonetic || emphasis {
        base.to_string()
    } else {
        format!("{base}《{reading}》")
    }
}

/// Renders text for the name scout: [`canonicalize_markup`] with every
/// `｜B《R》` kept, so the scout sees each reading next to its name.
pub fn for_scout(text: &str) -> String {
    canonicalize_markup(text).into_owned()
}

/// Extracts the text of an HTML element with ruby in canonical form.
///
/// Each `<ruby>` becomes `｜{rb or bare base text}《{rt}》`; `<rp>` fallback
/// brackets are skipped and all other text passes through unchanged. A ruby
/// with several base/reading pairs yields one annotation per pair.
pub fn html_text(elem: ElementRef<'_>) -> String {
    let mut out = String::new();
    push_html_text(elem, &mut out);
    out
}

fn push_html_text(elem: ElementRef<'_>, out: &mut String) {
    for child in elem.children() {
        match child.value() {
            Node::Text(text) => out.push_str(text),
            Node::Element(_) => {
                let Some(child) = ElementRef::wrap(child) else {
                    continue;
                };
                match child.value().name() {
                    "ruby" => push_ruby(child, out),
                    "rp" | "rt" => {}
                    _ => push_html_text(child, out),
                }
            }
            _ => {}
        }
    }
}

fn push_ruby(ruby: ElementRef<'_>, out: &mut String) {
    let mut base = String::new();
    for child in ruby.children() {
        match child.value() {
            Node::Text(text) => base.push_str(text),
            Node::Element(_) => {
                let Some(child) = ElementRef::wrap(child) else {
                    continue;
                };
                match child.value().name() {
                    "rp" => {}
                    "rt" => {
                        let reading: String = child.text().collect();
                        let reading = reading.trim();
                        if reading.is_empty() {
                            out.push_str(&base);
                        } else {
                            out.push('｜');
                            out.push_str(&base);
                            out.push('《');
                            out.push_str(reading);
                            out.push('》');
                        }
                        base.clear();
                    }
                    _ => push_html_text(child, &mut base),
                }
            }
            _ => {}
        }
    }
    out.push_str(&base);
}

fn is_hiragana(c: char) -> bool {
    matches!(c, '\u{3041}'..='\u{309F}' | 'ー')
}

fn is_kanji(c: char) -> bool {
    matches!(
        c,
        '\u{4E00}'..='\u{9FFF}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{20000}'..='\u{2FFFF}'
            | '々'
            | '〆'
            | 'ヶ'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use scraper::{Html, Selector};

    fn assert_renders(cases: &[(&str, &str)]) {
        for (input, expected) in cases {
            assert_eq!(for_translation(input), *expected, "input: {input}");
        }
    }

    #[test]
    fn phonetic_ruby_renders_base() {
        assert_renders(&[
            ("先に｜宮城《きゅうじょう》へ", "先に宮城へ"),
            ("｜山田《やまだ》さん", "山田さん"),
            ("｜一ヶ月《いっかげつ》", "一ヶ月"),
        ]);
    }

    #[test]
    fn same_reading_renders_base() {
        assert_renders(&[
            ("｜ルビ《ルビ》だ", "ルビだ"),
            ("｜Alice《Alice》", "Alice"),
        ]);
    }

    #[test]
    fn gloss_ruby_keeps_reading() {
        assert_renders(&[
            (
                "｜聖剣《エクスカリバー》を抜く",
                "聖剣《エクスカリバー》を抜く",
            ),
            ("｜こんな奴《ルビのサンプル》", "こんな奴《ルビのサンプル》"),
            ("｜あいつ《ゆうしゃ》", "あいつ《ゆうしゃ》"),
            ("｜魔法《magic》", "魔法《magic》"),
            ("｜Ｓランク《すごいやつ》", "Ｓランク《すごいやつ》"),
        ]);
    }

    #[test]
    fn emphasis_dots_render_base() {
        assert_renders(&[("｜本当《・・》に", "本当に"), ("｜嘘《﹅﹅》", "嘘")]);
    }

    #[test]
    fn blank_base_renders_reading() {
        assert_renders(&[
            ("｜《こんりゅう》した", "こんりゅうした"),
            ("｜　　《こんりゅう》", "こんりゅう"),
        ]);
    }

    #[test]
    fn bare_double_angle_brackets_are_untouched() {
        assert_renders(&[
            ("《王都》へ向かった", "《王都》へ向かった"),
            ("宮城《きゅうじょう》", "宮城《きゅうじょう》"),
        ]);
    }

    #[test]
    fn canonicalize_converts_pixiv_tags() {
        let cases = [
            ("[[rb:建立 > こんりゅう]]した", "｜建立《こんりゅう》した"),
            ("[[rb:　　 > こんりゅう]]", "｜《こんりゅう》"),
            ("前\n[newpage]\n後", "前\n\n後"),
            ("前[newpage]後", "前\n\n後"),
            ("[chapter:序章]\n本文", "序章\n本文"),
            ("前[chapter:序章]本文", "前\n序章\n本文"),
            ("[[jumpuri:公式 > https://x]]を見て", "公式を見て"),
            ("前[jump:2]後[pixivimage:123-1][uploadedimage:456]", "前後"),
            ("一行目\r\n二行目", "一行目\n二行目"),
            (
                "[abc] と [[rb:x]] はそのまま",
                "[abc] と [[rb:x]] はそのまま",
            ),
        ];
        for (input, expected) in cases {
            let once = canonicalize_markup(input);
            assert_eq!(once, expected, "input: {input}");
            assert_eq!(canonicalize_markup(&once), once, "not idempotent: {input}");
        }
    }

    #[test]
    fn canonicalize_borrows_plain_text() {
        assert!(matches!(canonicalize_markup("普通の文"), Cow::Borrowed(_)));
    }

    #[test]
    fn pixiv_ruby_renders_for_translation_and_scout() {
        let text = "[[rb:建立 > こんりゅう]]と[[rb:聖剣 > エクスカリバー]]";
        assert_eq!(for_translation(text), "建立と聖剣《エクスカリバー》");
        assert_eq!(
            for_scout(text),
            "｜建立《こんりゅう》と｜聖剣《エクスカリバー》"
        );
    }

    #[test]
    fn html_text_emits_canonical_ruby() {
        let html = Html::parse_fragment(concat!(
            "<p>先に<ruby>宮城<rp>《</rp><rt>きゅうじょう</rt><rp>》</rp></ruby>へ",
            "<ruby><rb>こんな奴</rb><rp>（</rp><rt>ルビのサンプル</rt><rp>）</rp></ruby>と",
            "<ruby><rb>東</rb><rt>ひがし</rt><rb>京</rb><rt>きょう</rt></ruby>",
            "<span>だ</span>。</p>"
        ));
        let p = html.select(&Selector::parse("p").unwrap()).next().unwrap();

        assert_eq!(
            html_text(p),
            "先に｜宮城《きゅうじょう》へ｜こんな奴《ルビのサンプル》と｜東《ひがし》｜京《きょう》だ。"
        );
    }
}
