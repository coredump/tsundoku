# Tsundoku - Complete Implementation Specification

## Overview

A multi-platform web novel downloader and translator supporting Syosetu (ncode.syosetu.com, novel18.syosetu.com), Kakuyomu (kakuyomu.jp), and Pixiv (pixiv.net/novel) platforms. The system downloads Japanese web novels, maintains a persistent character name mapping database with voting system, and translates content using OpenAI-compatible LLM APIs.

## Project Metadata

- **Name**: Tsundoku
- **Version**: 0.1.0
- **Rust Edition**: 2024
- **Dependencies** (Cargo.toml):
  - `reqwest` (HTTP client with cookie support)
  - `scraper` (HTML parsing with CSS selectors)
  - `async-openai` or `reqwest` for OpenAI API (LLM API client)
  - `tokio` (async runtime)
  - `serde` + `serde_json` (JSON serialization)
  - `toml` or `rust-ini` (INI/TOML config parsing)
  - `regex` (regular expressions)
  - `dirs` (platform-specific directories)
  - `clap` (command-line argument parsing)
  - `crossterm` or `console` (terminal colors and formatting)
  - `thiserror` or `anyhow` (error handling)

## Architecture

### Module Structure

```
tsundoku/
├── src/
│   ├── main.rs                 # Main entry point and CLI
│   ├── lib.rs                  # Library root (optional)
│   ├── config.rs               # Configuration loading and validation
│   ├── console.rs              # Console output formatting
│   ├── translator.rs           # Translation system
│   ├── name_mapping.rs         # Name mapping store and scout
│   └── scrapers/
│       ├── mod.rs              # Scraper trait and registry
│       ├── syosetu.rs          # Syosetu scraper
│       ├── kakuyomu.rs         # Kakuyomu scraper
│       └── pixiv.rs            # Pixiv scraper
├── tests/
│   ├── integration_tests.rs    # Integration tests
│   ├── scraper_tests.rs        # Scraper-specific tests
│   └── common/mod.rs           # Test utilities
├── Cargo.toml                  # Project metadata and dependencies
├── Cargo.lock                  # Dependency lock file
└── .gitignore                  # Git ignore rules
```

## Configuration System

### Configuration File Location

Platform-specific configuration directory structure:

**Windows**:
- Path: `%APPDATA%\Tsundoku\config.ini`
- Example: `C:\Users\Username\AppData\Roaming\Tsundoku\config.ini`

**macOS**:
- Path: `~/Library/Application Support/Tsundoku/config.ini`

**Linux/Unix**:
- Path: `$XDG_CONFIG_HOME/Tsundoku/config.ini` (if XDG_CONFIG_HOME is set)
- Fallback: `~/.config/Tsundoku/config.ini`

### Configuration Schema

INI format with the following sections:

#### [API] Section
Main translation API configuration:
- `key`: OpenAI-compatible API key (required, string)
- `base_url`: API base URL (default: "https://api.openai.com/v1", string)
- `model`: Model identifier (default: "gpt-4o-mini", string)

#### [ScoutAPI] Section
Optional separate API for name scouting (falls back to [API] if not set):
- `key`: API key for name scout (string)
- `base_url`: API base URL (string)
- `model`: Model identifier (string)

#### [Translation] Section
Translation behavior configuration:
- `chunk_size_chars`: Maximum UTF-8 bytes per source chunk (default: "4000", integer)
- `retries`: Number of retry attempts for failed translations (default: "3", integer)
- `delay_between_requests_sec`: Delay between API requests (default: "1", float)
- `history_length`: Number of message pairs to retain in conversation history (default: "5", integer)
- `glossary_max_entries`: Maximum canonical glossary entries injected into a chapter translation prompt; `0` disables injection (default: "100", integer)

#### [NameScout] Section
Name and terminology extraction configuration:
- `chunk_size_chars`: Maximum UTF-8 bytes per mapping scout chunk (default: "2500", integer)
- `retries`: Number of retry attempts (default: "3", integer)
- `delay_between_requests_sec`: Delay between requests (default: "1", float)
- `json_retries`: Number of JSON parsing retry attempts (default: "3", integer)

#### [Scraping] Section
Web scraping behavior:
- `delay_between_requests_sec`: Delay between web requests (default: "1", float)

#### [Prompts] Section
LLM system prompts (strings):
- `title_translation_prompt`: Prompt for title translation
- `content_translation_prompt`: Prompt for content translation
- `name_scout_prompt`: Prompt for name and terminology extraction (JSON format expected)

#### [Paths] Section
File system paths:
- `output_directory`: Directory for translated novels (default: ".", path string)
- `names_directory`: Directory for name mapping cache (default: platform-specific, path string)

### Configuration Initialization

1. On first run, if config file doesn't exist:
   - Create default config.ini at platform-specific location
   - Set API key placeholder: "YOUR_OPENAI_COMPATIBLE_KEY_HERE"
   - Exit with message directing user to edit config

2. On subsequent runs:
   - Load existing config
   - Auto-populate any missing sections/keys with defaults
   - Save updated config if changes were made
   - Validate API key is not the placeholder value

## Scraper System

### Scraper Interface

All scrapers must implement the `Scraper` trait:

```rust
pub trait Scraper: Send + Sync {
    /// Returns regex patterns for URL matching
    fn url_patterns() -> Vec<Regex>;
    
    /// Check if this scraper can handle the given URL
    fn can_handle(&self, url: &str) -> bool;
    
    /// Get novel metadata
    async fn get_novel_info(&self, novel_url: &str) -> Result<NovelInfo>;
    
    /// Get list of chapters (or ChapterList::OneShot for single-chapter works)
    async fn get_chapter_list(&self, base_url: &str) -> Result<ChapterList>;
    
    /// Download chapter content
    async fn download_chapter_content(&self, chapter_url: &str) -> Result<String>;
}

pub struct NovelInfo {
    pub title: String,
    pub base_url: String,
    pub novel_id: String,
}

pub enum ChapterList {
    Chapters(Vec<ChapterInfo>),
    OneShot,
}

pub struct ChapterInfo {
    pub title: String,
    pub url: String,
}
```

### Syosetu Scraper (modules/syosetu.py)

**Supported URLs**:
- `https://ncode.syosetu.com/n[code]/` (general audience)
- `https://ncode.syosetu.com/n[code]/[chapter_num]/` (direct chapter)
- `https://novel18.syosetu.com/n[code]/` (18+ content)
- `https://novel18.syosetu.com/n[code]/[chapter_num]/` (18+ direct chapter)

**Session Setup**:
```rust
let client = reqwest::Client::builder()
    .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36")
    .cookie_store(true)
    .build()?;

// Set over18 cookie for adult content
let cookie = "over18=yes";
client.get(url)
    .header("Cookie", cookie)
    .send()
    .await?;
```

**Novel Info Extraction**:
1. Fetch URL using reqwest, handle encoding via `encoding_rs` if needed
2. Parse with `scraper::Html::parse_document()`
3. Find title:
   - Primary: `.p-novel__title` (CSS selector)
   - Fallback: `p.novel_title` (class attribute)
