# Golden fixtures

These pin the exact behaviour of narrator's Python EPUB chunker
(`/home/fernando/git/narrator/app/book.py`) so the Rust port can assert
byte-exact parity. The Python repo is read-only here — nothing in this
directory ever writes to it, and the vault epub is copied to a temp dir before
ebooklib touches it.

## Regenerate

```sh
./regen.sh          # JSON fixtures only (fixture.epub left alone)
./regen.sh --epub   # also rebuild fixture.epub — changes its bytes
```

`regen.sh` shells out to `uv run --no-project --with ebooklib --with
beautifulsoup4 --with lxml`, so there is nothing to install. If you'd rather
have a persistent venv, `uv venv .venv` here works too; `.venv` is gitignored.

**`fixture.epub` is the artefact of record.** ebooklib's zip writer stamps the
current clock into every member, so rebuilding it produces a semantically
identical but byte-different file — which invalidates `fixture_plan.json` and
`fixture_est.json`. `make_fixture_epub.py` therefore refuses to overwrite
without `--force`, and `--epub` implies regenerating the derived JSON too.

## Scripts

| File | What |
|---|---|
| `make_fixture_epub.py` | Builds the synthetic `tests/fixtures/fixture.epub` in a temp dir, then copies it in. Exercises multi-spine, an image-only chapter, blockquote nesting, h1–h3 (and an ignored h4), stripped script/style/nav/header/footer, whitespace soup, abbreviations, quote-terminated sentences, an over-long clause-split sentence, unspeakable chunks, CJK/Cyrillic/accented text, and a TOC that leaves one chapter untitled. |
| `gen_golden.py` | Imports `book.py` by path and emits every JSON fixture. Never imports `app/server.py` (import-time side effects); `reading_log()` and `note_filename()` are transcribed verbatim into the script and must be kept in sync by hand. |

## Outputs (`tests/fixtures/`)

| File | Contents |
|---|---|
| `fixture.epub` | The synthetic book. Committed. |
| `chunking_cases.json` | `{input, max_chars, speakable, sentences, chunks}` per case. |
| `fixture_plan.json` | `build_plan(extract_chapters(fixture.epub), 300)` verbatim. |
| `fixture_est.json` | `est_chunk_s`/`est_chapter_s` over that plan, keyed by chapter index, plus `CHARS_PER_SEC`. |
| `real_book_counts.json` | Per-chapter shape of one real epub, with its `sha256`. |
| `real_book_head_plan.json` | Full plan for that epub's first 3 chapters. |
| `real/<book>.epub` | The real epub itself, bundled only when ≤ 12 MB. If `bundled_path` is `null` in `real_book_counts.json`, the Rust test should skip unless the file is found at `source_path` with the recorded sha256. |
| `reading_log.json` / `.md` | The positions dict and the byte-exact Reading Log markdown (trailing newline included). |
| `note_markdown.json` | Fleeting-note `filename` + `markdown` for three synthetic memos. |

## Behaviours the fixtures deliberately lock in

These are bugs-as-specified. The Rust port must reproduce them, not fix them.

- **The abbreviation lookbehinds are inert.** `_ABBR` sits *before*
  `(?<=[.!?])` in the pattern, so at the split position it inspects the text
  ending in `r.` rather than `Dr`. `"Dr. Smith went home."` still splits into
  `["Dr.", "Smith went home."]`.
- **Closing quotes and brackets are eaten.** `["\'”’)\]]*\s+` is part of the
  *separator*, so `He said "go." Then left.` yields `['He said "go.',
  'Then left.']` — the closing `"` is gone from the output.
- **Blockquote nesting duplicates text.** `find_all(["p", …, "blockquote"])`
  returns the blockquote *and* its inner `<p>`s, so the passage is emitted
  twice (flattened, then per-paragraph).
- **An over-long sentence with no clause punctuation is never split** and is
  emitted as one chunk longer than `max_chars`.
- **`sentences("")` is `[]`**, and so is `sentences("   ")`.
- **Chapter indices are post-filter.** Chapters with no `p`/`h1..3`/
  `blockquote` are dropped by `extract_chapters`, so `chap_05.xhtml` lands at
  plan index 3, not 4.
- **A missing TOC title falls back to `paragraphs[0][:60]`** — a hard 60-char
  truncation, not word-aware.
- **Reading Log cells are not markdown-escaped**, so a `|` in a book stem or
  chapter title really does break the table.
