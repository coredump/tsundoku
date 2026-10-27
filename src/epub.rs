//! EPUB volume builder for translated chapters.
//!
//! Tsundoku saves each translated chapter as plain text named `NNNN - Title.txt`
//! in the novel folder. This module groups those files into fixed-size volumes and
//! writes each volume as an EPUB 3 book, with an NCX table of contents for older
//! readers.

use crate::book::BookMetadata;
use crate::error::EpubError;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use zip::CompressionMethod;
use zip::write::{SimpleFileOptions, ZipWriter};

/// Characters that, repeated on a line of their own, mark a scene break
/// (`◆　◆　◆`, `* * *`, `---`). Ellipses and quote brackets are excluded so
/// dialogue such as `「……」` stays text.
const SCENE_BREAK_GLYPHS: &[char] = &[
    '◆', '◇', '○', '●', '□', '■', '☆', '★', '＊', '*', '※', '♦', '♢', '◎', '†', '—', '―', '-',
    '－', '=', '＝', '~', '～',
];

const CONTAINER_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
<rootfiles>
<rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
</rootfiles>
</container>
"#;

const STYLE_CSS: &str = "body { margin: 0 4%; line-height: 1.5; }
h1, h2 { text-align: center; }
h2 { margin: 1.5em 0 1em; }
p { margin: 0 0 0.8em; text-indent: 0; }
ul { margin: 0 0 0.8em 1.2em; padding: 0; }
.scene-break { text-align: center; margin: 1.5em 0; }
.title-page { text-align: center; margin-top: 30%; }
";

/// A translated chapter file found in a novel folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChapterFile {
    /// Chapter number from the file name prefix.
    pub number: u32,
    /// Chapter title from the file name.
    pub title: String,
    /// Path to the plain-text chapter.
    pub path: PathBuf,
}

/// Collect translated chapters (`NNNN - Title.txt`) directly inside `novel_dir`.
///
/// Subfolders such as `Original/` and `.tsundoku/` (chapter records) are ignored.
/// Chapters are sorted by number; if two files share a number, the first by file
/// name is kept.
pub fn collect_chapters(novel_dir: &Path) -> Result<Vec<ChapterFile>, EpubError> {
    let mut chapters = Vec::new();
    for entry in fs::read_dir(novel_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let file_name = entry.file_name();
        let Some(stem) = file_name
            .to_str()
            .and_then(|name| name.strip_suffix(".txt"))
        else {
            continue;
        };
        let Some((number, title)) = stem.split_once(" - ") else {
            continue;
        };
        let Ok(number) = number.parse() else {
            continue;
        };
        chapters.push(ChapterFile {
            number,
            title: title.to_string(),
            path,
        });
    }
    chapters.sort_by(|a, b| a.number.cmp(&b.number).then_with(|| a.path.cmp(&b.path)));
    chapters.dedup_by_key(|chapter| chapter.number);
    Ok(chapters)
}

/// Chapters grouped into one EPUB volume.
#[derive(Debug, Clone)]
pub struct Volume {
    /// 1-based volume number.
    pub number: u32,
    /// First chapter number of the volume's range.
    pub first: u32,
    /// Last chapter number of the volume's range.
    pub last: u32,
    /// Chapters present on disk, in order.
    pub chapters: Vec<ChapterFile>,
}

impl Volume {
    /// Chapter numbers absent from the range, up to the highest chapter present.
    ///
    /// Chapters after the last present one are not reported, so a volume whose
    /// translation is still in progress only reports real gaps.
    pub fn missing(&self) -> Vec<u32> {
        let Some(highest) = self.chapters.last().map(|chapter| chapter.number) else {
            return Vec::new();
        };
        (self.first..=highest)
            .filter(|number| {
                self.chapters
                    .binary_search_by_key(number, |chapter| chapter.number)
                    .is_err()
            })
            .collect()
    }
}