4. Extract novel ID: regex `/(n\w+)/?` from URL
5. Extract base URL: regex `(https://[\w.]+/n\w+)/?` + trailing "/"

**Chapter List Extraction**:
1. Start with base_url
2. Loop for pagination:
   - Parse HTML
   - Find chapter links:
     - Primary: `.p-eplist__sublist > a` (CSS selector)
     - Fallback: `.novel_sublist2 > dd > a` (CSS selector)
   - If no chapters found on first page:
     - Check if one-shot story (content exists directly on page)
     - Look for `.p-novel__text.js-novel-text:not(.p-novel__text--preface):not(.p-novel__text--afterword)` or `#novel_honbun`
     - Return "ONESHOT" if found, else return empty list
   - For each link: extract text as title, join href with base URL
   - Find next page:
     - Primary: `.c-pager__item--next` with href attribute
     - Fallback: `<a>` tag with text matching regex `/次へ/`
   - Apply delay, continue to next page
3. Return list of dicts: `[{'title': str, 'url': str}, ...]`

**One-Shot Detection**:
- Check for story content selectors on main page
- Selector: `.p-novel__text.js-novel-text:not(.p-novel__text--preface):not(.p-novel__text--afterword)`
- Fallback: `#novel_honbun`
- If content div exists, it's a one-shot

**Content Download**:
1. Apply rate limiting delay
2. Fetch chapter URL
3. Find content div:
   - Primary: `.p-novel__text.js-novel-text:not(.p-novel__text--preface):not(.p-novel__text--afterword)`
   - Fallback: `#novel_honbun`
4. Extract text with ruby kept (`ruby::html_text`):
   - Each `<ruby>` becomes `｜{<rb> or bare base text}《{<rt>}》`; `<rp>` is skipped
5. Extract text:
   - Find all `<p>` tags in content div
   - If found: join with "\n" (empty paragraphs stay as empty lines)
   - Else: use the text of the entire div
6. Return stripped text

### Kakuyomu Scraper (modules/kakuyomu.py)

**Supported URLs**:
- `https://kakuyomu.jp/works/[work_id]`
- `https://kakuyomu.jp/works/[work_id]/episodes/[episode_id]` (direct chapter)

**Session Setup**:
```rust
let client = reqwest::Client::builder()
    .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36")
    .build()?;
```

**Novel Info Extraction**:
1. Strip `/episodes/\d+/?` from URL if present
2. Strip trailing slash from base_url
3. Fetch and parse HTML
4. Find title: CSS selector `h1[class^="Heading_heading__"] a`
   - Get title from `title` attribute, fallback to text content
5. Extract work_id: regex `/works/(\d+)` from URL
6. Return (title, base_url, work_id)

**Chapter List Extraction**:
1. Fetch base_url
2. Find chapter links: CSS selector `a[class^=WorkTocSection_link__]`
3. For each link:
   - Extract text as title
   - Join href with base_url
   - Strip trailing slash from chapter URL
4. Return list of dicts: `[{'title': str, 'url': str}, ...]`

**Content Download**:
1. Apply rate limiting delay
2. Fetch chapter URL
3. Optional: Extract chapter title from `p.widget-episodeTitle`
4. Find content: `div.widget-episodeBody`
5. Find all `<p>` tags within content div
6. Extract each paragraph with `ruby::html_text` (ruby as `｜base《reading》`) and trim it;
   blank paragraphs (`<p class="blank">`) are kept as empty lines so scene breaks survive
7. Join paragraph texts with "\n" and return stripped text

Kakuyomu originals saved before ruby support contain readings inlined as `base（reading）`;
delete those `Original/*.txt` files to re-download them.

### Pixiv Scraper (modules/pixiv.py)

**Supported URLs**:
- `https://www.pixiv.net/novel/show.php?id=[novel_id]` (individual novel)
- `https://www.pixiv.net/novel/series/[series_id]` (series)

**Session Setup**:
```rust
let client = reqwest::Client::builder()
    .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36")
    .default_headers({
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("Accept", "application/json, text/javascript, */*; q=0.01".parse().unwrap());
        headers.insert("Accept-Language", "en-US,en;q=0.9".parse().unwrap());
        headers.insert("Referer", "https://www.pixiv.net/".parse().unwrap());
        headers.insert("X-Requested-With", "XMLHttpRequest".parse().unwrap());
        headers
    })
    .build()?;
```

**API Endpoints**:
- Individual novel: `https://www.pixiv.net/ajax/novel/{novel_id}`
- Series info: `https://www.pixiv.net/ajax/novel/series/{series_id}`
- Series content: `https://www.pixiv.net/ajax/novel/series_content/{series_id}?limit=30&last_order={last_order}&order_by=asc`

**URL Parsing**:
- Individual regex: `r"https://www\.pixiv\.net/novel/show\.php\?id=(\d+)"`
- Series regex: `r"https://www\.pixiv\.net/novel/series/(\d+)"`
- Returns: `(url_type, novel_id)` where url_type is "individual" or "series"

**Novel Info Extraction**:
1. Parse URL to determine type (individual vs series)
2. For individual novels:
   - Make AJAX request to individual novel endpoint
   - Extract title from response body
   - Apply Unicode unescaping to title
   - Return (title, original_url, novel_id)
3. For series:
   - Make AJAX request to series endpoint
   - Extract title from response body
   - Apply Unicode unescaping to title
   - Return (title, original_url, series_id)

**Chapter List Extraction**:
1. Parse URL to determine type
2. For individual novels:
   - Return "ONESHOT" string
3. For series:
   - Call `_get_all_series_chapters(series_id)`
   - Format chapters as list of dicts: `[{'title': str, 'url': str, 'id': str, 'order': int}, ...]`
   - Note: 'url' field contains chapter ID for compatibility

**Series Pagination** (`_get_all_series_chapters`):
1. Initialize: `all_chapters = []`, `last_order = 0`, `limit = 30`
2. Loop:
   - Construct URL with limit and last_order parameters
   - Make AJAX request
   - Extract `body.page.seriesContents` array
   - If empty array, break loop
   - For each chapter:
     - Extract id, title, order
     - Apply Unicode unescaping to title
     - If title is empty/whitespace:
       - Make additional AJAX request to individual chapter endpoint
       - Extract actual title from response
       - Apply Unicode unescaping
     - Append to all_chapters: `{'id': str, 'title': str, 'order': int}`
   - If got less than limit (30) chapters, break (last page)
   - Update last_order to order of last chapter
   - Continue loop (rate limiting applied in AJAX request)
3. Return all_chapters list

**Content Download**:
1. Extract novel ID from URL or use directly if already an ID
2. Make AJAX request to individual novel endpoint
3. Extract title and content from response body
4. Apply Unicode unescaping to content
5. Convert Pixiv markup with `ruby::canonicalize_markup` and return the result

**AJAX Request Handling** (`_make_ajax_request`):
1. Apply rate limiting delay (time.sleep)
2. Make GET request with 30-second timeout
3. Error handling:
   - Timeout → return None
   - ConnectionError → return None
   - HTTPError → return None
   - RequestException → return None
