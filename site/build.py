"""Build the website into _site/: the landing page plus the manual rendered from docs/manual.

    uv run python site/build.py            then open _site/index.html
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
<title>{title} — Orchard Commander Manual</title>
<link rel="icon" href="../icon.png">
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@400;500&display=swap">
<link rel="stylesheet" href="https://unpkg.com/@phosphor-icons/web@2.1.1/src/regular/style.css">
<link rel="stylesheet" href="../nocturne.css">
<link rel="stylesheet" href="../style.css">
</head>
<body>
<div class="page">
<header class="topbar">
  <nav class="topnav">
    <a class="brand" href="../"><img src="../icon.png" alt="" width="26" height="26"><span>Orchard Commander</span></a>
    <a class="navlink" href="../#features">Features</a>
    <a class="navlink" href="./">Manual</a>
    <a class="navlink" href="https://github.com/sandboxly/orchard-commander"><i class="ph ph-github-logo"></i>GitHub</a>
    <a class="btn btn-primary" href="../#download">Download</a>
  </nav>
  <div class="fade-rule"></div>
</header>
<div class="manual">
  <aside><h4>User guide</h4><ol>{toc}</ol></aside>
  <main class="doc">{body}{pager}</main>
</div>
<footer class="footer">
  <span class="footer-brand"><img src="../icon.png" alt="" width="20" height="20">Orchard Commander · MIT License</span>
  <a href="https://github.com/sandboxly/orchard-commander/tree/main/docs/manual">Edit this manual on GitHub</a>
</footer>
</div>
</body>
</html>
"""


def title_of(text: str) -> str:
    match = re.search(r"^# (.+)$", text, re.MULTILINE)
    return match.group(1).strip() if match else "Manual"


def render(text: str) -> str:
    body = markdown.markdown(text, extensions=["tables", "fenced_code", "toc", "sane_lists", "attr_list"])
    return re.sub(r'href="([^":#]+)\.md(#[^"]*)?"', lambda m: f'href="{m.group(1)}.html{m.group(2) or ""}"', body)


def build() -> None:
    shutil.rmtree(OUT, ignore_errors=True)
    shutil.copytree(SITE, OUT, ignore=shutil.ignore_patterns("build.py", "__pycache__"))

    chapters = sorted(p for p in MANUAL.glob("*.md") if p.name != "index.md")
    pages = [MANUAL / "index.md", *chapters]
    titles = {p: title_of(p.read_text(encoding="utf-8")) for p in pages}
    (OUT / "manual").mkdir(exist_ok=True)
    for i, page in enumerate(pages):
        target = "index.html" if page.name == "index.md" else page.with_suffix(".html").name
        toc = "".join(
            f'<li><a href="{"index.html" if p.name == "index.md" else p.with_suffix(".html").name}"'
            f'{" class=current" if p == page else ""}>{html.escape(titles[p])}</a></li>'
            for p in pages
        )
        prev_link = next_link = "<span></span>"
        if i > 0:
            prev = pages[i - 1]
            prev_link = (f'<a href="{"index.html" if prev.name == "index.md" else prev.with_suffix(".html").name}">'
                         f"← {html.escape(titles[prev])}</a>")
        if i + 1 < len(pages):
            nxt = pages[i + 1]
            next_link = f'<a href="{nxt.with_suffix(".html").name}">{html.escape(titles[nxt])} →</a>'
        (OUT / "manual" / target).write_text(PAGE.format(
            title=html.escape(titles[page]), toc=toc, body=render(page.read_text(encoding="utf-8")),
            pager=f'<nav class="pager">{prev_link}{next_link}</nav>',
        ), encoding="utf-8")
    print(f"Built {OUT.relative_to(ROOT)} ({len(pages)} manual pages)")


if __name__ == "__main__":
    build()
    sys.exit(0)
