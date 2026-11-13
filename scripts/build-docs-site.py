#!/usr/bin/env python3
"""Build the HTML documentation site from the shipped Markdown (DOC-16).

    scripts/build-docs-site.py                      # target/docs-site/, one file per page
    scripts/build-docs-site.py --out DIR            # somewhere else
    scripts/build-docs-site.py --single FILE        # everything in one HTML file
    scripts/build-docs-site.py --single FILE --fragment
                                                    # the same, without the document shell,
                                                    # for hosts that supply their own

The Markdown under docs/ is the source; this only renders it. Every page
opens straight from disk (no server, no fetch), search included, and the
build fails on any link or anchor that points nowhere — the same slug rules
scripts/check-docs.py holds the Markdown to, so a link that passes there
lands here.

Standard library only: it runs wherever CI does.
"""

from __future__ import annotations

import base64
import html
import importlib.util
import json
import mimetypes
import os
import re
import shutil
import sys
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "scripts" / "docs-site"
REPO_URL = "https://github.com/neuralpunk/meshware/blob/main/"

_spec = importlib.util.spec_from_file_location("check_docs", ROOT / "scripts" / "check-docs.py")
_check = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_check)
slug = _check.slug  # one set of anchor rules for the Markdown and the site

# ---------------------------------------------------------------------------
# What the site holds, in order. (source, title override or None)
# ---------------------------------------------------------------------------

NAV = [
    ("Get started", [
        ("docs/introduction.md", "What Meshware is"),
        ("docs/quickstart.md", "Quickstart"),
    ]),
    ("Use Meshware", [
        ("docs/guides/running-work.md", "Running work"),
        ("docs/guides/machines.md", "Machines"),
        ("docs/deployments.md", "Deployments"),
        ("docs/guides/people-and-access.md", "People and access"),
        ("docs/guides/secrets.md", "Secrets"),
        ("docs/director.md", "Director"),
    ]),
    ("Operate", [
        ("docs/operating/README.md", "Overview"),
        ("docs/operating/install.md", "Installing"),
        ("docs/operating/network.md", "Network"),
        ("docs/operating/security.md", "Security"),
        ("docs/operating/identity.md", "People, machines and roles"),
        ("docs/operating/upgrade.md", "Upgrading"),
        ("docs/operating/compatibility.md", "Compatibility"),
        ("docs/operating/backup.md", "Backup and restore"),
        ("docs/operating/runtimes.md", "Runtimes"),
        ("docs/operating/air-gap.md", "Air-gapped meshes"),
        ("docs/operating/troubleshooting.md", "Troubleshooting"),
        ("docs/operating/platforms.md", "Platforms"),
        ("docs/operating/limits.md", "Limits"),
        ("docs/operating/legacy.md", "Removed features"),
        ("docs/operating/uninstall.md", "Removing Meshware"),
    ]),
    ("Reference", [
        ("docs/reference/README.md", "Overview"),
        ("docs/reference/config.md", "Configuration"),
        ("docs/reference/environment.md", "Environment variables"),
        ("docs/reference/metrics.md", "Metrics"),
        ("docs/reference/deployment-file.md", "Deployment file"),
        ("docs/reference/schedule-file.md", "Schedule file"),
        ("docs/reference/script-definitions.md", "Script definitions"),
        ("docs/reference/plugins.md", "Plugin contracts"),
    ]),
    ("Background", [
        ("docs/no-control-plane.md", "No control plane"),
        ("docs/operational-cost.md", "Operational cost"),
        ("docs/whose-fleet-is-it.md", "Whose fleet is it"),
        ("docs/sending-history-anywhere.md", "Sending history anywhere"),
        ("docs/querying-the-archive.md", "Querying the archive"),
    ]),
    ("Project", [
        ("CHANGELOG.md", "Changelog"),
        ("SECURITY.md", "Security policy"),
    ]),
]

# The home page's tiles: (source, label, blurb). The first is the wide one.
TILES = [
    ("docs/quickstart.md", "Get started",
     "From download to two machines running your commands, step by step, in about fifteen minutes."),
    ("docs/introduction.md", "Start here", "What Meshware is, and the words you will meet."),
    ("docs/deployments.md", "Keep services up",
     "Describe a service once; Meshware keeps the copies running, rolls out changes and moves them off failing machines."),
    ("docs/guides/people-and-access.md", "Let people in",
     "Keys instead of passwords, five roles, grants for some machines or for a while, and the audit log."),
    ("docs/guides/running-work.md", "Run things", "One command on one machine, on every GPU machine, or on all of them."),
    ("docs/guides/secrets.md", "Keep secrets", "Passwords and tokens sealed until the job that needs them starts."),
    ("docs/director.md", "Use the app", "Every screen of Director, and the command that does the same."),
    ("docs/operating/troubleshooting.md", "Fix problems",
     "The errors people meet in the first hour, by the words on the screen, and what to do."),
]

# Repository files a page may link to that are not pages here. Anything
# else outside the site is an error, so a new link is a decision.
REPO_FILES_OK = ("LICENSE", "README.md", "Cargo.toml", "examples/", "packaging/", "scripts/", "crates/", "sdk/", "apps/", "deny.toml")