4. Validate content-type: must contain "application/json"
5. Parse JSON response
6. Validate structure: must be dict
7. Check error field: if `error == True` or missing, return None
8. Check body field: if missing/None, return None
9. Return body content

**Unicode Unescaping** (`unescape_unicode`):
1. Handle edge cases:
   - None/empty input → return empty string
   - No `\u` sequences in text → return unchanged
2. For text with Unicode escapes:
   - Use a regex or manual parsing to find `\uXXXX` sequences
   - Convert each sequence to the corresponding Unicode character
   - Handle errors gracefully → return original on failure
3. Return unescaped text

```rust
fn unescape_unicode(text: &str) -> String {
    // Handle \uXXXX escape sequences
    let re = Regex::new(r"\\u([0-9a-fA-F]{4})").unwrap();
    re.replace_all(text, |caps: &regex::Captures| {
        let hex = &caps[1];
        u32::from_str_radix(hex, 16)
            .ok()
            .and_then(char::from_u32)
            .map(|c| c.to_string())
            .unwrap_or_else(|| caps[0].to_string())
    }).to_string()
}
```

**Key Implementation Details**:
- All API responses have structure: `{"error": bool, "body": {...}, "message": str}`
- Error handling is comprehensive with logging at each failure point
- Rate limiting is applied before every AJAX request
- Unicode escape sequences (e.g., `\u5dfb`) are common in Pixiv API responses

### Ruby and Markup Normalisation (`ruby.rs`)

Every scraper stores ruby in the canonical form `｜base《reading》` in `Original/*.txt`.

**`canonicalize_markup(text)`** (idempotent, matches only these exact tag shapes, also
normalises `\r\n` to `\n`):
- `[[rb:B > R]]` → `｜B《R》` (a blank base becomes `｜《R》`)
- `[[jumpuri:T > U]]` → `T`
- `[chapter:T]` → `T` on its own line
- `[newpage]` → an empty line
- `[jump:N]`, `[pixivimage:…]`, `[uploadedimage:…]` → removed

**Use-time rendering** (it runs on cached originals too, so older Pixiv originals need
no migration):
- Translation input: `ruby::for_translation` → name substitution → `mark_scene_breaks`.
  This applies to chapter content, chapter titles and one-shot content. Each `｜B《R》`
  is rendered as:

  | Ruby kind | Rule | Rendered as |
  |---|---|---|
  | Blank base | B is empty or whitespace | `R` |
  | Phonetic | R == B, or R is all hiragana and B contains kanji | `B` |
  | Emphasis | R made only of `・﹅﹆ヽ` | `B` |
  | Gloss | anything else | `B《R》` |

  Bare `《…》` without `｜` is left alone. The translation prompt adds a rule that
  `X《Y》` means Y written with X and must be translated as Y.
- Name scout payload: `ruby::for_scout` keeps `｜B《R》`. The scout response contract
  says to report `original` as X only and romanise from Y; name originals containing
  `｜`, `《` or `》` are rejected.

## Translation System

### Translator Class

**Initialization**:
- Load API credentials and configuration
- Validate API key is not placeholder
- Initialize OpenAI client (lazy initialization supported for testing)
- Set up refusal phrase detection
- Initialize progress tracking variables

**Refusal Phrases** (case-insensitive prefix matching):
```rust
const REFUSAL_PHRASES: &[&str] = &[
    "i'm sorry",
    "i cannot",
    "i am unable",
    "as an ai",
    "my apologies",
    "i am not programmed",
    "i do not have the ability",
];
```

**Translation Method** (`translate(text: &str, is_title: bool, glossary: &[TermEntry], progress_info: Option<ProgressInfo>) -> Result<String>`):

1. **Empty Text Handling**:
   - If text is empty → return empty string immediately

2. **Title Translation** (is_title=True):
   - Initialize message history with title_translation_prompt as system message
   - Display: "Translating title 「{snippet}...」"
   - Translate once with the shared retry logic below (no chunking); empty replies and refusals are retried
   - Return result, or `Err` once retries are exhausted

3. **Content Translation** (is_title=False):
   - Add canonical glossary entries present in the name-substituted text to content_translation_prompt, overrides first, then by vote count
   - Initialize message history with that system message
   - Split text into chunks using `_split_text_into_chunks`
   - For each chunk, with the shared retry logic:
     - Display preparing status before API call
     - Call `_translate_single_chunk` with progress info
     - On success: append to results
     - On failure: retry up to `retries` times with exponential backoff; a refusal or empty reply clears history first
     - Reply still containing Chinese/Japanese characters on the last attempt: keep it with a warning
     - Refused on every attempt: append "[TRANSLATION FAILED]\n{original_chunk}"; other errors fail the chapter
   - Join translated chunks with "\n\n"
   - Return result

**Text Chunking Algorithm** (`split_text_into_chunks`):

Phase 1 - Line-based chunking:
1. Split text by `"\n"` into lines
2. Initialize: `chunks: Vec<String>`, `current_chunk: Vec<&str>`, `current_size: usize = 0`
3. For each line:
   - Calculate `line_size = line.len() + if current_chunk.is_empty() { 0 } else { 1 }`
   - If `current_size + line_size > chunk_size` AND `!current_chunk.is_empty()`:
     - Push `current_chunk.join("\n")` to chunks
     - Start new chunk with current line
   - Else:
     - Push line to current_chunk
     - Add line_size to current_size
4. Push remaining current_chunk to chunks if not empty

Phase 2 - Word-based splitting for oversized chunks:
1. For each chunk from phase 1:
   - If `chunk.len() <= chunk_size`: push to final_chunks
   - Else: split by whitespace (word boundaries)
     - Initialize: `current_chunk: Vec<&str>`, `current_size: usize = 0`
     - For each word:
       - Calculate `word_size = word.len() + if current_chunk.is_empty() { 0 } else { 1 }`
       - If `current_size + word_size > chunk_size` AND `!current_chunk.is_empty()`:
         - Push `current_chunk.join(" ")` to final_chunks
         - Start new chunk with current word
       - Else:
         - Push word to current_chunk
         - Add word_size to current_size
     - Push remaining current_chunk to final_chunks
2. Return final_chunks

**Single Chunk Translation** (`translate_single_chunk`):

1. **Setup**:
   - Accept message_history (with system prompt), start_time, progress_info
   - Initialize defaults if not provided

2. **Retry Loop** (up to `retries` attempts):
   - Prepare messages: clone history + add user message with chunk text
   - Create streaming completion via OpenAI-compatible API
   - Stream chunks and accumulate response:
     - For each content chunk:
       - Append to full_response (use `String::push_str`)
       - Update chars_since_last_update
       - Every 1 second, display progress:
         - Calculate speed: chars / time_delta
         - Display: "[Chapter X, Chunk Y/Z.] Progress: {count} chars at {speed}/sec. {preview}"
         - Preview: last 50 chars of response, newlines replaced with spaces
         - Use ANSI codes for styling and line clearing
         - Reset progress tracking variables
   - On completion: clear progress line with `"\r\x1b[2K"`
   - Trim and validate response