/// Replace file-name titles with the exact titles recorded in `book.json`.
///
/// File names lose characters such as `?` and are length-limited; chapters without
/// a recorded title keep their file-name title.
pub fn apply_chapter_titles(chapters: &mut [ChapterFile], metadata: &BookMetadata) {
    for chapter in chapters {
        if let Some(recorded) = metadata.chapters.get(&chapter.number)
            && !recorded.title.is_empty()
        {
            chapter.title.clone_from(&recorded.title);
        }
    }
}

/// Group sorted chapters into volumes of `chapters_per_volume` chapter numbers.
///
/// Volume ranges depend on chapter numbers, not on how many files exist, so volume 2
/// is always chapters 101–200 (for 100 per volume) even while some are untranslated.
pub fn plan_volumes(chapters: Vec<ChapterFile>, chapters_per_volume: u32) -> Vec<Volume> {
    let chapters_per_volume = chapters_per_volume.max(1);
    let mut volumes: Vec<Volume> = Vec::new();
    for chapter in chapters {
        let number = chapter.number.saturating_sub(1) / chapters_per_volume + 1;
        if volumes.last().is_none_or(|volume| volume.number != number) {
            let first = (number - 1) * chapters_per_volume + 1;
            volumes.push(Volume {
                number,
                first,
                last: first.saturating_add(chapters_per_volume - 1),
                chapters: Vec::new(),
            });
        }
        if let Some(volume) = volumes.last_mut() {
            volume.chapters.push(chapter);
        }
    }
    volumes
}

/// Book-level metadata shared by every volume of a novel.
#[derive(Debug, Clone)]
pub struct BookInfo {
    /// Series title.
    pub title: String,
    /// Optional author for the metadata.
    pub author: Option<String>,
    /// Optional URL of the original work, written as `dc:source`.
    pub source: Option<String>,
    /// Stable identifier prefix; the volume number is appended per book.
    pub identifier: String,
}

impl BookInfo {
    /// Derive title and identifier from a Tsundoku folder name such as
    /// `[syosetu: n3631do] Reincarnated as a Barrier Master`.
    pub fn from_folder_name(folder_name: &str) -> Self {
        let (source, title) = match folder_name
            .strip_prefix('[')
            .and_then(|rest| rest.split_once(']'))
        {
            Some((source, title)) => (Some(source), title.trim()),
            None => (None, folder_name.trim()),
        };
        let key: String = source
            .unwrap_or(title)
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        Self {
            title: title.to_string(),
            author: None,
            source: None,
            identifier: format!("urn:tsundoku:{key}"),
        }
    }

    /// Take title, author, and source from `book.json` where they are set.
    pub fn with_metadata(mut self, metadata: &BookMetadata) -> Self {
        if !metadata.title.is_empty() {
            self.title.clone_from(&metadata.title);
        }
        if metadata.author.is_some() {
            self.author.clone_from(&metadata.author);
        }
        if !metadata.source_url.is_empty() {
            self.source = Some(metadata.source_url.clone());
        }
        self
    }

    /// File name for a volume, e.g. `Title - Vol. 01 (0001-0100).epub`.
    pub fn volume_file_name(&self, volume: &Volume) -> String {
        let title: String = self
            .title
            .chars()
            .map(|c| match c {
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
                other => other,
            })
            .collect();
        format!(
            "{title} - Vol. {:02} ({:04}-{:04}).epub",
            volume.number, volume.first, volume.last
        )
    }
}

/// Write one volume to `path`.
///
/// The archive is written to a temporary file next to `path` and renamed into place,
/// so an interrupted run never leaves a truncated EPUB behind.
pub fn write_volume(
    path: &Path,
    book: &BookInfo,
    volume: &Volume,
    modified: SystemTime,
) -> Result<(), EpubError> {
    let partial = path.with_extension("epub.part");
    if let Err(error) = write_archive(&partial, book, volume, modified) {
        let _ = fs::remove_file(&partial);
        return Err(error);
    }
    fs::rename(&partial, path)?;
    Ok(())
}