LOGO = (
    '<svg viewBox="0 0 100 100" aria-hidden="true"><g transform="translate(50 50) scale(1.15) translate(-50 -50)">'
    '<polygon points="50,22 84.6,42 50,62 15.4,42" fill="#12857a"/>'
    '<polygon points="15.4,42 50,62 50,78 15.4,58" fill="#0b524b"/>'
    '<polygon points="50,62 84.6,42 84.6,58 50,78" fill="#083d38"/>'
    '<circle cx="24" cy="55" r="3" fill="#f0b254"/></g></svg>'
)
BRACKET = '<svg viewBox="0 0 24 80" aria-hidden="true"><path fill="currentColor" d="M13 80h11v-8.5l-11-7v-49L24 9V0H13L0 8v63.5z"/></svg>'
ICON_SEARCH = '<svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true"><circle cx="8.5" cy="8.5" r="5.5"/><path d="m13 13 4.5 4.5"/></svg>'
ICON_THEME = '<svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true"><circle cx="10" cy="10" r="7"/><path d="M10 3v14a7 7 0 0 0 0-14z" fill="currentColor"/></svg>'
ICON_MENU = '<svg viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true"><path d="M3 6h14M3 10h14M3 14h14"/></svg>'
FAVICON = (
    "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'%3E%3Crect width='100' "
    "height='100' rx='22' fill='%2312857a'/%3E%3Cg transform='translate(50 50) scale(1.15) translate(-50 -50)'%3E"
    "%3Cpolygon points='50,22 84.6,42 50,62 15.4,42' fill='%23fff'/%3E%3Cpolygon points='15.4,42 50,62 50,78 15.4,58' "
    "fill='%23a5d0cc'/%3E%3Cpolygon points='50,62 84.6,42 84.6,58 50,78' fill='%236cb3ad'/%3E%3Ccircle cx='24' "
    "cy='55' r='3' fill='%23f0b254'/%3E%3C/g%3E%3C/svg%3E"
)
FONTS = ("https://fonts.googleapis.com/css2?family=Instrument+Sans:wght@400;500;600;700"
         "&family=JetBrains+Mono:wght@400;500&display=swap")


# ---------------------------------------------------------------------------
# Pages
# ---------------------------------------------------------------------------

@dataclass
class Page:
    src: str            # repo-relative Markdown path
    url: str            # site-relative output path
    group: str
    title: str
    slug: str           # the single-file build's page id
    text: str = ""
    body: str = ""
    toc: list = field(default_factory=list)     # (level, id, text)
    ids: set = field(default_factory=set)
    sections: list = field(default_factory=list)  # search entries


def url_for(src: str) -> str:
    if src == "CHANGELOG.md":
        return "changelog.html"
    if src == "SECURITY.md":
        return "security-policy.html"
    rel = src[len("docs/"):] if src.startswith("docs/") else src
    if rel.endswith("README.md"):
        return rel[: -len("README.md")] + "index.html"
    return rel[:-3] + ".html"


