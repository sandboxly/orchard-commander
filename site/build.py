"""Build the website into _site/: the landing page plus the manual rendered from docs/manual.

    pip install markdown && python site/build.py    then open _site/index.html
"""

from __future__ import annotations

import html
import re
import shutil
import sys
from pathlib import Path

import markdown

ROOT = Path(__file__).resolve().parent.parent
SITE = ROOT / "site"
OUT = ROOT / "_site"
MANUAL = ROOT / "docs" / "manual"

PAGE = """<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} — Orchard Commander manual</title>
<meta name="theme-color" content="#1d2021">
<link rel="icon" href="../icon.png">
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600&family=JetBrains+Mono:wght@400;500;700&display=swap">
<link rel="stylesheet" href="https://unpkg.com/@phosphor-icons/web@2.1.1/src/regular/style.css">
<link rel="stylesheet" href="../style.css">
<script src="../main.js" defer></script>
</head>
<body>
<a class="skip" href="#main">Skip to content</a>

<header class="site-head">
  <div class="wrap">
    <a class="brand" href="../"><img src="../icon.png" alt="">Orchard Commander</a>
    <nav class="site-nav" aria-label="Main">
      <a href="../#features">Features</a>
      <a href="./" aria-current="page">Manual</a>
      <a href="https://github.com/sandboxly/orchard-commander"><i class="ph ph-github-logo" aria-hidden="true"></i><span class="gh-label">GitHub</span></a>
      <a href="../#download">Download</a>
    </nav>
  </div>
</header>

<div class="wrap doc">
  <nav class="toc frame" aria-label="Chapters">
    <span class="chip">Manual</span>
    <details open>
      <summary>{summary} <i class="ph ph-caret-down" aria-hidden="true"></i></summary>
      <ol>
{toc}
      </ol>
    </details>
  </nav>

  <main id="main" class="prose">
    <p class="kicker">{kicker}</p>
{body}
    <nav class="pager" aria-label="Chapter navigation">
{pager}
    </nav>
  </main>
</div>

<footer class="site-foot">
  <div class="wrap">
    <span><img src="../icon.png" alt="" width="18" height="18">Orchard Commander · MIT License</span>
    <a href="https://github.com/sandboxly/orchard-commander/tree/main/docs/manual">Edit this manual on GitHub</a>
    <a href="https://github.com/sandboxly/orchard-commander/issues">Report a problem</a>
  </div>
</footer>
</body>
</html>
"""

CODE_LABELS = {"sh": "shell", "bash": "shell", "console": "shell", "toml": "toml", "": "code"}
NOTES = {"Note": ("ph-info", ""), "Tip": ("ph-lightbulb", ""), "Important": ("ph-warning", " warn"),
         "Warning": ("ph-warning", " warn"), "Caution": ("ph-warning", " warn")}


def title_of(text: str) -> str:
    match = re.search(r"^# (.+)$", text, re.MULTILINE)
    return match.group(1).strip() if match else "Manual"


def html_name(page: Path) -> str:
    return "index.html" if page.name == "index.md" else page.with_suffix(".html").name


def render(text: str) -> str:
    body = markdown.markdown(text, extensions=["tables", "fenced_code", "toc", "sane_lists", "attr_list"])
    # Links between chapters point at the rendered pages.
    body = re.sub(r'href="([^":#]+)\.md(#[^"]*)?"', lambda m: f'href="{m.group(1)}.html{m.group(2) or ""}"', body)
    # The design's blocks: framed code with a language chip, scrollable tables, note boxes.
    body = re.sub(
        r'<pre><code(?: class="language-(\w+)")?>',
        lambda m: f'<div class="frame codeblock on-bg"><span class="chip">{CODE_LABELS.get(m.group(1) or "", m.group(1))}'
                  "</span><pre><code>",
        body,
    )
    body = body.replace("</code></pre>", "</code></pre></div>")
    body = body.replace("<table>", '<div class="table-wrap"><table>').replace("</table>", "</table></div>")

    def note(m: re.Match) -> str:
        icon, extra = NOTES[m.group(1)]
        return (f'<div class="frame note on-bg{extra}"><span class="chip"><i class="ph {icon}" aria-hidden="true">'
                f"</i>{m.group(1)}</span><p>{m.group(2)}</p></div>")

    body = re.sub(r"<p>(?:<strong>)?(" + "|".join(NOTES) + r"):(?:</strong>)? (.*?)</p>", note, body, flags=re.S)
    return body


def build() -> None:
    shutil.rmtree(OUT, ignore_errors=True)
    shutil.copytree(SITE, OUT, ignore=shutil.ignore_patterns("build.py", "__pycache__"))

    chapters = sorted(p for p in MANUAL.glob("*.md") if p.name != "index.md")
    pages = [MANUAL / "index.md", *chapters]
    titles = {p: title_of(p.read_text(encoding="utf-8")) for p in pages}
    (OUT / "manual").mkdir(exist_ok=True)
    for i, page in enumerate(pages):
        number = chapters.index(page) + 1 if page in chapters else None
        current = ' aria-current="page"'
        toc = "\n".join(
            f'        <li><a href="{html_name(c)}"{current if c == page else ""}>{html.escape(titles[c])}</a></li>'
            for c in chapters
        )
        links = []
        if i > 0:
            prev = pages[i - 1]
            links.append(f'      <a class="prev" href="{html_name(prev)}"><span>← Previous</span>'
                         f"{html.escape(titles[prev])}</a>")
        if i + 1 < len(pages):
            nxt = pages[i + 1]
            links.append(f'      <a class="next" href="{html_name(nxt)}"><span>Next →</span>{html.escape(titles[nxt])}</a>')
        (OUT / "manual" / html_name(page)).write_text(PAGE.format(
            title=html.escape(titles[page]),
            kicker=f"Chapter {number:02d}" if number else "User guide",
            summary=html.escape(f"Chapter {number} · {titles[page]}" if number else "Chapters"),
            toc=toc,
            body=render(page.read_text(encoding="utf-8")),
            pager="\n".join(links),
        ), encoding="utf-8")
    print(f"Built {OUT.relative_to(ROOT)} ({len(pages)} manual pages)")


if __name__ == "__main__":
    build()
    sys.exit(0)