3. **Validation**:
   - If empty or starts with refusal phrase:
     - If retries remain: sleep with exponential backoff, retry
     - Else: return `Err`
   - If successful:
     - Append user and assistant messages to history
     - Trim history: keep system message + last `history_length` pairs
       - Remove pairs from position 1 (skip system message)
     - Return `Ok(translated_text)`

4. **Error Handling**:
   - On error: clear progress line, log warning, continue to next retry
   - After all retries exhausted: return `Err`

**Progress Display Format**:
- Uses ANSI escape codes: `\r\033[2K` to clear line and return to start
- Color coding:
  - Cyan bold: Chapter/chunk label
  - Green bold: Character count
  - Yellow bold: Speed
  - Dim gray: Preview text
- Updates every 1 second maximum
- Always clears line after chunk completion

## Name Mapping System

### Name Mapping Store

**Purpose**: Persistent storage of character names and story-specific terminology with vote-based consensus, manual overrides, and independent coverage tracking.

**File Location**:
- Platform-specific names directory (see Configuration)
- Filename format: `{module_name}: {novel_id}.json`
- Windows special handling: replace `:` with ` -` in filename

**JSON Structure**:
```json
{
  "names": {
    "{original_japanese}": {
      "part": "family|given|unknown",
      "votes": {"{english_name}": "count_int"},
      "override": "optional_manual_rendering",
      "english": "selected_english_name",
      "count": "selected_vote_count_int"
    }
  },
  "terms": {
    "{original_japanese_term}": {
      "votes": {"{english_term}": "count_int"},
      "override": "optional_manual_rendering",
      "english": "selected_english_term",
      "count": "selected_vote_count_int"
    }
  },
  "coverage": ["name_coverage_chapter_number_int"],
  "term_coverage": ["term_coverage_chapter_number_int"]
}
```

**Initialization** (`__init__`):
1. Build filepath from module name and novel ID
2. Initialize default data structure: `{"names": {}, "coverage": []}`
3. Load from disk if file exists
4. Validate and ensure required keys exist
5. Purge bad votes

**Vote Recording** (`record_votes(entries: &[NameEntry])`):

For each entry struct `NameEntry { original: String, english: String, part: NamePart }`:
1. Validate entry:
   - Skip if original or english is empty
   - Skip if part not in `{Family, Given, Unknown}`, default to `Unknown`
   - Skip if original contains bad characters (see Bad Original Pattern)
   - Skip if english contains whitespace
   - Skip if either contains honorifics (see Honorific Pattern)
2. Get or create entry in names HashMap
3. Update part field (prefer known parts over Unknown)
4. Increment vote count for english translation
5. Recalculate winning translation:
   - Select translation with highest vote count
   - On tie: prefer current best (stability)
6. Update english and count fields

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum NamePart {
    Family,
    Given,
    Unknown,
}

pub struct NameEntry {
    pub original: String,
    pub english: String,
    pub part: NamePart,
}
```

**Bad Original Pattern**:
Reject originals matching: `r"[\s・･｡､,，。／/：:;!！?？\-—–‑·（）()［\]{}＜＞<>『』「」〈〉【】]"`
Rationale: Names shouldn't contain punctuation, whitespace, or full names with separators

```rust
lazy_static! {
    static ref BAD_ORIGINAL_REGEX: Regex = Regex::new(
        r"[\s・･｡､,，。／/：:;!！?？\-—–‑·（）()［\]{}＜＞<>『』「」〈〉【】]"
    ).unwrap();
}
```

**Honorific Pattern**:
Reject if:
- Original ends with: `r"(さん|ちゃん|くん|君|様|さま|殿|氏|先生|先輩|嬢)$"`
- English (lowercase) contains: "-san", "-chan", "-kun", "-sama", " san", " chan", " kun", " sama"

```rust
lazy_static! {
    static ref HONORIFIC_SUFFIX_REGEX: Regex = Regex::new(
        r"(さん|ちゃん|くん|君|様|さま|殿|氏|先生|先輩|嬢)$"
    ).unwrap();
}

const ENGLISH_HONORIFICS: &[&str] = &[
    "-san", "-chan", "-kun", "-sama",
    " san", " chan", " kun", " sama",
];
```

**Vote Purging** (`purge_bad_votes`):
Run on load and explicit calls:
1. Iterate all names (use `retain` for in-place filtering)
2. Delete entire entry if:
   - Original has bad characters
   - Original or english contains honorific
   - No valid votes remain after filtering
3. For remaining entries:
   - Filter out votes with whitespace in english
   - Filter out votes with honorifics
   - Recalculate best english and count
   - Update entry

**Coverage Tracking**:
- `is_chapter_covered(&self, chapter_number: u32) -> bool`: Check if chapter in coverage set
- `add_coverage(&mut self, chapters: &[u32])`: Add chapter numbers to coverage `HashSet` or sorted `Vec`

**Text Replacement** (`apply_to_text(&self, text: &str) -> String`):
1. Build replacement map: `HashMap<&str, &str>` for all names (original -> english)
2. Sort by original length (descending) to replace longest matches first
3. For each (original, english) pair:
   - Use `str::replace()` or regex replacement for the substitution
4. Return modified text

**Validation** (`validate_data(data: &NameMappingData) -> Result<()>`):
Enforce JSON schema:
- Root must be a struct/object
- Must have "names" (`HashMap<String, NameInfo>`) and "coverage" (`Vec<u32>`) fields
- Coverage entries must be integers
- Name keys must be strings
- Name entries must be structs with:
  - part: `NamePart` enum (Family, Given, Unknown)
  - votes: `HashMap<String, u32>` with string keys and int values
  - english: `Option<String>` (optional)
  - count: `Option<u32>` (optional)

```rust
#[derive(Serialize, Deserialize)]
pub struct NameMappingData {
    pub names: HashMap<String, NameInfo>,
    pub coverage: Vec<u32>,
}