fn write_archive(
    path: &Path,
    book: &BookInfo,
    volume: &Volume,
    modified: SystemTime,
) -> Result<(), EpubError> {
    let mut zip = ZipWriter::new(fs::File::create(path)?);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    // The mimetype entry must come first and be stored uncompressed.
    zip.start_file("mimetype", stored)?;
    zip.write_all(b"application/epub+zip")?;
    zip.start_file("META-INF/container.xml", deflated)?;
    zip.write_all(CONTAINER_XML.as_bytes())?;
    zip.start_file("OEBPS/style.css", deflated)?;
    zip.write_all(STYLE_CSS.as_bytes())?;
    zip.start_file("OEBPS/title.xhtml", deflated)?;
    zip.write_all(title_page_xhtml(book, volume).as_bytes())?;

    for chapter in &volume.chapters {
        let text = fs::read_to_string(&chapter.path).map_err(|source| EpubError::ReadChapter {
            path: chapter.path.clone(),
            source,
        })?;
        zip.start_file(format!("OEBPS/{}", chapter_href(chapter.number)), deflated)?;
        zip.write_all(chapter_xhtml(&chapter.title, &text).as_bytes())?;
    }

    zip.start_file("OEBPS/nav.xhtml", deflated)?;
    zip.write_all(nav_xhtml(volume).as_bytes())?;
    zip.start_file("OEBPS/toc.ncx", deflated)?;
    zip.write_all(toc_ncx(book, volume).as_bytes())?;
    zip.start_file("OEBPS/content.opf", deflated)?;
    zip.write_all(content_opf(book, volume, modified).as_bytes())?;
    zip.finish()?;
    Ok(())
}

fn chapter_href(number: u32) -> String {
    format!("c{number:05}.xhtml")
}

fn volume_title(book: &BookInfo, volume: &Volume) -> String {
    format!("{} Vol. {}", book.title, volume.number)
}

fn volume_identifier(book: &BookInfo, volume: &Volume) -> String {
    format!("{}:vol{:02}", book.identifier, volume.number)
}

/// A block of chapter content.
#[derive(Debug, PartialEq, Eq)]
enum Block<'a> {
    Paragraph(&'a str),
    List(Vec<&'a str>),
    SceneBreak,
}

/// Split plain chapter text into blocks.
///
/// Every non-empty line is a paragraph; web novels put one paragraph per line, and
/// blank lines only add spacing. Consecutive lines starting with `- ` or a single
/// `・` (the Japanese bullet) form a list, and lines made only of scene-break glyphs
/// become a single scene break.
fn parse_blocks(text: &str) -> Vec<Block<'_>> {
    let mut blocks = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if is_scene_break(line) {
            if !matches!(blocks.last(), None | Some(Block::SceneBreak)) {
                blocks.push(Block::SceneBreak);
            }
            continue;
        }
        let bullet_item = line.strip_prefix("- ").or_else(|| {
            line.strip_prefix('・')
                .filter(|rest| !rest.starts_with('・'))
        });
        if let Some(item) = bullet_item {
            let item = item.trim_start();
            if let Some(Block::List(items)) = blocks.last_mut() {
                items.push(item);
            } else {
                blocks.push(Block::List(vec![item]));
            }
            continue;
        }
        blocks.push(Block::Paragraph(line));
    }
    if matches!(blocks.last(), Some(Block::SceneBreak)) {
        blocks.pop();
    }
    blocks
}

fn is_scene_break(line: &str) -> bool {
    let mut glyphs = 0;
    for c in line.chars() {
        if c.is_whitespace() {
            continue;
        }
        if !SCENE_BREAK_GLYPHS.contains(&c) {
            return false;
        }
        glyphs += 1;
    }
    glyphs >= 3
}

