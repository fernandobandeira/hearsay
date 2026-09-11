#!/usr/bin/env python3
r"""Emit golden JSON fixtures pinning narrator's Python chunker behaviour.

Reads (never writes) the reference implementation at
/home/fernando/git/narrator/app/book.py. app/server.py is deliberately NOT
imported - it has import-time side effects - so the two functions we need from
it (reading_log, note_filename) and the note() body_md template are transcribed
verbatim below and must be kept in sync by hand.

Outputs into tests/fixtures/:
    chunking_cases.json     sentences()/chunk_paragraph()/is_speakable()
    fixture_plan.json       build_plan(extract_chapters(fixture.epub), 300)
    fixture_est.json        est_chunk_s / est_chapter_s over that plan
    real_book_counts.json   per-chapter shape of one real epub
    real_book_head_plan.json  full plan for that epub's first 3 chapters
    reading_log.json/.md    the Reading Log renderer's input and exact output
    note_markdown.json      fleeting-note filenames and bodies
"""
import hashlib
import json
import re
import shutil
import sys
import tempfile
from pathlib import Path
from urllib.parse import quote

NARRATOR_APP = "/home/fernando/git/narrator/app"
sys.dont_write_bytecode = True     # never drop a .pyc into the read-only repo
sys.path.insert(0, NARRATOR_APP)
import book  # noqa: E402  (the reference implementation, read-only)

REPO = Path(__file__).resolve().parents[2]
FIX = REPO / "tests" / "fixtures"
FIXTURE_EPUB = FIX / "fixture.epub"
REAL_DIR = FIX / "real"

# A real epub, read out of Fernando's vault. The vault is never modified: the
# file is copied to a temp dir before ebooklib touches it.
REAL_SOURCE = Path("/home/fernando/git/obsidian-notes/03 - Resources/Books/"
                   "Business/7 Powers (2016).epub")
REAL_COPY_MAX_BYTES = 12 * 1024 * 1024

MAX_CHARS = 300


def write_json(name, obj):
    p = FIX / name
    p.write_text(json.dumps(obj, ensure_ascii=False, indent=2) + "\n",
                 encoding="utf-8")
    return p


# ---------------------------------------------------------------- chunking
SEED = "The quick brown fox jumps over the lazy dog and keeps going "