def slug_for(url: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", url[:-5].lower()).strip("-")


def load_pages() -> list[Page]:
    pages = []
    for group, items in NAV:
        for src, title in items:
            path = ROOT / src
            if not path.exists():
                raise SystemExit(f"NAV names {src}, which does not exist")
            text = path.read_text()
            h1 = re.search(r"^#\s+(.+?)\s*$", text, re.M)
            url = url_for(src)
            pages.append(Page(src, url, group, title or (h1.group(1) if h1 else src), slug_for(url), text))
    return pages


# ---------------------------------------------------------------------------
# Markdown, the subset these docs use
# ---------------------------------------------------------------------------

LIST_ITEM = re.compile(r"^(\s*)([-*+]|\d{1,3}[.)])\s+(.*)$")
FENCE_OPEN = re.compile(r"^(\s*)(`{3,}|~{3,})\s*([\w+-]*)\s*$")
TABLE_SEP = re.compile(r"^\s*\|?\s*:?-{2,}:?\s*(\|\s*:?-{2,}:?\s*)*\|?\s*$")
REF_DEF = re.compile(r"^\s{0,3}\[([^\]]+)\]:\s*(\S+)(?:\s+\"[^\"]*\")?\s*$")
HR = re.compile(r"^\s{0,3}([-*_])(\s*\1){2,}\s*$")
HTML_BLOCK = re.compile(r"^\s{0,3}<(a\s|/?div|/?details|/?summary|!--|br\s*/?>|img\s|p\s|/?table)", re.I)
ALERT = re.compile(r"^\[!(NOTE|TIP|IMPORTANT|WARNING|CAUTION)\]\s*$", re.I)


class Renderer:
    """One page's Markdown to HTML, with its links resolved by `link`."""

    def __init__(self, page: Page, link, image, prefix: str = ""):
        self.page = page
        self.prefix = prefix      # the single-file build prefixes every id with its page
        self.link = link          # (href) -> href, raising on a dead link
        self.image = image        # (src) -> src
        self.refs: dict[str, str] = {}
        self.seen: dict[str, int] = {}
        self.toc: list = []
        self.ids: set = set()
        self.sections: list = []

    # -- inline ------------------------------------------------------------
    def inline(self, text: str) -> str:
        stash: list[str] = []

        def keep(s: str) -> str:
            stash.append(s)
            return f"\x00{len(stash) - 1}\x00"

        # Code spans first: nothing inside them is markup.
        def code(m):
            body = m.group(2)
            if body.startswith(" ") and body.endswith(" ") and body.strip():
                body = body[1:-1]
            return keep(f"<code>{html.escape(body, quote=False)}</code>")
        text = re.sub(r"(`+)(.+?)(?<!`)\1(?!`)", code, text)
        # Escapes.
        text = re.sub(r"\\([\\`*_{}\[\]()#+\-.!|<>~])", lambda m: keep(html.escape(m.group(1))), text)
        # Autolinks.
        text = re.sub(r"<(https?://[^>\s]+)>", lambda m: keep(
            f'<a href="{html.escape(m.group(1))}">{html.escape(m.group(1))}</a>'), text)
        text = html.escape(text, quote=False)
        # Images, then links (whose text may itself be inline markup).
        text = re.sub(r"!\[([^\]]*)\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)",
                      lambda m: keep(f'<img src="{html.escape(self.image(html.unescape(m.group(2))))}" '
                                     f'alt="{m.group(1)}" loading="lazy">'), text)

        def a(label: str, href: str) -> str:
            target = self.link(html.unescape(href))
            ext = target.startswith(("http://", "https://"))
            rel = ' rel="noopener"' if ext else ""
            return keep(f'<a href="{html.escape(target)}"{rel}>') + label + keep("</a>")
        text = re.sub(r"\[((?:[^\[\]]|\[[^\]]*\])*)\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)",
                      lambda m: a(m.group(1), m.group(2)), text)

        def ref(m):
            label, key = m.group(1), (m.group(2) or m.group(1))
            href = self.refs.get(key.lower())
            if href is None:
                return m.group(0)
            return a(label, href)
        text = re.sub(r"\[((?:[^\[\]]|\[[^\]]*\])*)\]\[([^\]]*)\]", ref, text)
        # Emphasis.
        text = re.sub(r"\*\*(?=\S)(.+?)(?<=\S)\*\*", r"<strong>\1</strong>", text)
        text = re.sub(r"__(?=\S)(.+?)(?<=\S)__", r"<strong>\1</strong>", text)
        text = re.sub(r"(?<![\w*])\*(?=[^\s*])(.+?)(?<=[^\s*])\*(?![\w*])", r"<em>\1</em>", text)
        text = re.sub(r"(?<![\w_])_(?=[^\s_])(.+?)(?<=[^\s_])_(?![\w_])", r"<em>\1</em>", text)
        # Hard breaks are not used; a trailing double space is just space.
        while "\x00" in text:
            text = re.sub(r"\x00(\d+)\x00", lambda m: stash[int(m.group(1))], text)
        return text

    # -- blocks ------------------------------------------------------------
    def render(self, text: str) -> str:
        lines = text.split("\n")
        for line in lines:
            m = REF_DEF.match(line)
            if m:
                self.refs[m.group(1).lower()] = m.group(2)
        return self.blocks(lines, top=True)

    def heading(self, level: int, raw: str, top: bool) -> str:
        base = slug(raw)
        n = self.seen.get(base, 0)
        self.seen[base] = n + 1
        hid = base if n == 0 else f"{base}-{n}"
        self.ids.add(hid)
        inner = self.inline(raw)
        plain = re.sub(r"<[^>]+>", "", inner)
        if level == 1 and top:
            return ""  # the page template sets the title
        if top and level in (2, 3):
            self.toc.append((level, hid, html.unescape(plain)))
        if top and level in (2, 3):
            self.sections.append({"h": html.unescape(plain), "id": hid, "x": []})
        full = self.prefix + hid
        anchor = f'<a class="anchor" href="#{full}" aria-label="Link to this section">#</a>'
        return f'<h{level} id="{full}">{anchor}{inner}</h{level}>'

    def blocks(self, lines: list[str], top: bool = False, tight: bool = False) -> str:
        out: list[str] = []
        i, n = 0, len(lines)

        def starts_block(line: str) -> bool:
            return bool(FENCE_OPEN.match(line) or re.match(r"^#{1,6}\s", line) or line.lstrip().startswith(">")
                        or HR.match(line) or re.match(r"^\s{0,3}([-*+]|\d{1,3}[.)])\s+\S", line))

        while i < n:
            line = lines[i]
            if not line.strip():
                i += 1
                continue
            m = FENCE_OPEN.match(line)
            if m:
                indent, fence, lang = len(m.group(1)), m.group(2), m.group(3).lower()
                body = []
                i += 1
                while i < n and not re.match(r"^\s*" + re.escape(fence[0]) + "{" + str(len(fence)) + r",}\s*$", lines[i]):
                    body.append(lines[i][indent:] if lines[i][:indent].strip() == "" else lines[i].lstrip())
                    i += 1
                i += 1
                out.append(self.code_block("\n".join(body), lang))
                self.note("\n".join(body))
                continue
            m = re.match(r"^(#{1,6})\s+(.*?)\s*#*\s*$", line)
            if m:
                out.append(self.heading(len(m.group(1)), m.group(2), top))
                i += 1
                continue
            if HR.match(line):
                out.append('<hr class="pattern">')
                i += 1
                continue
            if REF_DEF.match(line):
                i += 1
                continue
            if line.lstrip().startswith(">"):
                quote = []
                while i < n and lines[i].lstrip().startswith(">"):
                    quote.append(re.sub(r"^\s*> ?", "", lines[i]))
                    i += 1
                out.append(self.blockquote(quote))
                continue
            if "|" in line and i + 1 < n and TABLE_SEP.match(lines[i + 1]):
                rows = [line]
                i += 2
                while i < n and lines[i].strip() and "|" in lines[i]:
                    rows.append(lines[i])
                    i += 1
                out.append(self.table(rows))
                continue
            m = LIST_ITEM.match(line)
            if m and len(m.group(1)) <= 3:
                html_list, i = self.list_block(lines, i)
                out.append(html_list)
                continue
            if HTML_BLOCK.match(line):
                raw = []
                while i < n and lines[i].strip():
                    raw.append(lines[i])
                    i += 1
                chunk = "\n".join(raw)
                # An explicit anchor a heading already carries would be a
                # second element with one id (config.md has `## [network]`
                # and `<a id="network">`): keep the heading's.
                chunk = re.sub(r'<a id="([^"]+)"></a>',
                               lambda m: "" if m.group(1) in self.ids else m.group(0), chunk)
                self.ids |= set(re.findall(r'<a id="([^"]+)"', chunk))
                if chunk.strip():
                    out.append(chunk.replace('<a id="', f'<a id="{self.prefix}'))
                continue
            para = [line.strip()]
            i += 1
            while i < n and lines[i].strip() and not starts_block(lines[i]) and not (
                    "|" in lines[i] and i + 1 < n and TABLE_SEP.match(lines[i + 1])):
                para.append(lines[i].strip())
                i += 1
            text = " ".join(para)
            self.note(text)
            img = re.fullmatch(r"!\[([^\]]*)\]\(([^)\s]+)\)", text)
            if img:
                src = html.escape(self.image(img.group(2)))
                alt = html.escape(img.group(1))
                out.append(f'<figure><img src="{src}" alt="{alt}" loading="lazy">'
                           f"<figcaption>{alt}</figcaption></figure>")
            elif tight:
                out.append(self.inline(text))
            else:
                out.append(f"<p>{self.inline(text)}</p>")
        return "\n".join(out)

    def note(self, text: str) -> None:
        """Feed a section's text to the search index."""
        if self.sections:
            self.sections[-1]["x"].append(text)
        else:
            self.sections.append({"h": "", "id": "", "x": [text]})

    def blockquote(self, lines: list[str]) -> str:
        kind = None
        if lines and ALERT.match(lines[0].strip()):
            kind = ALERT.match(lines[0].strip()).group(1).lower()
            lines = lines[1:]
        inner = self.blocks(lines)
        if kind is None:
            return f'<blockquote class="callout is-quote">{inner}</blockquote>'
        cls = {"warning": "is-warning", "caution": "is-caution"}.get(kind, "")
        label = kind.capitalize()
        return (f'<aside class="callout {cls}" role="note"><div class="callout__label">{label}</div>'
                f"{inner}</aside>")

    def table(self, rows: list[str]) -> str:
        def cells(row: str) -> list[str]:
            row = row.strip()
            if row.startswith("|"):
                row = row[1:]
            if row.endswith("|") and not row.endswith("\\|"):
                row = row[:-1]
            parts, cur, in_code = [], "", False
            j = 0
            while j < len(row):
                ch = row[j]
                if ch == "\\" and j + 1 < len(row) and row[j + 1] == "|":
                    cur += "\\|"
                    j += 2
                    continue
                if ch == "`":
                    in_code = not in_code
                if ch == "|" and not in_code:
                    parts.append(cur)
                    cur = ""
                else:
                    cur += ch
                j += 1
            parts.append(cur)
            return [p.strip().replace("\\|", "|") for p in parts]

        head = cells(rows[0])
        body = [cells(r) for r in rows[1:]]
        self.note(" ".join(" ".join(r) for r in [head] + body))
        th = "".join(f"<th>{self.inline(c)}</th>" for c in head)
        trs = []
        for r in body:
            r = (r + [""] * len(head))[: len(head)]
            trs.append("<tr>" + "".join(f"<td>{self.inline(c)}</td>" for c in r) + "</tr>")
        wide = " is-wide" if len(head) > 3 else ""
        return (f'<div class="table{wide}" tabindex="0"><table><thead><tr>{th}</tr></thead>'
                f"<tbody>{''.join(trs)}</tbody></table></div>")

    def list_block(self, lines: list[str], i: int) -> tuple[str, int]:
        first = LIST_ITEM.match(lines[i])
        base = len(first.group(1))
        ordered = first.group(2)[0].isdigit()
        start = int(re.match(r"\d+", first.group(2)).group(0)) if ordered else 1
        items: list[list[str]] = []
        loose = False
        n = len(lines)
        while i < n:
            m = LIST_ITEM.match(lines[i])
            if not m or len(m.group(1)) != base or m.group(2)[0].isdigit() != ordered:
                break
            content = len(m.group(1)) + len(m.group(2)) + 1
            item = [m.group(3)]
            i += 1
            while i < n:
                line = lines[i]
                if not line.strip():
                    j = i + 1
                    while j < n and not lines[j].strip():
                        j += 1
                    if j < n and (len(lines[j]) - len(lines[j].lstrip())) >= content:
                        item.append("")
                        loose = True
                        i += 1
                        continue
                    nxt = LIST_ITEM.match(lines[j]) if j < n else None
                    if nxt and len(nxt.group(1)) == base and nxt.group(2)[0].isdigit() == ordered:
                        loose = True
                    break
                ind = len(line) - len(line.lstrip())
                if ind >= content:
                    item.append(line[content:])
                    i += 1
                    continue
                m2 = LIST_ITEM.match(line)
                if m2 and len(m2.group(1)) <= base:
                    break
                if m2:  # a nested list indented less than the content column
                    item.append(line[base + 2:] if ind >= base + 2 else line.lstrip())
                    i += 1
                    continue
                if FENCE_OPEN.match(line) or line.lstrip().startswith(">") or re.match(r"^#{1,6}\s", line):
                    break
                item.append(line.strip())  # lazy continuation
                i += 1
            while i < n and not lines[i].strip():
                j = i
                while j < n and not lines[j].strip():
                    j += 1
                nxt = LIST_ITEM.match(lines[j]) if j < n else None
                if nxt and len(nxt.group(1)) == base and nxt.group(2)[0].isdigit() == ordered:
                    i = j
                    break
                break
            items.append(item)
        lis = []
        for item in items:
            lis.append(f"<li>{self.blocks(item, tight=not loose)}</li>")
        tag = "ol" if ordered else "ul"
        attr = f' start="{start}"' if ordered and start != 1 else ""
        return f"<{tag}{attr}>{''.join(lis)}</{tag}>", i

    # -- code --------------------------------------------------------------
    def code_block(self, body: str, lang: str) -> str:
        lang = {"sh": "shell", "bash": "shell", "shell-session": "console", "yml": "yaml", "": "text"}.get(lang, lang)
        highlighted = highlight(body, lang)
        label = {"console": "Terminal", "text": "Text"}.get(lang, lang.upper() if len(lang) <= 4 else lang.capitalize())
        return (f'<div class="code" data-lang="{html.escape(lang)}"><div class="code__bar"><span class="code__lang">'
                f'{html.escape(label)}</span><button class="code__copy" type="button">Copy</button></div>'
                f"<pre><code>{highlighted}</code></pre></div>")


def highlight(body: str, lang: str) -> str:
    """A light touch: prompts, comments, keys and strings. Enough to read by."""
    esc = lambda s: html.escape(s, quote=False)
    out = []
    for line in body.split("\n"):
        if lang == "console":
            m = re.match(r"^(\$|#) (.*)$", line)
            if m and m.group(1) == "$":
                cmd, comment = split_comment(m.group(2))
                out.append(f'<span class="tok-p">$ </span>{esc(cmd)}' +
                           (f'<span class="tok-c">{esc(comment)}</span>' if comment else ""))
            elif line.startswith("# "):
                out.append(f'<span class="tok-c">{esc(line)}</span>')
            else:
                out.append(f'<span class="tok-o">{esc(line)}</span>' if line else "")
        elif lang in ("toml", "yaml", "ini", "shell", "python", "sql", "json", "hcl"):
            cmt = "--" if lang == "sql" else "#"
            stripped = line.lstrip()
            if lang != "json" and stripped.startswith(cmt):
                out.append(f'<span class="tok-c">{esc(line)}</span>')
                continue
            code, comment = split_comment(line, cmt) if lang != "json" else (line, "")
            code_html = esc(code)
            code_html = re.sub(r"(&quot;|\")(?:[^\"&]|&(?!quot;))*?(&quot;|\")|'[^']*'",
                               lambda m: f'<span class="tok-s">{m.group(0)}</span>', code_html)
            if lang in ("toml", "ini", "hcl"):
                code_html = re.sub(r"^(\s*)(\[\[?[^\]]+\]\]?|[\w.\-\"]+)(?=\s*=|\s*$)",
                                   lambda m: f'{m.group(1)}<span class="tok-k">{m.group(2)}</span>'
                                   if m.group(2).startswith("[") or "=" in code else m.group(0), code_html)
            elif lang == "yaml":
                code_html = re.sub(r"^(\s*-?\s*)([\w.\-]+)(?=:)",
                                   lambda m: f'{m.group(1)}<span class="tok-k">{m.group(2)}</span>', code_html)
            elif lang == "json":
                code_html = re.sub(r'<span class="tok-s">(&quot;[^&]*&quot;)</span>(\s*:)',
                                   r'<span class="tok-k">\1</span>\2', code_html)
            out.append(code_html + (f'<span class="tok-c">{esc(comment)}</span>' if comment else ""))
        else:
            out.append(esc(line))
    return "\n".join(out)


def split_comment(line: str, marker: str = "#") -> tuple[str, str]:
    """Split a trailing comment off, ignoring markers inside quotes."""
    quote = None
    for j, ch in enumerate(line):
        if quote:
            if ch == quote:
                quote = None
            continue
        if ch in "\"'":
            quote = ch
            continue
        if line.startswith(marker, j) and (j == 0 or line[j - 1] in " \t"):
            k = j
            while k > 0 and line[k - 1] in " \t":
                k -= 1
            return line[:k], line[k:]
    return line, ""


# ---------------------------------------------------------------------------
# Links
# ---------------------------------------------------------------------------

class Site:
    def __init__(self, pages: list[Page], single: bool, out: Path | None):
        self.pages = pages
        self.by_src = {p.src: p for p in pages}
        self.single = single
        self.out = out
        self.errors: list[str] = []
        self.pending: list[tuple[Page, str, str]] = []  # (from page, target page src, anchor)
        self.images: dict[str, str] = {}

    def resolve(self, page: Page, href: str) -> str:
        if re.match(r"^[a-z][a-z0-9+.-]*:", href, re.I):
            return href
        path, _, anchor = href.partition("#")
        if not path:
            target = page
        else:
            repo = os.path.normpath(os.path.join(os.path.dirname(page.src), path))
            if (ROOT / repo).is_dir():
                repo = os.path.join(repo, "README.md")
            target = self.by_src.get(repo)
            if target is None:
                if repo.startswith("..") or not (ROOT / repo).exists():
                    self.errors.append(f"{page.src}: {href} — no such file")
                    return "#"
                if not repo.startswith(REPO_FILES_OK):
                    self.errors.append(f"{page.src}: {href} — {repo} is not a page on the site")
                    return "#"
                return REPO_URL + repo + (f"#{anchor}" if anchor else "")
        if anchor:
            self.pending.append((page, target.src, anchor))
        return self.href(page, target, anchor)

    def href(self, page: Page, target: Page | None, anchor: str = "", home: bool = False) -> str:
        if self.single:
            if home:
                return "#p-home"
            return f"#{target.slug}--{anchor}" if anchor else f"#p-{target.slug}"
        dest = "index.html" if home else target.url
        rel = os.path.relpath(dest, os.path.dirname(page.url) or ".") if page else dest
        return rel + (f"#{anchor}" if anchor else "")

    def image(self, page: Page, src: str) -> str:
        repo = os.path.normpath(os.path.join(os.path.dirname(page.src), src))
        path = ROOT / repo
        if not path.is_file():
            self.errors.append(f"{page.src}: image {src} — no such file")
            return ""
        if self.single:
            mime = mimetypes.guess_type(path.name)[0] or ("image/webp" if path.suffix == ".webp" else "application/octet-stream")
            return f"data:{mime};base64,{base64.b64encode(path.read_bytes()).decode()}"
        rel_asset = "assets/" + repo.replace("docs/", "", 1)
        self.images[rel_asset] = repo
        return os.path.relpath(rel_asset, os.path.dirname(page.url) or ".")

    def check_anchors(self) -> None:
        for page, target, anchor in self.pending:
            if anchor not in self.by_src[target].ids:
                self.errors.append(f"{page.src}: #{anchor} — no such heading in {target}")


# ---------------------------------------------------------------------------
# Assembly
# ---------------------------------------------------------------------------

def neighbours(pages: list[Page], i: int) -> tuple[Page | None, Page | None]:
    return (pages[i - 1] if i > 0 else None), (pages[i + 1] if i + 1 < len(pages) else None)


def sidebar(site: Site, current: Page | None) -> str:
    parts = ['<nav class="side" id="side" aria-label="Documentation">']
    for group, _ in NAV:
        members = [p for p in site.pages if p.group == group]
        cur = " is-current" if current and current.group == group else ""
        parts.append(f'<div class="side__group{cur}"><div class="side__label">{html.escape(group)}</div><ul>')
        for p in members:
            here = ' aria-current="page"' if current is p else ""
            parts.append(f'<li><a href="{html.escape(site.href(current, p))}" data-slug="{p.slug}"{here}>'
                         f"{html.escape(p.title)}</a></li>")
        parts.append("</ul></div>")
    parts.append("</nav>")
    return "".join(parts)


def topbar(site: Site, current: Page | None) -> str:
    home = html.escape(site.href(current, None, home=True))
    return (
        '<header class="top">'
        f'<button class="top__menu" id="menuToggle" type="button" aria-label="Show the contents" '
        f'aria-controls="side" aria-expanded="false">{ICON_MENU}</button>'
        f'<a class="top__brand" href="{home}">{LOGO}<span>Meshware</span><small>/ Docs</small></a>'
        f'<div class="top__search">{search_box("topSearch")}</div>'
        f'<div class="top__tools"><button class="top__tool" id="themeToggle" type="button">{ICON_THEME}</button></div>'
        "</header>"
    )


def search_box(ident: str, big: bool = False) -> str:
    return (
        f'<div class="search" data-search><label class="search__box" for="{ident}">{ICON_SEARCH}'
        f'<span class="sr">Search the documentation</span>'
        f'<input class="search__input" id="{ident}" type="search" autocomplete="off" spellcheck="false" '
        f'placeholder="{"Search the documentation" if big else "Search"}">'
        '<kbd class="search__key">/</kbd></label>'
        '<div class="search__results" hidden></div></div>'
    )


def eyebrow(text: str) -> str:
    return f'<div class="eyebrow">{BRACKET}<span>{html.escape(text)}</span><svg class="r" viewBox="0 0 24 80" aria-hidden="true"><path fill="currentColor" d="M13 80h11v-8.5l-11-7v-49L24 9V0H13L0 8v63.5z"/></svg></div>'


def article(site: Site, page: Page, i: int) -> str:
    prev, nxt = neighbours(site.pages, i)
    home = html.escape(site.href(page, None, home=True))
    crumbs = (f'<nav class="crumbs" aria-label="Breadcrumb"><a href="{home}">Docs</a><span aria-hidden="true">/</span>'
              f'<span>{html.escape(page.group)}</span><span aria-hidden="true">/</span>'
              f'<span aria-current="page">{html.escape(page.title)}</span></nav>')
    pager = ['<nav class="pager" aria-label="Previous and next">']
    if prev:
        pager.append(f'<a href="{html.escape(site.href(page, prev))}"><span>Previous</span><b>{html.escape(prev.title)}</b></a>')
    else:
        pager.append('<div class="is-empty"></div>')
    if nxt:
        pager.append(f'<a class="is-next" href="{html.escape(site.href(page, nxt))}"><span>Next</span><b>{html.escape(nxt.title)}</b></a>')
    pager.append("</nav>")
    source = REPO_URL + page.src
    foot = (f'<footer class="foot"><span>Meshware documentation</span>'
            f'<span>Written in <a href="{source}" rel="noopener">{html.escape(page.src)}</a></span></footer>')
    toc = ""
    if len(page.toc) > 1:
        items = "".join(
            f'<li class="is-h{lvl}"><a href="#{site_anchor(site, page, hid)}">{html.escape(text)}</a></li>'
            for lvl, hid, text in page.toc)
        toc = f'<aside class="toc" aria-label="On this page"><div class="toc__label">On this page</div><ol>{items}</ol></aside>'
    title = page.title
    return (f'<div class="page" data-page="{page.slug}" data-title="{html.escape(title)} · Meshware Docs" '
            f'id="p-{page.slug}">'
            f'<article class="article">{crumbs}{eyebrow(page.group)}<h1>{html.escape(title)}</h1>'
            f'<div class="prose">{page.body}</div>{"".join(pager)}{foot}</article>{toc}</div>')


def site_anchor(site: Site, page: Page, hid: str) -> str:
    return f"{page.slug}--{hid}" if site.single else hid


def home_page(site: Site) -> str:
    tiles = []
    for n, (src, label, blurb) in enumerate(TILES):
        p = site.by_src[src]
        lead = " is-lead" if n == 0 else ""
        tiles.append(f'<a class="tile{lead}" href="{html.escape(site.href(None, p) if site.single else p.url)}">'
                     f"<span>{html.escape(label)}</span><b>{html.escape(p.title)}</b><p>{html.escape(blurb)}</p></a>")
    more = []
    for group, _ in NAV[2:]:
        links = "".join(f'<li><a href="{html.escape(site.href(None, p) if site.single else p.url)}">{html.escape(p.title)}</a></li>'
                        for p in site.pages if p.group == group)
        more.append(f"<div><h2>{html.escape(group)}</h2><ul>{links}</ul></div>")
    use = "".join(f'<li><a href="{html.escape(site.href(None, p) if site.single else p.url)}">{html.escape(p.title)}</a></li>'
                  for p in site.pages if p.group == "Use Meshware")
    more.insert(0, f"<div><h2>Use Meshware</h2><ul>{use}</ul></div>")
    return (
        '<div class="page is-home" data-page="home" data-title="Meshware Docs" id="p-home">'
        '<section class="hero pattern">'
        f"{eyebrow('Documentation')}"
        "<h1>Meshware documentation</h1>"
        "<p>You have machines. Pair them, and now you have a computer: run commands across them, keep "
        "services up, and decide who may do what. Everything is here, from the first install to the last "
        "configuration key.</p>"
        f"{search_box('heroSearch', big=True)}"
        "</section>"
        f'<div class="tiles">{"".join(tiles)}</div>'
        f'<div class="home__more">{"".join(more[:4])}</div>'
        "</div>"
    )


def search_entries(site: Site) -> list[dict]:
    entries = []
    for p in site.pages:
        for s in p.sections:
            text = re.sub(r"`|\*\*|\[|\]\([^)]*\)", "", " ".join(s["x"]))
            text = re.sub(r"\s+", " ", text).strip()
            if not s["h"] and not text:
                continue
            url = site.href(None, p, s["id"]) if site.single else (p.url + (f"#{s['id']}" if s["id"] else ""))
            entries.append({"t": p.title, "h": s["h"], "g": p.group, "u": url, "x": text[:220]})
    return entries


def document(head_title: str, body: str, css: str, scripts: str, base: str, single: bool, fragment: bool) -> str:
    attrs = f' data-base="{base}"' + (" data-single" if single else "")
    head = (f"<title>{html.escape(head_title)}</title>"
            f'<link rel="icon" href="{FAVICON}">'
            '<link rel="preconnect" href="https://fonts.googleapis.com">'
            '<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>'
            f'<link rel="stylesheet" href="{FONTS}">'
            f"{css}")
    if fragment:
        # The host supplies the document; this is what goes inside it. The
        # attributes the script reads ride on a wrapper instead of <body>.
        return f"{head}\n<div id=\"mw-docs\"{attrs}>{body}</div>{scripts}"
    return ("<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">"
            '<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">'
            '<meta name="description" content="How to install, run and operate Meshware: machines, deployments, '
            'people and access, secrets, Director, and every setting.">'
            f"{head}</head><body{attrs}>{body}{scripts}</body></html>\n")


def build(single_path: Path | None, out_dir: Path | None, fragment: bool) -> int:
    pages = load_pages()
    single = single_path is not None
    site = Site(pages, single, out_dir)
    for p in pages:
        r = Renderer(p, lambda href, p=p: site.resolve(p, href), lambda src, p=p: site.image(p, src),
                     prefix=f"{p.slug}--" if single else "")
        body = r.render(p.text)
        p.body, p.toc, p.ids, p.sections = body, r.toc, r.ids, r.sections
    site.check_anchors()
    if site.errors:
        print(f"docs site: {len(site.errors)} dead link(s)", file=sys.stderr)
        for e in site.errors:
            print(f"    {e}", file=sys.stderr)
        return 1

    css = (ASSETS / "site.css").read_text()
    js = (ASSETS / "site.js").read_text()
    index = "window.MW_SEARCH=" + json.dumps(search_entries(site), separators=(",", ":"), ensure_ascii=False) + ";"
    if single:
        # The single document's body carries data-single; a fragment's wrapper does.
        body = topbar(site, None) + '<div class="shell">' + sidebar(site, None) + '<main class="main" id="main">' + \
            home_page(site) + "".join(article(site, p, i) for i, p in enumerate(pages)) + "</main></div>"
        body = body.replace('<div class="page" data-page', '<div class="page" hidden data-page')
        scripts = f"<script>{index}</script><script>{js.replace('document.body.hasAttribute', 'mwRoot().hasAttribute').replace('document.body.getAttribute', 'mwRoot().getAttribute')}</script>"
        scripts = "<script>function mwRoot(){return document.getElementById('mw-docs')||document.body;}</script>" + scripts
        text = document("Meshware Docs", body, f"<style>{css}</style>", scripts, "", True, fragment)
        single_path.parent.mkdir(parents=True, exist_ok=True)
        single_path.write_text(text)
        size = single_path.stat().st_size
        print(f"{single_path.relative_to(ROOT) if single_path.is_relative_to(ROOT) else single_path}: "
              f"{len(pages)} pages in one file, {size / 1e6:.1f} MB")
        return 0

    if out_dir.exists():
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True)
    (out_dir / "assets").mkdir()
    (out_dir / "assets" / "site.css").write_text(css)
    (out_dir / "assets" / "site.js").write_text(js)
    (out_dir / "assets" / "search.js").write_text(index)
    for rel, repo in site.images.items():
        dest = out_dir / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / repo, dest)

    def write(url: str, title: str, current: Page | None, main_html: str, is_home: bool = False) -> None:
        depth = os.path.dirname(url)
        up = os.path.relpath("assets", depth or ".")
        css_link = f'<link rel="stylesheet" href="{up}/site.css">'
        scripts = f'<script src="{up}/search.js"></script><script src="{up}/site.js"></script>'
        base = os.path.relpath(".", depth or ".") + "/"
        base = "" if base == "./" else base
        main_cls = "main home" if is_home else "main"
        body = (topbar(site, current if current else None) + '<div class="shell">' + sidebar(site, current) +
                f'<main class="{main_cls}" id="main">{main_html}</main></div>')
        (out_dir / url).parent.mkdir(parents=True, exist_ok=True)
        (out_dir / url).write_text(document(title, body, css_link, scripts, base, False, False))

    # The home page links as the top level does.
    write("index.html", "Meshware Docs", None, home_page(site), is_home=True)
    for i, p in enumerate(pages):
        write(p.url, f"{p.title} · Meshware Docs", p, article(site, p, i))
    problems = verify_tree(out_dir)
    if problems:
        print(f"docs site: {len(problems)} broken link(s) in the output", file=sys.stderr)
        for e in problems:
            print(f"    {e}", file=sys.stderr)
        return 1
    print(f"{out_dir.relative_to(ROOT) if out_dir.is_relative_to(ROOT) else out_dir}: {len(pages) + 1} pages, "
          f"{len(site.images)} images; every link and anchor resolves")
    return 0