#[derive(Serialize, Deserialize)]
pub struct NameInfo {
    pub part: NamePart,
    pub votes: HashMap<String, u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub english: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
}
```

**Reload from Disk** (`reload_from_disk(&mut self) -> Result<()>`):
1. Read file as UTF-8 using `std::fs::read_to_string()`
2. Parse JSON using `serde_json::from_str()`
3. Validate structure
4. If valid:
   - Replace current data
   - Ensure required keys
   - Purge bad votes
   - Save (normalizes file)
   - Return `Ok(())`
5. If error: log error message, return `Err`

### Mapping Scout

**Purpose**: Extract character names and story-specific terminology from Japanese text using a secondary LLM pass.

**Initialization**:
- Load ScoutAPI configuration (falls back to main API config)
- Validate API key
- Set up refusal phrase detection
- Configure chunk size (typically smaller than translation chunks)

**Prompt Format**:
System prompt requests a JSON response:
```
{"names":[{"original":"<exact name characters>","part":"family|given|unknown","english":"<best English rendering>"}],"terms":[{"original":"<exact Japanese term>","english":"<canonical English rendering>"}]}
```
Terms include distinctive items, skills, magic systems, ranks, organizations, places, and fictional concepts, but exclude person names and ordinary vocabulary.

**Name Collection** (`collect_names_stream(text: &str) -> impl Iterator<Item = Vec<NameEntry>>`):

Returns an iterator (or async stream) that yields parsed name lists per chunk:

1. Split text into chunks (same algorithm as translation chunking)
2. For each chunk:
   - Display: "Name scout chunk X/Y ({char_count} chars)"
   - Retry loop (up to json_retries):
     - Prefix the chunk with already-selected name and term renderings whose originals occur in it (names use the katakana word-boundary rule); the snapshot is refreshed after each scouted chapter
     - Call `call_model(message)` to get raw LLM response
     - On network error: log warning, apply backoff, retry
     - On empty/refusal response: log warning, apply backoff, retry
     - On success: parse response with `parse_response(raw)`
       - On JSON parse error: log warning, apply backoff, retry
       - On success: yield parsed name list, break
   - If all retries failed: log error, continue to next chunk

**Model Call** (`call_model(chunk: &str) -> Result<String>`):
1. Prepare messages: system prompt + user content
2. Call OpenAI-compatible API (non-streaming) using `reqwest` or `async-openai`
3. Extract message content from response
4. Return trimmed content string

**Response Parsing** (`parse_response(raw: &str) -> Result<Vec<NameEntry>>`):

1. **JSON Extraction**:
   - Trim whitespace
   - If starts with "```": remove markdown code fence
     - Remove leading `` ^```[a-zA-Z]*\s* ``
     - Remove trailing `` \s*```$ ``
   - Extract JSON between first `{` and last `}`
   - Parse with `serde_json::from_str()`

2. **Structure Validation**:
   - Must have "names" key containing array
   - Each array item must be an object

3. **Entry Normalization**:
   - Extract original, english, part fields (convert to strings)
   - Normalize part: lowercase, default to `Unknown` if invalid
   - Skip entry if original or english is empty
   - Append to normalized Vec: `Vec<NameEntry>`

4. Return normalized list

**Integration with Main Flow**:
Called after chapter download but before translation:
```rust
for ch in missing_chapters {
    let chapter_payload = build_name_scout_payload(&[ch]);
    for parsed in name_scout.collect_names_stream(&chapter_payload) {
        name_mapping.record_votes(&parsed);
        name_mapping.save()?;  // persist after every successful API call
    }
    name_mapping.add_coverage(&[ch.number]);
    name_mapping.save()?;
}
```

**Chapter Payload Format**:
```
### Chapter {number} - {title}
{content}

### Chapter {number} - {title}
{content}
```

## Main Workflow

### Command-Line Interface

**Usage**: `tsundoku [OPTIONS] <novel_url>`

**Arguments**:
- `novel_url`: Required positional argument, URL of novel to download

**Options**:
- `--start N`: Start downloading from chapter N (1-based, integer >= 1)
- `--end N`: Stop downloading at chapter N (1-based, inclusive, integer >= 1)
- `--no-name-pause`: Skip manual name mapping review pause

**Validation**:
- Range values must be positive integers
- Start cannot exceed end
- Start/end cannot exceed total chapter count
- Start/end cannot be used with one-shot stories

**Argument Parsing Implementation**:
Uses `clap` crate with derive macros for argument parsing:

```rust
use clap::Parser;

#[derive(Parser)]
#[command(name = "tsundoku")]
#[command(about = "Download and translate Japanese web novels")]
struct Args {
    /// URL of the novel to download
    novel_url: String,
    
    /// Start downloading from chapter N (1-based)
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    start: Option<u32>,
    
    /// Stop downloading at chapter N (1-based, inclusive)
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    end: Option<u32>,
    
    /// Skip manual name mapping review pause
    #[arg(long)]
    no_name_pause: bool,
}
```

### Scraper Selection

**Module Loading**:
1. Dynamically discover all modules in `modules/` directory
2. For each module:
   - Import module
   - Find classes with `url_regexs` attribute
   - Add to scrapers list
3. On error: log warning and continue

**URL Matching**:
1. Iterate through all scrapers
2. For each scraper, check all url_regexs patterns
3. Return first scraper class where regex matches URL
4. If no match: exit with error

### Processing Flow

#### Stage 1: Initial Setup and Information Gathering

1. **Load Configuration**:
   - Read/create config file
   - Validate API keys
   - Handle config errors

2. **Parse Arguments**:
   - Validate URL and options
   - Exit on parse errors

3. **Initialize Components**:
   - Select and instantiate scraper
   - Create Translator instance
   - Create NameScout instance

4. **Fetch Novel Metadata**:
   - Call `scraper.get_novel_info(novel_url)`
   - Get: novel_title, base_url, novel_id
   - Exit if failed

5. **Initialize Name Mapping**:
   - Determine module_name from scraper class name (lowercase, remove "scraper" suffix)
   - Create/load NameMappingStore(module_name, novel_id, config)

6. **Get Chapter List**:
   - Call `scraper.get_chapter_list(base_url)`
   - Handle "ONESHOT" vs list of chapters
   - Exit if no chapters found

7. **Validate Chapter Range**:
   - Check if one-shot (cannot use --start/--end)
   - Validate range is within bounds
   - Calculate start_chapter, end_chapter

8. **Determine Output Directory**:
   - Get from config Paths.output_directory
   - Expand user home directory (~)

#### Stage 2A: One-Shot Story Processing

For stories where chapter_list == "ONESHOT":

1. **Find or Create Story Folder**:
   - Search for existing folders:
     - New format: `[{module_name}: {novel_id}] {title}`
     - Old format: `[{novel_id}] {title}` (backward compatibility)
   - If found: use existing folder
   - If not found:
     - Translate title if translator available
     - Create folder with new format
     - Sanitize folder name for filesystem

2. **Download Original Content**:
   - Check if `original.txt` exists
   - If not: download from base_url, save as UTF-8
   - Display success/failure message

3. **Run Name Scout**:
   - Build chapter payload with title and content
   - Call `run_name_scout_for_chapters(name_scout, name_mapping, [chapter_data])`
   - Incremental save after each successful API call

4. **Manual Review** (unless --no-name-pause):
   - Display file path
   - Try to open in Kate editor (if available on Linux)
   - Prompt user to press Enter
   - Reload and validate name mapping
   - Retry until valid

5. **Translate Content**:
   - Check if `oneshot.txt` exists
   - If not:
     - Apply name mapping to original content
     - Call `translator.translate()` with progress info
     - Save as UTF-8
   - Display success/failure message

#### Stage 2B: Multi-Chapter Story Processing

For stories with chapter list:

1. **Find or Create Series Folder**:
   - Same logic as one-shot but with title translation
   - Create `Original/` subfolder

2. **Download Phase** (all requested chapters):
   - Calculate padding width from total chapter count
   - For each chapter in range:
     - Format chapter number with padding: `f"{number:0{width}d}"`
     - Build original filename: `{padded_num} - {sanitized_title}.txt`
     - Check if file exists in `Original/` folder
     - If not:
       - Download content from chapter URL
       - Save as UTF-8
       - Display success message
     - Read content into memory
     - Build chapter data dict: `{number, title, content, chapter_info, chapter_num_str}`
     - Append to downloaded_chapters list
   - Exit if no chapters downloaded