/// Words that start with an apostrophe marking dropped letters, e.g. `'em`, `'til`.
const LEADING_ELISIONS: &[&str] = &["em", "cause", "til", "tis", "twas", "bout", "round", "n"];

/// Map Japanese punctuation left in a translation to its English equivalent.
///
/// `「」` and `『』` become curly quotes, a run of `・` becomes `…` (a single `・`
/// between Latin letters, as in `Juka・York`, becomes a space, and any other single `・`
/// becomes the Latin middle dot `·`), every three periods
/// become `…` (`......` → `……`, `....` → `….`), runs of `―`, `—`, or `--` become one
/// em dash, and full-width ASCII forms and the ideographic space become their ASCII
/// equivalents.
fn normalize_punctuation(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let run = chars[i..].iter().take_while(|&&next| next == c).count();
        match c {
            '・' => {
                let between_letters = i > 0
                    && chars[i - 1].is_ascii_alphabetic()
                    && chars.get(i + 1).is_some_and(char::is_ascii_alphabetic);
                out.push(match (run, between_letters) {
                    (1, true) => ' ',
                    (1, false) => '·',
                    _ => '…',
                });
                i += run;
                continue;
            }
            '―' | '—' | '-' if c != '-' || run >= 2 => {
                let dash_run = chars[i..]
                    .iter()
                    .take_while(|&&next| matches!(next, '―' | '—') || (c == '-' && next == '-'))
                    .count();
                out.push('—');
                i += dash_run;
                continue;
            }
            '.' if run >= 3 => {
                for _ in 0..run / 3 {
                    out.push('…');
                }
                for _ in 0..run % 3 {
                    out.push('.');
                }
                i += run;
                continue;
            }
            '「' => out.push('“'),
            '」' => out.push('”'),
            '『' => out.push('‘'),
            '』' => out.push('’'),
            '\u{3000}' => out.push(' '),
            '\u{301C}' => out.push('~'),
            '\u{FF01}'..='\u{FF5E}' => {
                out.push(char::from_u32(c as u32 - 0xFEE0).unwrap_or(c));
            }
            _ => out.push(c),
        }
        i += 1;
    }
    out
}

/// Append `text` to `out` escaped for XML, with English punctuation.
///
/// Japanese punctuation is first normalized (see [`normalize_punctuation`]), then
/// straight quotes are made curly and characters XML 1.0 forbids are dropped. A
/// straight quote opens after the start of the text, whitespace, or an opening
/// bracket, dash, or quote, and closes otherwise. An apostrophe before digits
/// (`'90s`) or a known elision (`'em`) is always `’`.
fn push_escaped(out: &mut String, text: &str) {
    let text = normalize_punctuation(text);
    let text = text.as_str();
    let mut previous = None;
    for (index, c) in text.char_indices() {
        let emitted = match c {
            '"' => {
                if opens_quote(previous) {
                    '“'
                } else {
                    '”'
                }
            }
            '\'' => {
                if opens_quote(previous) && !is_leading_elision(&text[index + 1..]) {
                    '‘'
                } else {
                    '’'
                }
            }
            other => other,
        };
        match emitted {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\t' | '\n' | '\r' => out.push(emitted),
            c if c.is_control() || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
        previous = Some(emitted);
    }
}

fn opens_quote(previous: Option<char>) -> bool {
    previous.is_none_or(|c| {
        c.is_whitespace() || matches!(c, '(' | '[' | '{' | '—' | '–' | '“' | '‘' | '/')
    })
}

fn is_leading_elision(rest: &str) -> bool {
    if rest.starts_with(|c: char| c.is_ascii_digit()) {
        return true;
    }
    LEADING_ELISIONS.iter().any(|word| {
        rest.get(..word.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(word))
            && !rest[word.len()..].starts_with(char::is_alphabetic)
    })
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    push_escaped(&mut out, text);
    out
}

fn xhtml_document(title: &str, body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops" lang="en" xml:lang="en">
<head>
<meta charset="utf-8"/>
<title>{}</title>
<link rel="stylesheet" type="text/css" href="style.css"/>
</head>
<body>
{body}</body>
</html>
"#,
        escape(title)
    )
}