def verify_tree(out: Path) -> list[str]:
    """Every relative href and src in the output points at a file, and every
    #anchor at an id in it. The check that makes the site's own promise."""
    ids: dict[Path, set] = {}
    problems = []
    files = sorted(out.rglob("*.html"))
    for f in files:
        ids[f] = set(re.findall(r'\sid="([^"]+)"', f.read_text()))
    for f in files:
        for attr, target in re.findall(r'\s(href|src)="([^"]+)"', f.read_text()):
            if re.match(r"^[a-z][a-z0-9+.-]*:", target, re.I):
                continue
            path, _, anchor = html.unescape(target).partition("#")
            dest = (f.parent / path).resolve() if path else f
            if not dest.exists():
                problems.append(f"{f.relative_to(out)}: {target} — no such file")
                continue
            if anchor and dest.suffix == ".html" and anchor not in ids.get(dest, set()):
                problems.append(f"{f.relative_to(out)}: {target} — no such anchor")
    return problems


def main(argv: list[str]) -> int:
    out = ROOT / "target" / "docs-site"
    single = None
    fragment = "--fragment" in argv
    if "--out" in argv:
        out = Path(argv[argv.index("--out") + 1]).resolve()
    if "--single" in argv:
        single = Path(argv[argv.index("--single") + 1]).resolve()
    if "-h" in argv or "--help" in argv:
        print(__doc__)
        return 0
    return build(single, None if single else out, fragment)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