3. **Name Scout Phase**:
   - Call `run_name_scout_for_chapters(name_scout, name_mapping, downloaded_chapters)`
   - For each uncovered chapter:
     - Build payload with chapter header and content
     - Stream name collection (generator)
     - Record votes and save after each successful chunk
     - Add chapter to coverage and save

4. **Manual Review** (unless --no-name-pause):
   - Same as one-shot review process

5. **Translation Phase**:
   - For each downloaded chapter:
     - Check if translation already exists (glob `{chapter_num_str} - *.txt`)
     - If exists: skip
     - If not:
       - Translate title with `translate_and_validate_title_filepath()`:
         - Apply name mapping to title source
         - Translate with retries
         - Validate: no newlines, valid filesystem path
         - On repeated failure: use original title + "[TRANSLATION_FAILED]"
       - Apply name mapping to chapter content
       - Translate content with progress info
       - Save as UTF-8 to translated filepath
       - Display success/failure message

### Filename Sanitization

**Function**: `sanitize_filename(name)`

Algorithm:
1. Strip leading/trailing whitespace
2. Replace invalid characters: `r'[\\/*?"<>|]'` with "_"
   - Note: Colon `:` is NOT replaced (Linux compatibility)
3. Remove trailing dots and spaces: `r'[. ]+$'`
4. Return sanitized string

Used for:
- Series/story folder names
- Original chapter filenames
- Translated chapter filenames

### Folder Naming Conventions

**Multi-Chapter Series**:
- Format: `[{module_name}: {novel_id}] {translated_title}`
- Example: `[syosetu: n1234ab] My Novel Title`
- Subdirectory: `Original/` contains original Japanese text

**One-Shot Stories**:
- Format: Same as multi-chapter
- Files directly in folder:
  - `original.txt`: Original Japanese content
  - `oneshot.txt`: Translated content

**Backward Compatibility**:
- Old format: `[{novel_id}] {title}`
- System will find and reuse old format folders
- New downloads create new format folders

### Error Handling

**Configuration Errors**:
- Missing API key → exit with instruction to edit config
- Invalid config values → use defaults, warn user

**Network Errors**:
- Failed to fetch URL → log error, return None
- Timeout → log error, retry if within retry limit

**Translation Errors**:
- Empty response → retry with backoff
- Refusal detected → retry with backoff
- All retries failed → save "[TRANSLATION FAILED]" marker with original text

**Scraping Errors**:
- No chapters found → check for one-shot, else exit
- Content not found → log error, skip chapter

**Name Mapping Errors**:
- Invalid JSON → prompt user to fix, retry validation
- Missing required fields → use defaults, continue

**File System Errors**:
- Cannot create directory → exit with error
- Cannot write file → log error, continue to next item

## Console Output System

### Console Module (src/console.rs)

**Purpose**: Lightweight ANSI color formatting for terminal output.

**Initialization**:
- Detect if colors should be used:
  - Disabled if `NO_COLOR` environment variable is set
  - Disabled if stdout is not a TTY (check with `std::io::stdout().is_terminal()` or `atty` crate)
  - Enabled otherwise

**Style Codes**:
```rust
enum Style {
    Bold,
    Dim,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
}

impl Style {
    fn code(&self) -> &'static str {
        match self {
            Style::Bold => "1",
            Style::Dim => "2",
            Style::Red => "31",
            Style::Green => "32",
            Style::Yellow => "33",
            Style::Blue => "34",
            Style::Magenta => "35",
            Style::Cyan => "36",
            Style::Gray => "90",
        }
    }
}

const RESET: &str = "\x1b[0m";
```

**Methods**:

- `style(text: &str, styles: &[Style]) -> String`: Wrap text in ANSI codes
  - Format: `\x1b[{codes}m{text}\x1b[0m`
  - Multiple styles joined with semicolon

- `label(label: &str, color: Style) -> String`: Create colored `[LABEL]` prefix
  - Uses `style()` with color and bold

- `info(message: &str)`: Print with blue `[INFO]` label
- `success(message: &str)`: Print with green `[OK]` label
- `warning(message: &str)`: Print with yellow `[WARN]` label
- `error(message: &str)`: Print with red `[ERROR]` label
- `step(message: &str)`: Print with cyan `[STEP]` label
- `section(message: &str)`: Print magenta bold section header with newline before
- `muted(text: &str) -> String`: Return dim gray styled text
- `progress(message: &str)`: Print with cyan `[..]` label, flush output using `std::io::Write::flush()`

**Global Instance**:
Use a lazy-initialized global or pass console instance through the application:

```rust
use once_cell::sync::Lazy;

pub static CONSOLE: Lazy<Console> = Lazy::new(Console::new);
```

Or use the `console` crate for a more feature-rich implementation.

## Testing

### Test Structure

**Test Files**:
- `tests/integration_tests.rs`: Integration tests for full workflows
- `tests/scraper_tests.rs`: Scraper-specific tests
- `src/*/mod.rs` or individual files: Unit tests in `#[cfg(test)]` modules

**Test Runner**:
- Run all tests: `cargo test`
- Run specific test: `cargo test test_name`
- Run with output: `cargo test -- --nocapture`

### Key Test Coverage

**Configuration**:
- Platform-specific config directory paths
- Config creation and defaults
- Config validation and error handling

**Argument Parsing**:
- Valid and invalid range specifications
- Duplicate option detection
- Negative value rejection

**Translation**:
- Empty text handling
- Text chunking at boundaries
- Retry logic with refusals
- Streaming progress display
- Message history management

**Name Mapping**:
- Vote recording and consensus
- Coverage tracking
- Text replacement
- Bad name filtering (honorifics, punctuation, whitespace)
- JSON validation and reload

**Scraping** (all platforms):
- URL pattern matching
- Novel info extraction (multiple layouts)
- Chapter list with pagination
- Content extraction with ruby kept as `｜base《reading》`
- One-shot detection

**Pixiv-Specific**:
- URL parsing for individual and series
- Unicode unescaping
- AJAX request handling and error cases
- Series pagination logic
- Empty title handling (fetch actual title)
- Rate limiting verification

### Testing Best Practices

**Mocking**:
- Use `mockito` or `wiremock` crate to mock HTTP requests
- Create mock implementations of traits for unit testing
- Use `tokio::time::pause()` for time-related tests
- Use `tempfile` crate for temporary directories