def exactly(n):
    """A single sentence of exactly n characters, terminated with a period and
    carrying no trailing whitespace (so sentences() does not shorten it)."""
    s = (SEED * (n // len(SEED) + 2))[: n - 1]
    if s.endswith(" "):
        s = s[:-1] + "z"
    return s + "."


def padded(n, pad):
    """exactly(n) plus `pad` trailing spaces - sentences() strips them, so the
    chunk is n chars even though the input is n+pad."""
    return exactly(n) + " " * pad


LONG_SENTENCE = (
    "When the renderer finally reaches a paragraph of this length it must decide, "
    "without any help from the author, where the natural seams are, and so it looks "
    "for commas, for semicolons; for colons: and for em dashes—those being the only "
    "boundaries it trusts—because cutting mid-phrase produces audible glitches, "
    "breath in the wrong place, and a listener who reaches for the pause button "
    "instead of the next chapter, which is the one outcome the whole pipeline exists "
    "to avoid."
)

NINE_HUNDRED = (
    "A sentence with no internal punctuation at all cannot be split on clause "
    "boundaries because there are none to find and so the whole of it is handed to "
    "the renderer in a single piece no matter how long it grows which is exactly "
    "what happens here as the words continue to accumulate past the point where any "
    "reasonable author would have stopped and inserted a full stop or at the very "
    "least a comma but this one does not because the entire purpose of the string is "
    "to drive the chunker down the branch where the buffer never exceeds the limit "
    "since there is only one part to iterate over and therefore the output is a "
    "single chunk that is very much longer than the maximum character count that was "
    "requested by the caller which is the surprising behaviour this fixture exists to "
    "record for the benefit of whoever ports it."
)

CASES = [
    # --- baseline
    "",
    "   ",
    "\n\t  ",
    "One sentence only.",
    "One sentence only",
    "First. Second. Third.",
    "First.Second.Third.",          # no space after '.' -> no split
    "First.  Second.   Third.",     # multiple spaces
    "First.\nSecond.\n\nThird.",    # newline as the whitespace
    "First.\tSecond.",
    # --- the abbreviation lookbehinds (the inert-lookbehind bug lives here)
    "Dr. Smith went home. He slept.",
    "Mr. Brown and Mrs. Green and Ms. White met.",
    "It was St. Peter, Jr. and Sr. together.",
    "Cats vs. dogs. That is the question.",
    "Tokenizers, e.g. this one, are slow.",
    "Tokenizers, i.e. this one, are slow.",
    "Bananas, apples, etc. were on the list.",
    "dr. smith went home. he slept.",          # lowercase: lookbehind is case-sensitive
    "See Dr. Smith.",
    "vs. that",
    "e.g. this",
    # --- quotes and brackets swallowed by the trailing character class
    'He said "go." Then left.',
    "She replied 'fine.' Then stayed.",
    "He said “go.” Then left.",
    "He said ‘go.’ Then left.",
    "A bracketed one ends here.] And another (like so.) continues.",
    "Ends with a paren.) Next sentence.",
    'Nested closers."’) Next sentence.',
    "Question? Answer! Statement.",
    'Question?" Answer!” Statement.',
    # --- ellipses and dots
    "Wait… then go. Done.",
    "Wait... then go. Done.",
    "He paused . . . and left. Done.",
    # --- numbers, initials, acronyms
    "It weighs 3.5 kg. Then it fell.",
    "He lives in the U.S.A. Then he left.",
    "See Fig. 2 for details. Then read on.",
    "Chapter 1.2.3 covers it. Next.",
    "Pi is 3.14159 exactly. Or nearly.",
    "Version 2.0. Shipped.",
    # --- unspeakable / no alphanumerics
    "…",
    "“…”",
    "———",
    "!!!",
    "   ...   ",
    "123",
    # --- unicode
    "Всё смешалось в доме Облонских. Это второе предложение.",
    "彼を知り己を知れば百戦して殆うからず。二つ目の文です。",
    "中文段落没有西文句号因此不会被切分",
    "Café naïve résumé — élève über Sträße. Segunda fração, com acentuação.",
    "Emoji \U0001f3a7 survive. Second \U0001f4da sentence.",
    # --- sizes around max_chars
    exactly(299),
    exactly(300),
    exactly(301),
    exactly(900),
    padded(300, 5),
    padded(301, 5),
    exactly(150) + " " + exactly(150),
    LONG_SENTENCE,
    NINE_HUNDRED,
    # --- packing many short sentences across the limit
    " ".join(f"Sentence number {i} here." for i in range(1, 31)),
    # --- clause splitting shapes
    "A" * 150 + ", " + "B" * 150 + "; " + "C" * 150 + ": " + "D" * 150 + ".",
    "One, two, three—four; five: six. " + "X" * 320 + ".",
    # --- whitespace handling inside a paragraph
    "  leading and trailing whitespace.   ",
    "collapsed    spaces are NOT collapsed by sentences(). Only by extract_chapters.",
]


def chunking_cases():
    out = []
    for s in CASES:
        out.append({
            "input": s,
            "max_chars": MAX_CHARS,
            "speakable": book.is_speakable(s),
            "sentences": book.sentences(s),
            "chunks": book.chunk_paragraph(s, MAX_CHARS),
        })
    return out


# ---------------------------------------------------------------- estimates
def est_for_plan(plan):
    chunks, chapters = {}, {}
    for ch in plan:
        k = str(ch["index"])
        chunks[k] = [book.est_chunk_s(c, 0.5) for c in ch["chunks"]]
        chapters[k] = book.est_chapter_s(ch["chunks"], 0.30, 0.60, 0.5)
    return {
        "chars_per_sec": book.CHARS_PER_SEC,
        "params": {"silence": 0.5, "gap": 0.30, "para_gap": 0.60},
        "chunks": chunks,
        "chapters": chapters,
    }


# ---------------------------------------------------------------- reading log
# Transcribed verbatim from /home/fernando/git/narrator/app/server.py.
def reading_log(pos):
    rows = ["# Reading Log", "",
            "Written by narrator on every pause - last position per book.", "",
            "| book | chapter | chunk | updated |", "|---|---|---|---|"]
    for b, p in sorted(pos.items(), key=lambda kv: kv[1]["updated"], reverse=True):
        rows.append(f"| {Path(b).stem} | {p['chapter']+1}/{p['chapters_total']} "
                    f"{p['chapter_title']} | {p['chunk']+1}/{p['chunks_total']} "
                    f"| {p['updated']} |")
    return "\n".join(rows) + "\n"


READING_LOG_POSITIONS = {
    # Deliberately out of `updated` order in insertion order, so the sort is
    # what produces the output order (newest first).
    "/books/Business/7 Powers (2016).epub": {
        "chapter": 3, "chapters_total": 12,
        "chapter_title": "Counter-Positioning",
        "chunk": 41, "chunks_total": 188,
        "updated": "2026-09-02T08:15:00",
    },
    "/books/Philosophy/Преступление и наказание (1866).epub": {
        "chapter": 0, "chapters_total": 40,
        "chapter_title": "Часть первая — глава I",
        "chunk": 0, "chunks_total": 97,
        "updated": "2026-09-10T23:59:59",
    },
    # A stem carrying a pipe and brackets: markdown table cells are NOT escaped,
    # so the pipe really does break the column layout. Pinned on purpose.
    "/books/Technology/Weird [draft] | v2 (2024).epub": {
        "chapter": 7, "chapters_total": 9,
        "chapter_title": "A | piped ] title",
        "chunk": 2, "chunks_total": 3,
        "updated": "2026-09-08T12:00:00",
    },
}


# ---------------------------------------------------------------- voice notes
# Transcribed verbatim from server.py note_filename(); the NOTES_DIR collision
# check is dropped - the golden always takes the minute-resolution branch.
def note_filename(stamp_minute, stamp_second, text):
    words = re.sub(r"[^\w \-]", "", text, flags=re.UNICODE).lower().split()
    slug = " ".join(words[:6]) or "voice note"
    return f"{stamp_minute} {slug}.md"


def note_markdown(case):
    """Replicates server.py note(): ctx window, deep link, body_md."""
    chunks = case["chunks"]
    i = case["chunk_index"]
    ci = case["chapter_index"]
    title = case["book_title"]
    ctx = " ".join(c["text"] for c in chunks[max(0, i - 1):i + 2]
                   if not c.get("silent"))
    ctitle = case["chapter_title"] or f"Section {ci+1}"
    deep = (f"obsidian://narrator-open?book={quote(Path(case['book_path']).name)}"
            f"&chapter={ci}&chunk={i}")
    return "\n".join([
        "---",
        "type: fleeting",
        "source: voice",
        f"book: \"{title}\"",
        f"chapter: \"{ctitle}\"",
        f"position: chunk {i+1}/{len(chunks)}",
        f"language: {case['language']}",
        f"audio: {case['audio']}",
        f"captured: {case['captured']}",
        "---",
        "",
        f"> [!quote] [{title} — {ctitle}]({deep})",
        f"> {ctx}",
        "",
        case["transcript"],
        "",
        f"[⏮ this passage in narrator]({deep})",
        "",
    ]), ctx, ctitle, deep


NOTE_CASES = [
    {
        "book_title": "7 Powers",
        "book_path": "/books/Business/7 Powers (2016).epub",
        "chapter_index": 3,
        "chapter_title": "Counter-Positioning",
        "chunks": [
            {"text": "The incumbent declines to follow.", "silent": False},
            {"text": "…", "silent": True},
            {"text": "Not because it cannot, but because it will not.", "silent": False},
            {"text": "Collateral damage is the whole point.", "silent": False},
        ],
        "chunk_index": 2,
        "language": "en",
        "audio": "20260910143012.webm",
        "captured": "2026-09-10T14:30:12",
        "transcript": "This is exactly what happened with Netflix, isn't it? "
                      "The DVD business was the moat — and the anchor.",
    },
    {
        # Untitled chapter -> "Section N"; unicode + emoji + punctuation the
        # filename slug regex strips; a book name needing percent-encoding.
        "book_title": "Преступление и наказание",
        "book_path": "/books/Philosophy/Преступление и наказание (1866).epub",
        "chapter_index": 0,
        "chapter_title": "",
        "chunks": [
            {"text": "В начале июля, в чрезвычайно жаркое время…", "silent": False},
            {"text": "“…”", "silent": True},
        ],
        "chunk_index": 0,
        "language": "ru",
        "audio": "20260911090500.ogg",
        "captured": "2026-09-11T09:05:00",
        "transcript": "Отлично! Заметка \U0001f3a7 — guilt-as-a-system, \"not\" remorse; cf. Dr. Smith.",
    },
    {
        # Transcript with no word characters at all -> slug falls back to
        # "voice note". Also: chunk_index past the end of a 1-chunk chapter and
        # a silent neighbour, so the context window is short.
        "book_title": "Fixture",
        "book_path": "/books/Tech/Weird [draft] | v2 (2024).epub",
        "chapter_index": 5,
        "chapter_title": "",
        "chunks": [
            {"text": "…", "silent": True},
            {"text": "“…”", "silent": True},
        ],
        "chunk_index": 1,
        "language": "en",
        "audio": "20260911091500.webm",
        "captured": "2026-09-11T09:15:00",
        "transcript": "?!… \U0001f3a7 —— “”",
    },
]

NOTE_STAMPS = ["202609101430", "202609110905", "202609110915"]


# ---------------------------------------------------------------- real book
def real_book():
    if not REAL_SOURCE.exists():
        raise SystemExit(f"real epub missing: {REAL_SOURCE}")
    digest = hashlib.sha256(REAL_SOURCE.read_bytes()).hexdigest()
    size = REAL_SOURCE.stat().st_size

    with tempfile.TemporaryDirectory(prefix="narrator-golden-real-") as td:
        work = Path(td) / REAL_SOURCE.name
        shutil.copy2(REAL_SOURCE, work)          # never parse from the vault
        chapters = book.extract_chapters(work)
        plan = book.build_plan(chapters, MAX_CHARS)

    copied = None
    if size <= REAL_COPY_MAX_BYTES:
        REAL_DIR.mkdir(parents=True, exist_ok=True)
        dest = REAL_DIR / REAL_SOURCE.name
        shutil.copy2(REAL_SOURCE, dest)
        copied = str(dest.relative_to(REPO))

    counts = {
        "source_name": REAL_SOURCE.name,
        "source_path": str(REAL_SOURCE),
        "bundled_path": copied,          # null -> the Rust test must skip
        "size_bytes": size,
        "sha256": digest,
        "max_chars": MAX_CHARS,
        "chapters": [{
            "index": ch["index"],
            "id": ch["id"],
            "title": ch["title"],
            "n_chunks": len(ch["chunks"]),
            "first_chunk_text": ch["chunks"][0]["text"],
            "last_chunk_text": ch["chunks"][-1]["text"],
            "total_chars": sum(len(c["text"]) for c in ch["chunks"]),
        } for ch in plan],
    }
    head = {
        "source_name": REAL_SOURCE.name,
        "sha256": digest,
        "max_chars": MAX_CHARS,
        "plan": plan[:3],
    }
    return counts, head


# ---------------------------------------------------------------- main
def main():
    FIX.mkdir(parents=True, exist_ok=True)
    written = []

    cases = chunking_cases()
    written.append(write_json("chunking_cases.json", cases))

    chapters = book.extract_chapters(FIXTURE_EPUB)
    plan = book.build_plan(chapters, MAX_CHARS)
    written.append(write_json("fixture_plan.json", plan))
    written.append(write_json("fixture_est.json", est_for_plan(plan)))

    counts, head = real_book()
    written.append(write_json("real_book_counts.json", counts))
    written.append(write_json("real_book_head_plan.json", head))

    written.append(write_json("reading_log.json", READING_LOG_POSITIONS))
    md = reading_log(READING_LOG_POSITIONS)
    (FIX / "reading_log.md").write_text(md, encoding="utf-8")
    written.append(FIX / "reading_log.md")

    notes = []
    for stamp, case in zip(NOTE_STAMPS, NOTE_CASES):
        body, ctx, ctitle, deep = note_markdown(case)
        notes.append({
            "input": dict(case, stamp=stamp),
            "derived": {"context": ctx, "chapter_title_used": ctitle,
                        "deep_link": deep},
            "filename": note_filename(stamp, None, case["transcript"]),
            "markdown": body,
        })
    written.append(write_json("note_markdown.json", notes))

    n_chunks = sum(len(c["chunks"]) for c in plan)
    print(f"chunking_cases      : {len(cases)} cases")
    print(f"fixture_plan        : {len(plan)} chapters, {n_chunks} chunks "
          f"({sum(1 for c in plan for k in c['chunks'] if k['silent'])} silent)")
    print(f"real_book_counts    : {counts['source_name']} -> "
          f"{len(counts['chapters'])} chapters, "
          f"{sum(c['n_chunks'] for c in counts['chapters'])} chunks, "
          f"bundled={counts['bundled_path']}")
    print(f"real_book_head_plan : {len(head['plan'])} chapters, "
          f"{sum(len(c['chunks']) for c in head['plan'])} chunks")
    print(f"reading_log         : {len(READING_LOG_POSITIONS)} books, "
          f"{len(md)} chars")
    print(f"note_markdown       : {len(notes)} notes")
    for p in written:
        print(f"  {p.relative_to(REPO)}  {p.stat().st_size} bytes")


if __name__ == "__main__":
    main()
