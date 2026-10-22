# Tsundoku

A Japanese web novel downloader and translator supporting Syosetu, Kakuyomu, and Pixiv platforms.

## Features

- Downloads web novels from multiple Japanese platforms:
  - Syosetu (ncode.syosetu.com, novel18.syosetu.com)
  - Kakuyomu (kakuyomu.jp)
  - Pixiv (pixiv.net/novel)
- Automatic character name extraction and mapping using LLM
- Translation using OpenAI-compatible APIs
- Persistent name mapping with vote-based consensus
- Incremental progress saving and resume capability
- Streaming translation with real-time progress display
- `tsundoku-epub`: builds EPUB volumes from the translated chapters

## Installation

### Pre-built Binaries

Download the latest release for your platform from the [Releases page](https://github.com/ripdog/tsundoku/releases):

- **Linux**: `tsundoku-linux-x86_64.tar.gz` (glibc) or `tsundoku-linux-x86_64-musl.tar.gz` (musl)
- **macOS**: `tsundoku-macos-x86_64.tar.gz` (Intel) or `tsundoku-macos-aarch64.tar.gz` (Apple Silicon)
- **Windows**: `tsundoku-windows-x86_64.exe.zip`

Extract and run the binary, or move it to a directory in your PATH.

### From AUR (Arch Linux)

```bash
yay -S tsundoku
# or
paru -S tsundoku
```

### From Source

Requires Rust 1.92+ (edition 2024 support)

```bash
git clone https://github.com/ripdog/Tsundoku.git
cd Tsundoku
cargo build --release
cargo install --path .
```

## Configuration

On first run, Tsundoku will create a default configuration file:

- Linux: `~/.config/Tsundoku/config.toml`
- macOS: `~/Library/Application Support/Tsundoku/config.toml`
- Windows: `%APPDATA%\Tsundoku\config.toml`

Edit the configuration file to add your OpenAI-compatible API key and customize settings.

### Required Configuration

At minimum, you must set your API key:

```toml
[api]
key = "your_api_key_here"
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini"
```

### Optional Configuration

#### Separate Scout API

You can use a different (cheaper) model for character name extraction:

```toml
[scout_api]
key = "your_scout_api_key"
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini"
```

#### Editor for Name Review

Specify which editor to use when reviewing name mappings:

```toml
[paths]
editor_command = "kate"  # or "vim", "nano", "code", "notepad", etc.
```

If not specified, Tsundoku will auto-detect a suitable editor based on your platform.

#### Chunk Size

Chapters are sent to the model in chunks. Sizes are counted in characters (not bytes), and chunks end at scene breaks (`◇◇◇`, `＊＊＊`, `* * *`, wide blank gaps) where possible. A scene longer than the limit is split into pieces of about equal size at paragraph boundaries, and an over-long paragraph at sentence ends.

```toml
[translation]
chunk_size_chars = 1500  # default

[name_scout]
chunk_size_chars = 900   # default
```

#### Reply Length Limit

Each translation request sends `max_tokens = max(512, ceil(max_tokens_per_char × source characters))`, so a model stuck in a loop stops instead of running forever. A reply cut off at that limit is retried like any other failure; if it is still cut off on the last attempt, it is kept and reported as truncated. Set the factor to `0` to send no limit.

```toml
[translation]
max_tokens_per_char = 2.0  # default
```

## Usage

Download and translate a novel:

```bash
tsundoku https://ncode.syosetu.com/n1234ab/
```

### Options

- `--start N`: Start downloading from chapter N (1-based)
- `--end N`: Stop downloading at chapter N (1-based, inclusive)
- `--no-name-pause`: Skip manual name mapping review pause
- `--debug`: Enable scraper debug logging

### Examples

Download chapters 5-10 only:

```bash
tsundoku --start 5 --end 10 https://ncode.syosetu.com/n1234ab/
```

Download without pausing for name review:

```bash
tsundoku --no-name-pause https://kakuyomu.jp/works/1234567890
```

### Pixiv Login Cookies

Some Pixiv novels require login to access content. Tsundoku can load browser-exported
Netscape cookie files automatically.

1. Export cookies as a Netscape `*.txt` file (many browser extensions support this).
2. Place the file in your Tsundoku config directory:
   - Linux: `~/.config/Tsundoku/`
   - macOS: `~/Library/Application Support/Tsundoku/`
   - Windows: `%APPDATA%\Tsundoku\`
3. Ensure the filename contains `pixiv` (case-insensitive).

Tsundoku scans the config directory recursively, picks the most recently modified
matching file, and sends those cookies to Pixiv requests.

## How It Works

1. **Download**: Scrapes the novel chapters from the source website
2. **Name Scout**: Extracts character names using LLM and builds a mapping
3. **Review** (optional): Pause to let you manually review/edit name mappings
4. **Translate**: Applies name mappings and translates content using LLM
5. **Save**: Stores translated chapters as text files

### Ruby (Furigana)

Downloaded originals keep every ruby annotation as `｜base《reading》`, the
notation Syosetu and Kakuyomu authors write by hand. Pixiv `[[rb:base > reading]]`
tags are converted to the same form, and other Pixiv tags (`[newpage]`,
`[chapter:…]`, `[[jumpuri:…]]`, `[jump:…]`, images) become plain text or are
removed. Kakuyomu blank paragraphs are kept as empty lines so scene breaks survive.

- The name scout sees every reading next to its base, which helps it romanise names.
- The translator gets phonetic readings (hiragana on kanji) and emphasis dots
  removed, blank-base ruby replaced by its reading, and gloss ruby such as
  `聖剣《エクスカリバー》` kept so the intended word (Excalibur) is used.

Kakuyomu originals downloaded by older versions contain readings inlined as
`base（reading）`, which can't be undone safely. Delete those `Original/*.txt`
files to re-download them with ruby; already translated chapters are unaffected.

### Resume Capability

Tsundoku is fully resumable at every stage:

- **Already downloaded chapters are skipped** - Original text files are checked before downloading
- **Already translated chapters are skipped** - Translated files are checked before translation
- **Name scouting tracks coverage** - Chapters that have been scanned for names won't be scanned again
- **Progress is saved incrementally** - Name mappings are saved after each successful API call

This means you can:
- Stop and restart the program at any time
- Re-run the same command to continue where you left off
- Add new chapters to an existing series by running with a wider `--end` range
- Retry failed translations without redoing successful ones

Simply run the same command again, and Tsundoku will intelligently skip completed work.

### Output Structure

Multi-chapter novels:
```
[syosetu: n1234ab] Novel Title/
├── Original/
│   ├── 001 - Chapter 1 Title.txt
│   ├── 002 - Chapter 2 Title.txt
│   └── ...
├── 001 - Chapter 1 Title.txt
├── 002 - Chapter 2 Title.txt
└── ...
```

One-shot stories:
```
[pixiv: 12345678] Story Title/
├── original.txt
└── oneshot.txt
```

## Building EPUBs

`tsundoku-epub` turns a novel folder's translated chapters into EPUB 3 books, 100 chapters per volume by default:

```bash
tsundoku-epub "[syosetu: n1234ab] Novel Title"
```

Volumes are written to `<novel folder>/EPUB/` as `Novel Title - Vol. 01 (0001-0100).epub`, `Vol. 02 (0101-0200)`, and so on. Volume ranges follow chapter numbers, so rerunning after more chapters are translated rebuilds the same files with the new chapters added. Chapters missing from the middle of a volume are reported as warnings; chapters after the last translated one are not.

Each non-empty line becomes a paragraph, consecutive lines starting with `- ` or a single `・` become a list, and lines made only of break glyphs (`◆　◆　◆`, `* * *`, `---`) become scene breaks. Authors also mark scene changes with three or more blank lines, which models collapse; `tsundoku` therefore replaces each such gap with a `* * *` line before translating, so the break survives into the book. Dialogue made of punctuation, such as `「……」`, stays as text. Straight quotes and apostrophes are converted to curly ones (`"It's"` → `“It’s”`) and every three periods to an ellipsis (`...` → `…`), so every book uses one style even when the model mixed both. Japanese punctuation the model left in is converted too: `「」`/`『』` to curly quotes, `・・・` to `…`, a single `・` to a space between names or `·` elsewhere, `――` and `--` to `—`, and full-width `！？～` and digits to ASCII. The `.txt` files are left untouched. Each book has a title page, a table of contents (EPUB 3 nav plus NCX for older readers), and series metadata that Calibre and KOReader use to order volumes.

`tsundoku` keeps a `book.json` in the novel folder with the book title, original title, source URL, and each translated chapter's exact title and original title. `tsundoku-epub` uses it for headings (file names lose characters such as `?`) and for the EPUB's title, author, and source. `author` is never filled automatically; set it by hand (or later in Calibre). Edits to `title` and `author` are preserved; chapters translated before `book.json` existed fall back to their file names.

```json
{
  "title": "Novel Title",
  "original_title": "原題",
  "author": null,
  "source_url": "https://ncode.syosetu.com/n1234ab/",
  "chapters": {
    "12": { "title": "Chapter 12: Peaceful? Days...", "original_title": "第十二話　平穏な？日々" }
  }
}
```

Options:

- `--chapters-per-volume N`: chapters per EPUB (default 100)
- `--title TEXT`: book title (defaults to `title` in `book.json`, then the folder name without its `[site: id]` prefix)
- `--author TEXT`: author name for the metadata (defaults to `author` in `book.json`)
- `-o, --output DIR`: output folder (default `<novel folder>/EPUB`)

## Name and Terminology Mapping

Tsundoku automatically extracts character names and story-specific terminology into a persistent mapping database. Scout votes choose canonical English renderings unless a manual override is present. Before each chapter is translated, matching canonical glossary terms are included in every translation request, keeping recurring items, skills, ranks, organizations, and fictional concepts consistent across the novel.

Mappings are stored in:

- Linux: `~/.local/share/Tsundoku/names/`
- macOS: `~/Library/Application Support/Tsundoku/names/`
- Windows: `%APPDATA%\Tsundoku\names\`

The default mapping file format is:

```json
{
  "names": {
    "太郎": {
      "part": "given",
      "votes": {
        "Taro": 5,
        "Tarou": 2
      },
      "override": "Tarou",
      "english": "Tarou",
      "count": 2,
      "gender_votes": {
        "male": 3,
        "female": 1
      },
      "gender_override": "male",
      "gender": "male",
      "note": "Ruri's older brother"
    }
  },
  "terms": {
    "魔石": {
      "votes": {
        "Magic Stone": 3,
        "Mana Stone": 1
      },
      "english": "Magic Stone",
      "count": 3
    }
  },
  "coverage": [1, 2, 3],
  "term_coverage": [1, 2, 3]
}
```

`override` is optional for both names and terms and wins over all vote totals. It may name a valid rendering absent from `votes`; in that case the recalculated `count` is omitted. `english` and `count` are recalculated fields, not manual controls. Delete `override` or set it to `null` to resume vote-based selection. Scouting continues to tally every valid vote while an override is active, so removing it immediately uses all accumulated votes.

Each name can also carry a gender and a short note. The scout votes for a gender only when it quotes Japanese words from the text that show it (彼, 彼女, 兄, 娘, …), and fills `note` (a role or relationship, at most 80 characters) only while it is empty, so a hand-written note is never overwritten; delete it to let the scout refill it. `gender` is recalculated like `english`: `gender_override` wins; otherwise the leading gender needs at least two votes and at least 75% of `gender_votes`, so relatives sharing a family name stay genderless. Set `"gender_override": "unknown"` to never state a gender for a character; `unknown` is valid only as an override. All four keys are optional and omitted when empty, so older files load unchanged. An older Tsundoku binary reads such a file but drops these keys when it saves it.

Before each chapter is translated, the names whose Japanese occurs in the chapter and that have a gender or a note are listed in its translation prompt, most frequent first, so the model uses the right pronouns even where the Japanese omits the subject:

```
Characters in this chapter (use matching pronouns even where the Japanese omits the subject):
- Kirigaya: male; Timer Association liaison
```

By default at most 30 characters are listed; set `translation.cast_max_entries` to `0` to disable the list.

Terms are automatically collected by the scout and vote-consolidated across chapters. Only canonical terms present in the chapter being translated, after name substitution, are added to its translation prompt; overridden terms come first, then the most-voted. By default, at most 100 terms are included; set `translation.glossary_max_entries` to `0` to disable glossary injection. Existing mapping files without `terms` or `term_coverage` remain valid; Tsundoku scouts terminology for their covered chapters without recording duplicate name votes.

Names take precedence over terms. Any term whose Japanese contains a mapped name (for example `ジュカ` or `ジュカ王国` when `ジュカ` is a name) is dropped from the mapping, because name substitution already rewrites it before translation. Single-character terms (`市` would match inside `都市`), terms rendered as themselves (`HP → HP`), and renderings that still contain Japanese or Chinese script are rejected. Orthographic variants of a name keep separate entries but share one rendering, chosen from their pooled votes: small vs. full-size vowels (`ヴィヴァル`/`ヴィヴアル`), hiragana written for katakana (`ぺーリス`/`ペーリス`), and dashes used as long-vowel marks (`ヒ―データ`/`ヒーデータ`). An override on any variant applies to all of them. Long-vowel differences are not merged, since `ヒーラ` and `ヒーラー` ("healer") are different words.

All-katakana names are substituted only as whole katakana words, so a name `ロー` does not rewrite `スローダ`; `・` counts as a separator. Single-character names (`女` would rewrite every `彼女`), titles, ranks, kinship words, archaic pronouns, and interjections (`宰相`, `将軍`, `父上`, `そこもと`, `ひー`, …) are never kept as names, nor are suffixed forms such as `バーサーム家`, `田中一家`, `メインティア王`, `ミーク姫`, or any name ending in a royal title (`…王`, `…王子`, `…殿下`): the bare name inside them is substituted on its own, so the suffix's meaning reaches the translator. All of this cleanup runs whenever a mapping file is loaded, so existing files are fixed in place on the next run.

The scout also sees the renderings already chosen for any names and terms that appear in the chunk it is reading, and is told to reuse them. That keeps it from romanizing the same name differently in every chapter. Changing an `override` therefore also steers later scouting toward that rendering.

To apply changed mappings to an already translated chapter, delete that chapter's translated output file before rerunning Tsundoku.

### Chapter Summaries

With `name_scout.summaries = true`, the scout also writes a short English synopsis of each chapter (at most 40 words per chunk, 120 per chapter) to `{module}: {novel_id}.summaries.json` next to the mapping file (`{module} - {novel_id}.summaries.json` on Windows), kept separate so the mapping you review does not grow:

```json
{ "version": 1, "chapters": { "12": "Tatsuya brings Ruri to the village; Monika audits the inn." } }
```

A chapter counts as summarized once its key exists, even with an empty synopsis. Summaries are off by default because they add scout output. When translating chapter N, the synopses of the previous `translation.summary_chapters` chapters (default 2; `0` disables) are added at the end of the prompt as context; chapters without a synopsis are skipped. Before injection, any losing rendering of a name is replaced with its selected one, so an override made after scouting also fixes older synopses:

```
Story so far (context only; do not translate):
- Ch 10: …
- Ch 11: …
```

```toml
[name_scout]
summaries = true

[translation]
summary_chapters = 2
```

## Development

### Building

```bash
cargo build
```

### Testing

```bash
cargo test
```

### Running

```bash
cargo run -- https://ncode.syosetu.com/n1234ab/
```

### Scouting Without Translation

To download or reuse original chapters, build the name and terminology mapping, review it, and exit before any translation request:

```bash
cargo run -- --scout-only https://ncode.syosetu.com/n1234ab/
```

This mode reuses existing originals from the story folder and downloads only missing chapters, including when a novel’s chapter count changes its filename padding. If no story folder exists, it creates one using the original title without calling the translation API. Use `--no-name-pause` only for unattended scouting; otherwise Tsundoku pauses after scouting so you can correct the mapping before a later translation run. `--scout-only` cannot be combined with `--no-name-scout`.

Turning on `name_scout.summaries` for a novel that is already scouted back-fills the missing synopses on the next run (including `--scout-only`): chapters already covered for names and terms get one summary-only request per chunk, which records no name or term votes and does not trigger the review pause.

### Creating a Release

To create a new release:

1. Update version in `Cargo.toml`
2. Run `cargo build` to update `Cargo.lock`
3. Commit the changes: `git commit -am "Bump version to X.Y.Z"`
4. Create and push a tag: `git tag vX.Y.Z && git push origin vX.Y.Z`

The GitHub Actions workflow will automatically:
- Create a GitHub release
- Build and upload cross-platform binaries
- Build the Arch Linux package with updated PKGBUILD
- Update the PKGBUILD in the repository with the new version and checksum

## License

Licensed under the GNU General Public License v3.0 or later. See [LICENSE](LICENSE) for details.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## Acknowledgments

Inspired by the original Python-based syosetu_grabber project.