**Fixtures**:
- Create test configuration structs
- Use `tempfile::TempDir` for file operations
- Include sample HTML/JSON responses as const strings or test files

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use mockito::Server;
    
    #[tokio::test]
    async fn test_scraper_fetches_novel_info() {
        let mut server = Server::new();
        let mock = server.mock("GET", "/novel/123")
            .with_body(include_str!("fixtures/novel_page.html"))
            .create();
        
        let scraper = SyosetuScraper::new(&config);
        let info = scraper.get_novel_info(&server.url()).await.unwrap();
        
        assert_eq!(info.title, "Expected Title");
        mock.assert();
    }
}
```

## Platform-Specific Considerations

### Windows

**Path Handling**:
- Config directory: Use `dirs::config_dir()` which returns `%APPDATA%` on Windows
- Replace `:` with ` -` in name mapping filenames (colons not allowed in filenames)
- Use `std::path::Path` and `PathBuf` for cross-platform path handling

**Line Endings**:
- Rust handles line endings automatically with `\n`
- UTF-8 encoding used for all text files

### macOS

**Path Handling**:
- Config directory: `dirs::config_dir()` returns `~/Library/Application Support/`
- Use `std::path::Path` for path operations
- Colons allowed in filenames

### Linux/Unix

**Path Handling**:
- Config directory: `dirs::config_dir()` respects `$XDG_CONFIG_HOME` or falls back to `~/.config/`
- Use `std::path::Path` for path operations
- Colons allowed in filenames

**Editor Integration**:
- Check for Kate editor availability with `which::which("kate")`
- Launch Kate for name mapping review if available using `std::process::Command`
- Fallback to manual editing with user prompt

## Performance Considerations

### Rate Limiting

**Scraping**:
- Apply delay before each web request
- Configurable per-platform (Scraping.delay_between_requests_sec)
- Respect robots.txt and ToS (manual responsibility)

**Translation**:
- Apply delay between API requests
- Configurable (Translation.delay_between_requests_sec)
- Exponential backoff on retries

**Name Scouting**:
- Apply delay between API requests
- Configurable (NameScout.delay_between_requests_sec)
- Separate from main translation to allow different models/endpoints

### Memory Management

**Streaming**:
- OpenAI API responses streamed for real-time progress
- Full response accumulated in memory (necessary for validation)

**Chunking**:
- Large chapters split to stay within model context limits
- Chunks processed sequentially to maintain memory bounds

**File Handling**:
- Read entire files into memory (web novels typically < 50KB per chapter)
- Write atomically (no partial writes on error)

### API Cost Optimization

**Two-Tier API System**:
- Main API (Translation): Higher quality model
- Scout API (Name extraction): Can use cheaper/faster model
- Separate configurations allow cost/quality trade-offs

**Incremental Progress**:
- Name mappings saved after each successful scout chunk
- Translations saved per-chapter
- Resume capability (skip already translated chapters)

**Coverage Tracking**:
- Avoid re-scouting chapters for names
- Coverage list stored in name mapping JSON

## Security Considerations

### API Keys

**Storage**:
- Plain text in config file (user responsibility to protect)
- File permissions should be user-only (Unix: 600)
- Config location in user-specific directory

**Validation**:
- Check for placeholder value on startup
- Exit immediately if API key not configured

### Web Scraping

**User Agent**:
- Modern browser user agent used
- Identifies as legitimate browser to avoid blocks

**Cookies**:
- Syosetu: Set over18=yes cookie for 18+ content access
- No authentication cookies stored

**HTTPS**:
- All requests use HTTPS for API and web scraping
- Certificate validation via requests library

### Input Validation

**URLs**:
- Regex validation before processing
- Reject unrecognized patterns

**File Paths**:
- Sanitize filenames for security
- Use pathlib for safe path construction
- No shell command execution with user input

**JSON Data**:
- Schema validation for name mappings
- Handle malformed JSON gracefully

## Error Messages and Logging

### User-Facing Messages

**Info Level**:
- Progress updates (downloading, translating)
- File operations (saved, loaded)
- Status changes (found existing folder, skipping)

**Success Level**:
- Completed operations (downloaded chapter, saved file)
- Found resources (novel title, chapter count)

**Warning Level**:
- Recoverable errors (retry after failure)
- Skipped items (bad name mapping)
- Fallback behavior (using old selector)

**Error Level**:
- Fatal errors (cannot continue)
- Configuration problems (invalid API key)
- Network failures (cannot reach server)

### Debug Information

**Pixiv Scraper**:
- Uses Python logging module
- Logger: `__name__` (modules.pixiv)
- Levels: INFO for success, WARNING for recoverable errors, ERROR for failures
- Format: `%(asctime)s - %(name)s - %(levelname)s - %(message)s`
- Helpful for debugging API issues

## Dependencies and Versions

### Core Dependencies (Cargo.toml)

**reqwest**:
- HTTP client library with async support
- Used for all web scraping and API calls
- Features needed:
  - `cookies` for cookie jar support
  - `json` for JSON request/response handling
  - Default TLS backend (native-tls or rustls)
- Key functionality:
  - `Client` with persistent headers
  - Cookie management via `cookie_store`
  - Timeout support
  - Error handling with `reqwest::Error`

**scraper**:
- HTML parsing library built on `html5ever`
- Used for web scraping
- Features used:
  - CSS selectors via `Selector::parse()`
  - DOM navigation with `ElementRef`
  - Text extraction with `element.text()`
  - Element removal/manipulation

**serde** + **serde_json**:
- Serialization/deserialization framework
- Used for JSON parsing and generation
- Derive macros for struct serialization
- Features used:
  - `#[derive(Serialize, Deserialize)]`
  - Custom serialization options
  - Pretty printing with `serde_json::to_string_pretty()`

**tokio**:
- Async runtime for Rust
- Used for async HTTP requests and file I/O
- Features needed:
  - `rt-multi-thread` for multi-threaded runtime
  - `macros` for `#[tokio::main]`
  - `time` for delays and timeouts
  - `fs` for async file operations (optional)

**clap**:
- Command-line argument parsing
- Used for CLI interface
- Features needed:
  - `derive` for derive macros
- Provides validation and help generation

**regex**:
- Regular expression library
- Used for URL matching and text processing
- Thread-safe compiled regex with `Regex`

**dirs**:
- Cross-platform directory paths
- Used for config file location
- Key functions:
  - `dirs::config_dir()` for config directory
  - `dirs::home_dir()` for home directory

**toml** or **rust-ini**:
- Configuration file parsing
- INI format support for config.ini

**crossterm** or **console**:
- Terminal manipulation library
- Used for colored output and progress display
- Features:
  - ANSI color codes
  - Terminal detection (is_terminal)
  - Cursor manipulation

**thiserror** or **anyhow**:
- Error handling libraries
- `thiserror` for custom error types
- `anyhow` for application-level error handling

### Optional Dependencies

**async-openai**:
- OpenAI API client for Rust
- Alternative: use `reqwest` directly with manual JSON handling

**once_cell** or **lazy_static**:
- Lazy initialization for global state
- Used for compiled regex patterns and console instance

**encoding_rs**:
- Character encoding detection and conversion
- May be needed for non-UTF-8 web pages

**tempfile**:
- Temporary file/directory creation
- Used in tests

**mockito** or **wiremock**:
- HTTP mocking for tests

**which**:
- Find executables in PATH
- Used for editor detection

### Rust Standard Library

Key modules used:
- `std::path::{Path, PathBuf}`: Cross-platform path handling
- `std::fs`: File system operations
- `std::io`: Input/output traits and types
- `std::collections::HashMap`: Hash map data structure
- `std::env`: Environment variables
- `std::process::Command`: External process execution
- `std::time::{Duration, Instant}`: Time handling
- `std::thread::sleep`: Blocking delay (or `tokio::time::sleep` for async)

