#!/usr/bin/env python3
r"""Build the synthetic golden-fixture EPUB.

Writes to a temp dir first, then copies to tests/fixtures/fixture.epub.

The book deliberately exercises every branch of narrator's chunker:
  * several spine items, one of which has no <p>/<h1..3>/<blockquote> at all
    (image-only -> extract_chapters skips it entirely)
  * <blockquote><p>..</p></blockquote> nesting: BeautifulSoup.find_all returns
    BOTH elements, so the text is emitted twice (once as the blockquote's
    flattened get_text, once as the bare <p>). That duplication is real
    narrator behaviour and the Rust port must reproduce it.
  * h1/h2/h3 headings become paragraphs of their own (h4+ are ignored)
  * script/style/nav/header/footer are decomposed before text extraction
  * whitespace soup (newlines, tabs, NBSP) collapsed by re.sub(r"\s+", " ")
  * abbreviations (Dr./Mr./e.g./vs.) hitting the inert-lookbehind regex
  * quote-terminated sentences, an over-long sentence needing clause splitting
  * unspeakable paragraphs (no alnum at all)
  * CJK / Cyrillic / accented unicode
  * an NCX/nav toc that names some chapters and leaves one to fall back to
    paras[0][:60]

Everything lives inside <body>: ebooklib's EpubHtml.get_content() rebuilds each
document from its own template and keeps only the body, so a <head> <script>
would never reach the archive.

NOTE: ebooklib's zip writer is not byte-deterministic across runs (zip member
mtimes come from the current clock). The committed tests/fixtures/fixture.epub
is the artefact of record; re-running this script produces a *semantically*
identical but byte-different file, which would invalidate every fixture JSON
derived from it. Hence it refuses to overwrite unless given --force.
"""
import shutil
import sys
import tempfile
import zipfile
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
OUT = REPO / "tests" / "fixtures" / "fixture.epub"

# A single sentence well over 300 chars, with commas, semicolons and em dashes
# so chunk_paragraph's clause-splitting branch runs.
LONG_SENTENCE = (
    "When the renderer finally reaches a paragraph of this length it must decide, "
    "without any help from the author, where the natural seams are, and so it looks "
    "for commas, for semicolons; for colons: and for em dashes—those being the only "
    "boundaries it trusts—because cutting mid-phrase produces audible glitches, "
    "breath in the wrong place, and a listener who reaches for the pause button "
    "instead of the next chapter, which is the one outcome the whole pipeline exists "
    "to avoid."
)

CH1 = """
<style>p { color: red; }</style>
<script>var swallowed = "this must never be spoken";</script>
<header><p>RUNNING HEADER, STRIPPED</p></header>
<nav><p>Nav paragraph, stripped</p></nav>
<h1>The First Chapter</h1>
<p>This is an ordinary opening paragraph. It has two sentences.</p>
<h2>A Second-Level Heading</h2>
<p>Dr. Smith met Mr. Brown at the lab. They argued, e.g. about tokenizers, vs. about
   money. Ms. Jones and Mrs. Patel and St. Peter and Jr. and Sr. all watched, i.e. nobody
   left, etc. and then it ended.</p>
<h3>A Third-Level Heading</h3>
<h4>A Fourth-Level Heading Which Narrator Never Collects</h4>
<p>He said "go." Then left. She replied 'fine.' Then stayed. A bracketed one ends here.]
   And another (like so.) continues.</p>
<p>Spacing\tis deliberately
   awful   here,
   with newlines and tabs and a non-breaking space.</p>
<footer><p>FOOTER, STRIPPED</p></footer>
"""

CH2 = f"""
<h1>Quotations and Long Breaths</h1>
<blockquote><p>The quoted passage lives inside a blockquote. It is short.</p></blockquote>
<blockquote>
  <p>First quoted paragraph.</p>
  <p>Second quoted paragraph.</p>
</blockquote>
<p>{LONG_SENTENCE}</p>
<p>…</p>
<p>“…”</p>
<p>———</p>
<p>123 456 — 789</p>
"""

CH3 = """
<h1>Многоязычная глава</h1>
<p>Всё смешалось в доме Облонских. Это второе предложение.</p>
<p>彼を知り己を知れば百戦して殆うからず。二つ目の文です。</p>
<p>Café naïve résumé — élève über Sträße. Segunda fração, com acentuação.</p>
<p>中文段落没有西文句号因此不会被切分</p>
"""

# Image-only chapter: no p/h1/h2/h3/blockquote -> paras == [] -> chapter skipped.
CH4 = """
<div><img src="cover.png" alt="a plate"/></div>
<div><img src="cover.png" alt="another plate"/></div>
"""

# Untitled in the TOC -> title falls back to paras[0][:60], a hard 60-char
# truncation of a longer first paragraph.
CH5 = """
<p>This chapter is absent from the table of contents, so narrator falls back to the
   first sixty characters of its first paragraph as the title.</p>
<p>A short trailing paragraph.</p>
"""

# 1x1 transparent PNG.
PNG = bytes.fromhex(
    "89504e470d0a1a0a0000000d4948445200000001000000010806000000"
    "1f15c4890000000a49444154789c6360000002000100"
    "05fe02fa0000000049454e44ae426082"
)

SPECS = [
    ("c1", "chap_01.xhtml", "The First Chapter", CH1),
    ("c2", "chap_02.xhtml", "Quotations and Long Breaths", CH2),
    ("c3", "chap_03.xhtml", "Multilingual", CH3),
    ("c4", "chap_04.xhtml", "Plates Only", CH4),
    ("c5", "chap_05.xhtml", None, CH5),
]


def build(dest: Path) -> Path:
    from ebooklib import epub

    book = epub.EpubBook()
    book.set_identifier("narrator-golden-fixture-0001")
    book.set_title("Narrator Golden Fixture")
    book.set_language("en")
    book.add_author("Fixture Generator")

    book.add_item(epub.EpubItem(uid="cover_img", file_name="cover.png",
                                media_type="image/png", content=PNG))

    items, toc = [], []
    for uid, fname, title, html in SPECS:
        it = epub.EpubHtml(uid=uid, title=title or "", file_name=fname, lang="en")
        # bytes, not str: lxml refuses a unicode string that carries an encoding
        # declaration, and get_content() swallows the exception into b"".
        it.content = html.encode("utf-8")
        book.add_item(it)
        items.append(it)
        if title is not None:
            toc.append(epub.Link(fname, title, uid))

    book.toc = tuple(toc)
    book.spine = items
    book.add_item(epub.EpubNcx())
    book.add_item(epub.EpubNav())

    # epub3_pages makes ebooklib re-parse every document to build a page-list;
    # it chokes on the image-only chapter. narrator never reads the page-list.
    epub.write_epub(str(dest), book, {"epub3_pages": False})
    return dest


def main():
    force = "--force" in sys.argv
    if OUT.exists() and not force:
        print(f"{OUT} already exists ({OUT.stat().st_size} bytes); "
              f"pass --force to regenerate (this changes the committed bytes)")
        return
    with tempfile.TemporaryDirectory(prefix="narrator-golden-") as td:
        tmp = build(Path(td) / "fixture.epub")
        with zipfile.ZipFile(tmp) as z:
            names = z.namelist()
            for _, fname, _, _ in SPECS:
                hit = [n for n in names if n.endswith(fname)]
                assert hit, f"missing {fname}"
                assert z.getinfo(hit[0]).file_size > 0, f"empty {fname}"
        OUT.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(tmp, OUT)
    print(f"wrote {OUT} ({OUT.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