fn chapter_xhtml(title: &str, text: &str) -> String {
    let mut body = String::with_capacity(text.len() + text.len() / 4);
    body.push_str("<section epub:type=\"chapter\">\n<h2>");
    push_escaped(&mut body, title);
    body.push_str("</h2>\n");
    for block in parse_blocks(text) {
        match block {
            Block::Paragraph(paragraph) => {
                body.push_str("<p>");
                push_escaped(&mut body, paragraph);
                body.push_str("</p>\n");
            }
            Block::List(items) => {
                body.push_str("<ul>\n");
                for item in items {
                    body.push_str("<li>");
                    push_escaped(&mut body, item);
                    body.push_str("</li>\n");
                }
                body.push_str("</ul>\n");
            }
            Block::SceneBreak => {
                body.push_str("<p class=\"scene-break\">*&#160;&#160;*&#160;&#160;*</p>\n");
            }
        }
    }
    body.push_str("</section>\n");
    xhtml_document(title, &body)
}

fn title_page_xhtml(book: &BookInfo, volume: &Volume) -> String {
    let mut body = String::from("<section epub:type=\"titlepage\" class=\"title-page\">\n<h1>");
    push_escaped(&mut body, &book.title);
    body.push_str("</h1>\n");
    if let Some(author) = &book.author {
        body.push_str("<p>");
        push_escaped(&mut body, author);
        body.push_str("</p>\n");
    }
    let first = volume.chapters.first().map_or(volume.first, |c| c.number);
    let last = volume.chapters.last().map_or(volume.last, |c| c.number);
    write!(
        body,
        "<p>Volume {}</p>\n<p>Chapters {first}–{last}</p>\n</section>\n",
        volume.number
    )
    .expect("writing to a String cannot fail");
    xhtml_document(&volume_title(book, volume), &body)
}

fn nav_xhtml(volume: &Volume) -> String {
    let mut body = String::from("<nav epub:type=\"toc\" id=\"toc\">\n<h1>Contents</h1>\n<ol>\n");
    for chapter in &volume.chapters {
        write!(body, "<li><a href=\"{}\">", chapter_href(chapter.number))
            .expect("writing to a String cannot fail");
        push_escaped(&mut body, &chapter.title);
        body.push_str("</a></li>\n");
    }
    body.push_str("</ol>\n</nav>\n");
    xhtml_document("Contents", &body)
}

fn toc_ncx(book: &BookInfo, volume: &Volume) -> String {
    let mut ncx = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
<head>
<meta name="dtb:uid" content="{}"/>
<meta name="dtb:depth" content="1"/>
<meta name="dtb:totalPageCount" content="0"/>
<meta name="dtb:maxPageNumber" content="0"/>
</head>
<docTitle><text>{}</text></docTitle>
<navMap>
"#,
        escape(&volume_identifier(book, volume)),
        escape(&volume_title(book, volume))
    );
    for (index, chapter) in volume.chapters.iter().enumerate() {
        writeln!(
            ncx,
            "<navPoint id=\"np{0}\" playOrder=\"{0}\"><navLabel><text>{1}</text></navLabel><content src=\"{2}\"/></navPoint>",
            index + 1,
            escape(&chapter.title),
            chapter_href(chapter.number)
        )
        .expect("writing to a String cannot fail");
    }
    ncx.push_str("</navMap>\n</ncx>\n");
    ncx
}