## Implementation Checklist

### Core Functionality
- [ ] Platform-specific config directory detection (`dirs` crate)
- [ ] INI/TOML config file reading and writing
- [ ] Default config creation with all sections
- [ ] Config validation and auto-completion

### HTTP Client (`reqwest`)
- [ ] Async client with persistent headers and cookies
- [ ] User-Agent spoofing
- [ ] Cookie management for age gates
- [ ] Rate limiting with configurable delays (`tokio::time::sleep`)
- [ ] Timeout support (30 seconds for Pixiv)
- [ ] Error handling for all network error types

### HTML Parsing (`scraper`)
- [ ] CSS selector support (critical for all scrapers)
- [ ] Element attribute access
- [ ] Text extraction from elements
- [ ] Ruby extraction to `｜base《reading》`
- [ ] Relative URL resolution (`url` crate)

### OpenAI API Client
- [ ] Streaming completions support (SSE parsing)
- [ ] Custom base_url configuration
- [ ] Message history management
- [ ] Error handling and retries

### Scraper System
- [ ] Trait-based scraper interface
- [ ] Regex-based URL matching
- [ ] Scraper registry with dynamic dispatch
- [ ] Syosetu: pagination, ruby extraction, one-shot detection
- [ ] Kakuyomu: CSS class prefix matching
- [ ] Pixiv: AJAX requests, JSON parsing, Unicode unescaping, series pagination

### Translation
- [ ] Text chunking by lines then words
- [ ] Streaming with progress display (1-second updates)
- [ ] ANSI terminal codes for progress (`crossterm`/`console`)
- [ ] Message history with configurable length
- [ ] Refusal phrase detection
- [ ] Retry with exponential backoff

### Name Mapping
- [ ] JSON persistence with platform-specific paths (`serde_json`)
- [ ] Vote-based consensus system
- [ ] Coverage tracking
- [ ] Bad name filtering (regex patterns)
- [ ] Honorific detection
- [ ] Text replacement (longest first)
- [ ] JSON schema validation via serde

### Name Scout
- [ ] Separate API configuration
- [ ] JSON response parsing with fence removal
- [ ] Iterator/stream-based processing
- [ ] Incremental save after each chunk
- [ ] Empty title fallback (fetch individual chapter)

### File System
- [ ] UTF-8 encoding for all text files
- [ ] Filename sanitization (platform-specific)
- [ ] Directory creation with parents (`std::fs::create_dir_all`)
- [ ] Atomic file writes (write to temp, then rename)
- [ ] Folder naming (new and old format compatibility)

### Console Output
- [ ] ANSI color support detection (NO_COLOR, TTY)
- [ ] Styled output with colors and attributes
- [ ] Progress indicators
- [ ] Line clearing and updating

### Error Handling
- [ ] Graceful degradation (skip failed chapters)
- [ ] Retry logic with limits
- [ ] User-friendly error messages
- [ ] Validation at each stage
- [ ] Custom error types with `thiserror`

### Testing
- [ ] Mock HTTP requests (`mockito`/`wiremock`)
- [ ] Mock trait implementations
- [ ] Temporary directories (`tempfile`)
- [ ] Platform-specific path tests
- [ ] Edge cases (empty, malformed data)
- [ ] Async test support (`#[tokio::test]`)

## API Response Examples

### Pixiv AJAX Response Format

**Individual Novel** (`/ajax/novel/{id}`):
```json
{
  "error": false,
  "message": "",
  "body": {
    "id": "25184613",
    "title": "\\u7b2c\\u4e00\\u7ae0",
    "content": "Story content with \\u3042\\u3044\\u3046 escapes",
    "userId": "12345",
    "userName": "Author Name",
    "seriesId": "11075024",
    "seriesNavData": {...}
  }
}
```

**Series Info** (`/ajax/novel/series/{id}`):
```json
{
  "error": false,
  "message": "",
  "body": {
    "id": "11075024",
    "title": "Series Title",
    "userId": "12345",
    "userName": "Author Name"
  }
}
```

**Series Content** (`/ajax/novel/series_content/{id}?limit=30&last_order=0&order_by=asc`):
```json
{
  "error": false,
  "message": "",
  "body": {
    "page": {
      "seriesContents": [
        {
          "id": "25184613",
          "title": "Chapter Title",
          "order": 1,
          "available": true
        }
      ]
    }
  }
}
```

### OpenAI Streaming Format

**Request**:
```rust
#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    stream: bool,
}

#[derive(Serialize)]
struct Message {
    role: String,
    content: String,
}

let request = ChatRequest {
    model: "gpt-4o-mini".to_string(),
    messages: vec![
        Message { role: "system".to_string(), content: "System prompt...".to_string() },
        Message { role: "user".to_string(), content: "Text to translate...".to_string() },
    ],
    stream: true,
};
```

**Response Stream Chunks** (Server-Sent Events):
```
data: {"choices": [{"delta": {"content": "Trans"}, "index": 0}]}
data: {"choices": [{"delta": {"content": "lated "}, "index": 0}]}
data: {"choices": [{"delta": {"content": "text"}, "index": 0}]}
data: [DONE]
```

**Non-Streaming Response**:
```json
{
  "choices": [
    {
      "message": {
        "role": "assistant",
        "content": "Response text"
      },
      "index": 0
    }
  ]
}
```

**Rust Response Structs**:
```rust
#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: Option<ResponseMessage>,
    delta: Option<Delta>,
    index: u32,
}

#[derive(Deserialize)]
struct ResponseMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct Delta {
    content: Option<String>,
}
```

## Glossary

**Syosetu**: Japanese web novel platform (ncode.syosetu.com)
**Kakuyomu**: Japanese web novel platform by Kadokawa (kakuyomu.jp)
**Pixiv**: Japanese art and novel sharing platform (pixiv.net)
**Ruby**: HTML tag for furigana (pronunciation guide above kanji)
**Furigana**: Small hiragana characters showing pronunciation of kanji
**LLM**: Large Language Model (e.g., GPT-4)
**AJAX**: Asynchronous JavaScript and XML (API endpoint pattern)
**ANSI**: Terminal control codes for colors and formatting
**TTY**: Teletype, refers to terminal output device
**INI**: Configuration file format with sections and key-value pairs
**XDG**: X Desktop Group, standardizes Linux desktop configurations
**Unicode Escape**: `\uXXXX` format for Unicode characters in JSON/text

## Version History

This specification documents version 0.1.0 of Tsundoku (Rust implementation).

Based on the original Python syosetu_grabber v1.0.0 specification.

Key features:
- Multi-platform scraping (Syosetu, Kakuyomu, Pixiv)
- OpenAI-compatible translation with streaming progress
- Persistent name mapping with vote-based consensus
- Automatic character name extraction and substitution
- Incremental progress saving and resume capability
- Cross-platform configuration and file handling
- Comprehensive error handling and retry logic
- Async/await based architecture using Tokio

---

**End of Specification**

This document contains all the information necessary to implement the Tsundoku system in Rust while maintaining compatibility with the original's behavior, file formats, and configuration system.