fn content_opf(book: &BookInfo, volume: &Volume, modified: SystemTime) -> String {
    let series = escape(&book.title);
    let mut opf = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="book-id" xml:lang="en">
<metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
<dc:identifier id="book-id">{}</dc:identifier>
<dc:title>{}</dc:title>
<dc:language>en</dc:language>
"#,
        escape(&volume_identifier(book, volume)),
        escape(&volume_title(book, volume))
    );
    if let Some(author) = &book.author {
        writeln!(opf, "<dc:creator>{}</dc:creator>", escape(author))
            .expect("writing to a String cannot fail");
    }
    if let Some(source) = &book.source {
        writeln!(opf, "<dc:source>{}</dc:source>", escape(source))
            .expect("writing to a String cannot fail");
    }
    write!(
        opf,
        r##"<meta property="dcterms:modified">{}</meta>
<meta property="belongs-to-collection" id="series">{series}</meta>
<meta refines="#series" property="collection-type">series</meta>
<meta refines="#series" property="group-position">{1}</meta>
<meta name="calibre:series" content="{series}"/>
<meta name="calibre:series_index" content="{1}"/>
</metadata>
<manifest>
<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
<item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
<item id="css" href="style.css" media-type="text/css"/>
<item id="title" href="title.xhtml" media-type="application/xhtml+xml"/>
"##,
        utc_timestamp(modified),
        volume.number
    )
    .expect("writing to a String cannot fail");
    for chapter in &volume.chapters {
        writeln!(
            opf,
            r#"<item id="c{0:05}" href="{1}" media-type="application/xhtml+xml"/>"#,
            chapter.number,
            chapter_href(chapter.number)
        )
        .expect("writing to a String cannot fail");
    }
    opf.push_str("</manifest>\n<spine toc=\"ncx\">\n<itemref idref=\"title\"/>\n");
    for chapter in &volume.chapters {
        writeln!(opf, r#"<itemref idref="c{:05}"/>"#, chapter.number)
            .expect("writing to a String cannot fail");
    }
    opf.push_str("</spine>\n</package>\n");
    opf
}

/// Format `time` as an EPUB `dcterms:modified` value (`YYYY-MM-DDThh:mm:ssZ`).
fn utc_timestamp(time: SystemTime) -> String {
    let secs = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Convert days since 1970-01-01 to a proleptic Gregorian date (Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::time::Duration;
    use tempfile::TempDir;

    fn chapter(number: u32) -> ChapterFile {
        ChapterFile {
            number,
            title: format!("Chapter {number}"),
            path: PathBuf::from(format!("{number:04} - Chapter {number}.txt")),
        }
    }

    #[test]
    fn test_japanese_punctuation_becomes_english() {
        assert_eq!(
            escape(
                "「Hold on・・・」 『Not again』――Ri--Rico？！　Juka・York ～ well-known ・ list Fox・12"
            ),
            "“Hold on…” ‘Not again’—Ri—Rico?! Juka York ~ well-known · list Fox·12"
        );
        assert_eq!(
            escape("Well... maybe. Wait.... Hmm...... v1.2 e.g."),
            "Well… maybe. Wait…. Hmm…… v1.2 e.g."
        );
    }

    #[test]
    fn test_straight_quotes_become_curly() {
        assert_eq!(
            escape(r#""Wait," she said. "It's 'fine' since the '90s—'em too." (Rock 'n' roll.)"#),
            "“Wait,” she said. “It’s ‘fine’ since the ’90s—’em too.” (Rock ’n’ roll.)"
        );
        assert_eq!(
            escape("“Already” curly’s & <kept>"),
            "“Already” curly’s &amp; &lt;kept&gt;"
        );
        assert_eq!(escape("\"'Nested,' he said.\""), "“‘Nested,’ he said.”");
    }

    #[test]
    fn test_scene_breaks_lists_and_punctuation_dialogue() {
        let text = "◆　◆　◆\nFirst line.\n\n\n「……」\n◆◆◆◆◆\n* * *\n- one\n・two\n・・・Silence.\nAfter <list> & more\n---\n";

        assert_eq!(
            parse_blocks(text),
            [
                Block::Paragraph("First line."),
                Block::Paragraph("「……」"),
                Block::SceneBreak,
                Block::List(vec!["one", "two"]),
                Block::Paragraph("・・・Silence."),
                Block::Paragraph("After <list> & more"),
            ]
        );
        assert!(chapter_xhtml("A & B", "x < y").contains("<h2>A &amp; B</h2>\n<p>x &lt; y</p>"));
    }

    #[test]
    fn test_volumes_follow_chapter_numbers_and_report_gaps() {
        let chapters = [1, 2, 4, 100, 101, 150, 205].map(chapter).to_vec();

        let volumes = plan_volumes(chapters, 100);
        let summary: Vec<_> = volumes
            .iter()
            .map(|v| (v.number, v.first, v.last, v.chapters.len()))
            .collect();

        assert_eq!(
            summary,
            [(1, 1, 100, 4), (2, 101, 200, 2), (3, 201, 300, 1)]
        );
        assert_eq!(
            volumes[0].missing(),
            [3].into_iter().chain(5..100).collect::<Vec<_>>()
        );
        assert_eq!(volumes[1].missing(), (102..150).collect::<Vec<_>>());
        assert_eq!(volumes[2].missing(), (201..205).collect::<Vec<_>>());
    }

    #[test]
    fn test_collect_chapters_ignores_originals_and_sorts_numerically() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("Original")).unwrap();
        std::fs::create_dir_all(dir.path().join(".tsundoku/chunks")).unwrap();
        for name in [
            "1000 - Late.txt",
            "999 - Legacy padding.txt",
            "0002 - Two.txt",
            "notes.txt",
            "oneshot.txt",
            "Original/0001 - 原文.txt",
            ".tsundoku/chunks/1.json",
            ".tsundoku/chunks/0001 - Record.txt",
        ] {
            std::fs::write(dir.path().join(name), "text").unwrap();
        }

        let chapters = collect_chapters(dir.path()).unwrap();
        let found: Vec<_> = chapters
            .iter()
            .map(|c| (c.number, c.title.as_str()))
            .collect();

        assert_eq!(found, [(2, "Two"), (999, "Legacy padding"), (1000, "Late")]);
    }

    #[test]
    fn test_written_epub_has_stored_mimetype_first_and_ordered_spine() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("0002 - Second.txt"), "Two").unwrap();
        std::fs::write(dir.path().join("0001 - First & Foremost.txt"), "One").unwrap();
        let book = BookInfo::from_folder_name("[syosetu: n1234ab] Some Novel");
        let volume = plan_volumes(collect_chapters(dir.path()).unwrap(), 100).remove(0);
        let path = dir.path().join(book.volume_file_name(&volume));

        write_volume(
            &path,
            &book,
            &volume,
            UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        )
        .unwrap();

        let mut archive = zip::ZipArchive::new(std::fs::File::open(&path).unwrap()).unwrap();
        let mut mimetype = String::new();
        {
            let mut first = archive.by_index(0).unwrap();
            assert_eq!(first.name(), "mimetype");
            assert_eq!(first.compression(), CompressionMethod::Stored);
            first.read_to_string(&mut mimetype).unwrap();
        }
        assert_eq!(mimetype, "application/epub+zip");

        let mut opf = String::new();
        archive
            .by_name("OEBPS/content.opf")
            .unwrap()
            .read_to_string(&mut opf)
            .unwrap();
        assert!(opf.contains(
            "<dc:identifier id=\"book-id\">urn:tsundoku:syosetu:n1234ab:vol01</dc:identifier>"
        ));
        assert!(opf.contains("2023-11-14T22:13:20Z"));
        let spine = &opf[opf.find("<spine").unwrap()..];
        assert!(spine.find("c00001").unwrap() < spine.find("c00002").unwrap());
        assert!(
            !dir.path()
                .join("Some Novel - Vol. 01 (0001-0100).epub.part")
                .exists()
        );
    }
}
