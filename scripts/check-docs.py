#!/usr/bin/env python3
"""Check the shipped documentation against itself and against the binary.

    scripts/check-docs.py                  # every check
    scripts/check-docs.py links            # one of them

Checks:

  links     Every relative link in the shipped Markdown points at a file that
            exists, and every #anchor at a heading that exists (GitHub's
            slug rules). A doc that sends a reader to a page that is not
            there is a doc that stops being read.

  commands  Every `meshnode …` line the docs show — console blocks and
            inline code — goes through the real parser, with
            MESHWARE_CHECK_ARGS=1 so it parses and exits without running.
            A line somebody would paste must parse whole; an inline mention
            in a sentence (`meshnode adopt`) only has to name real verbs and
            flags. `<placeholders>` and `…` stand in for values.

  config    Every meshnode.toml snippet goes through `meshnode config check`.

  deployment
            Every key `meshnode deployment schema` accepts is described in
            the deployment file reference.

  help      No internal plan code — `(M31 A2)`, `(SEC-21)`, `MASTERPLAN §12`,
            `**M-14**` — in any page of `meshnode`'s help, read as the man
            pages carry it. Those codes point into plans that do not ship;
            in help they are noise a customer reads first.

  codes     The same for the shipped Markdown, outside code blocks. A `§2`
            on its own is a cross-reference to a doc's own numbered section
            and is fine; `(N5 P2)` or `MASTERPLAN D10` is not.

The binary is $MESHNODE, else the newest of target/{debug,release}/meshnode,
else `meshnode` on PATH.

Standard library only, so it runs anywhere CI does. Exit status is the
number of failed checks.
"""

from __future__ import annotations

import re
import sys
import unicodedata
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# What ships: the README, the security policy and everything under docs/.
# INTERNAL/ is ours and is never linked to.
SHIPPED = [
    ROOT / "README.md",
    ROOT / "SECURITY.md",
    *sorted((ROOT / "docs").rglob("*.md")),
]

# The changelog ships too, but only its links are checked: an old entry
# names the commands and keys of its own release, which is its job, and
# the next release may remove them.
LINKS_ONLY = [ROOT / "CHANGELOG.md", ROOT / "CONTRIBUTING.md"]

FENCE = re.compile(r"^(```|~~~)")
LINK = re.compile(r"(?<!!)\[(?:[^\]\\]|\\.)*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
IMAGE = re.compile(r"!\[(?:[^\]\\]|\\.)*\]\(([^)\s]+)\)")
REFDEF = re.compile(r"^\s{0,3}\[[^\]]+\]:\s+(\S+)", re.M)
HEADING = re.compile(r"^(#{1,6})\s+(.*?)\s*#*\s*$")


def prose_lines(text: str):
    """Yield (line number, line) outside fenced code blocks."""
    fenced = False
    for n, line in enumerate(text.splitlines(), 1):
        if FENCE.match(line.lstrip()):
            fenced = not fenced
            continue
        if not fenced:
            yield n, line


def slug(heading: str) -> str:
    """GitHub's anchor for a heading: the text, lower-cased, with anything
    that is not a letter, digit, space, hyphen or underscore removed and
    spaces turned into hyphens."""
    text = re.sub(r"<[^>]+>", "", heading)
    text = re.sub(r"!\[[^\]]*\]\([^)]*\)", "", text)
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)
    text = text.replace("`", "").replace("*", "")
    out = []
    for ch in text.lower():
        cat = unicodedata.category(ch)
        if ch in " -_" or cat[0] in "LN":
            out.append(ch)
    return "".join(out).replace(" ", "-")


def anchors(path: Path) -> set[str]:
    seen: dict[str, int] = {}
    found = set()
    for _, line in prose_lines(path.read_text()):
        m = HEADING.match(line)
        if not m:
            continue
        base = slug(m.group(2))
        n = seen.get(base, 0)
        seen[base] = n + 1
        found.add(base if n == 0 else f"{base}-{n}")
    # Explicit HTML anchors count too.
    found |= set(re.findall(r'<a id="([^"]+)"', path.read_text()))
    return found


def check_links() -> list[str]:
    problems = []
    cache: dict[Path, set[str]] = {}
    for doc in SHIPPED + LINKS_ONLY:
        if not doc.exists():
            continue
        text = doc.read_text()
        targets = []
        for n, line in prose_lines(text):
            for m in LINK.finditer(line):
                targets.append((n, m.group(1)))
            for m in IMAGE.finditer(line):
                targets.append((n, m.group(1)))
        for m in REFDEF.finditer(text):
            targets.append((text[: m.start()].count("\n") + 1, m.group(1)))
        for n, target in targets:
            if re.match(r"^[a-z][a-z0-9+.-]*:", target) or target.startswith("//"):
                continue  # an external URL, a mailto:, …
            where = f"{doc.relative_to(ROOT)}:{n}"
            file_part, _, frag = target.partition("#")
            dest = doc if not file_part else (doc.parent / file_part).resolve()
            if not dest.exists():
                problems.append(f"{where}: {target} — no such file")
                continue
            if ROOT / "INTERNAL" in dest.parents:
                problems.append(f"{where}: {target} — links into INTERNAL/, which does not ship")
                continue
            if frag and dest.suffix == ".md":
                if dest not in cache:
                    cache[dest] = anchors(dest)
                if frag not in cache[dest]:
                    problems.append(f"{where}: {target} — no heading #{frag} in {dest.name}")
    return problems


# -- commands ----------------------------------------------------------------

INLINE = re.compile(r"`((?:sudo\s+)?meshnode\s[^`]+)`")
SHELL_LANGS = {"", "console", "sh", "bash", "shell", "shell-session"}
PLACEHOLDER = re.compile(r"<[^<>\s][^<>]*>")


def meshnode_binary() -> str:
    import os
    import shutil

    if os.environ.get("MESHNODE"):
        return os.environ["MESHNODE"]
    built = [ROOT / "target/debug/meshnode", ROOT / "target/release/meshnode"]
    built = [b for b in built if b.exists()]
    if built:
        return str(max(built, key=lambda b: b.stat().st_mtime))
    found = shutil.which("meshnode")
    if not found:
        raise SystemExit("no meshnode binary: build one, or set MESHNODE")
    return found


def documented_commands():
    """Yield (where, command line, whole) for every `meshnode …` the docs
    show. `whole` is a line somebody would paste into a terminal; an inline
    mention in prose (`meshnode adopt`) names a verb and may leave out its
    arguments."""
    for doc in SHIPPED:
        if not doc.exists():
            continue
        lines = doc.read_text().splitlines()
        lang = None
        i = 0
        while i < len(lines):
            line = lines[i]
            stripped = line.lstrip("> ").lstrip()
            fence = FENCE.match(stripped)
            if fence:
                lang = None if lang is not None else stripped[3:].strip().lower()
                i += 1
                continue
            where = f"{doc.relative_to(ROOT)}:{i + 1}"
            if lang is not None:
                if lang in SHELL_LANGS:
                    text = stripped
                    if lang == "console" or text.startswith("$ "):
                        if not text.startswith("$ "):
                            i += 1
                            continue
                        text = text[2:]
                    # A trailing backslash continues the command.
                    while text.endswith("\\") and i + 1 < len(lines):
                        i += 1
                        text = text[:-1] + " " + lines[i].lstrip("> ").strip()
                    for cmd in split_pipeline(text):
                        yield where, cmd, True
            else:
                for m in INLINE.finditer(line):
                    for cmd in split_pipeline(m.group(1)):
                        yield where, cmd, False
            i += 1


def split_pipeline(text: str):
    """The `meshnode` invocations in one shell line, with comments, pipes,
    redirects, `sudo` and environment assignments taken off."""
    # Cut a trailing comment: a `#` after whitespace, outside quotes.
    out, quote = [], None
    for idx, ch in enumerate(text):
        if quote:
            if ch == quote:
                quote = None
        elif ch in "'\"":
            quote = ch
        elif ch == "#" and (idx == 0 or text[idx - 1].isspace()):
            break
        out.append(ch)
    text = "".join(out)
    for part in split_unquoted(text, ("||", "&&", ";", "|")):
        part = split_unquoted(" " + part, (" > ", " >> ", " 2> ", " < "))[0].strip()
        words = part.split()
        while words and (re.match(r"^[A-Z_][A-Z0-9_]*=", words[0]) or words[0] in ("sudo", "env", "time")):
            if words[0] == "sudo" and len(words) > 2 and words[1] == "-u":
                words = words[3:]
            else:
                words = words[1:]
        if words and words[0] == "meshnode":
            yield " ".join(words)


def split_unquoted(text: str, seps: tuple[str, ...]) -> list[str]:
    """`text` cut at each separator that is not inside quotes."""
    parts, cur, quote, i = [], [], None, 0
    while i < len(text):
        ch = text[i]
        if quote:
            if ch == quote:
                quote = None
            cur.append(ch)
            i += 1
            continue
        if ch in "'\"":
            quote = ch
            cur.append(ch)
            i += 1
            continue
        for sep in seps:
            if text.startswith(sep, i):
                parts.append("".join(cur))
                cur = []
                i += len(sep)
                break
        else:
            cur.append(ch)
            i += 1
    parts.append("".join(cur))
    return parts


def argv_for(cmd: str, drop_ellipsis: bool, fill: str = "x") -> list[str] | None:
    import shlex

    cmd = PLACEHOLDER.sub(fill, cmd)
    try:
        words = shlex.split(cmd)
    except ValueError:
        return None
    out = []
    for w in words:
        if w in ("…", "...", "[…]"):
            if not drop_ellipsis:
                out.append("x")
            continue
        out.append(w.replace("…", "x"))
    return out[1:]  # without `meshnode` itself


def check_commands() -> list[str]:
    import os
    import subprocess

    binary = meshnode_binary()
    env = dict(os.environ, MESHWARE_CHECK_ARGS="1")
    problems = []
    seen: dict[tuple[str, bool], str | None] = {}

    def parses(argv):
        proc = subprocess.run([binary, *argv], env=env, capture_output=True, text=True)
        return proc.returncode == 0, (proc.stderr or proc.stdout).strip()

    for where, cmd, whole in documented_commands():
        key = (cmd, whole)
        if key not in seen:
            error = None
            tries = [argv_for(cmd, drop, fill) for fill in ("x", "1") for drop in (False, True)]
            if tries[0] is None:
                error = "cannot be split as shell words"
            else:
                first_err = None
                for argv in dict.fromkeys(tuple(t) for t in tries if t is not None):
                    ok, err = parses(list(argv))
                    if ok or (not whole and mentions_a_verb(err)):
                        break
                    first_err = first_err or err
                else:
                    error = first_err.splitlines()[0] if first_err else "refused"
            seen[key] = error
        if seen[key]:
            problems.append(f"{where}: `{cmd}` — {seen[key]}")
    return problems


def mentions_a_verb(err: str) -> bool:
    """A refusal that only says arguments are missing — which is what naming
    a verb in a sentence looks like to the parser."""
    first = err.splitlines()[0] if err else ""
    return (
        "the following required arguments were not provided" in first
        or not first.startswith("error:")  # a verb that needs a subcommand prints its help
    )


# -- config ------------------------------------------------------------------

# A TOML block is a node's config when its tables are the config file's own
# sections. Deployment and schedule files, and other programs' config
# (Vector, systemd), are not, and are skipped.
NOT_NODE_CONFIG = re.compile(r"^\s*\[+(deployment|schedule|sources|transforms|sinks\.|Resolve|Service|Unit)\b", re.M)
NODE_CONFIG = re.compile(
    r"^\s*(\[+[a-z_]+\]+|node_name\s*=|data_dir\s*=|tags\s*=|listen\s*=)", re.M
)


def documented_configs():
    for doc in SHIPPED:
        if not doc.exists():
            continue
        lines = doc.read_text().splitlines()
        lang, start, block = None, 0, []
        for i, line in enumerate(lines):
            stripped = line.lstrip("> ").lstrip()
            if FENCE.match(stripped):
                if lang is None:
                    lang, start, block = stripped[3:].strip().lower(), i + 1, []
                else:
                    text = "\n".join(block)
                    if lang == "toml" and NODE_CONFIG.search(text) and not NOT_NODE_CONFIG.search(text):
                        yield f"{doc.relative_to(ROOT)}:{start}", text
                    lang = None
                continue
            if lang is not None:
                block.append(line.lstrip("> ") if line.startswith(">") else line)


def check_config() -> list[str]:
    import subprocess
    import tempfile

    binary = meshnode_binary()
    problems = []
    with tempfile.TemporaryDirectory() as tmp:
        for n, (where, text) in enumerate(documented_configs()):
            path = Path(tmp) / f"snippet-{n}.toml"
            path.write_text(text + "\n")
            proc = subprocess.run(
                [binary, "--data-dir", str(Path(tmp) / "home"), "config", "check", "--config", str(path)],
                capture_output=True,
                text=True,
            )
            if proc.returncode != 0:
                detail = [l.strip() for l in (proc.stderr or proc.stdout).splitlines() if l.strip()]
                problems.append(f"{where}: {' '.join(detail)[:300]}")
    return problems


# -- deployment schema -------------------------------------------------------


def check_deployment_keys() -> list[str]:
    """Every key `meshnode deployment schema` lists is described in the
    deployment file reference — the parser refuses any key not in the
    schema, so the schema is the complete list."""
    import json
    import subprocess

    out = subprocess.run(
        [meshnode_binary(), "deployment", "schema"], capture_output=True, text=True, check=True
    ).stdout
    schema = json.loads(out)
    keys = set()

    def walk(node):
        if isinstance(node, dict):
            for key, sub in node.get("properties", {}).items():
                keys.add(key)
                walk(sub)
            for sub in node.values():
                if isinstance(sub, (dict, list)):
                    walk(sub)
        elif isinstance(node, list):
            for sub in node:
                walk(sub)

    walk(schema)
    page = ROOT / "docs" / "reference" / "deployment-file.md"
    text = page.read_text()
    return [
        f"{page.relative_to(ROOT)}: the schema accepts `{key}`, and the reference never mentions it"
        for key in sorted(keys)
        if f"`{key}`" not in text and f"{key}:" not in text
    ]


# -- help text ---------------------------------------------------------------

# A plan code: in parentheses — `(M31 A2)`, `(N1 P5)`, `(SEC-21)`, `(§9)` —
# or bold (`**M-14**`), or MASTERPLAN and a section sign anywhere, or a bare
# `M31`, `C7`, `N5 P2`, `ENG-7` standing alone in a sentence.
HELP_CODE = re.compile(
    r"\((?=(?:[A-Z]{1,3}\d|C0[–-]C9|MASTERPLAN|§|[A-Z]{1,5}-[A-Z]?\d))[^()]*\)"
    r"|MASTERPLAN|\*\*[A-Z]{1,3}-\d+\*\*|§\s*\d"
    r"|(?<![\w/.-])(?:[A-Z]{1,2}\d{1,2}(?:\.\d)?(?:\s+P\d)?|[A-Z]{2,5}-\d{1,2})(?![\w/-])"
)
# Real names that look like codes.
NOT_A_CODE = {"S3", "R2", "H2", "H3", "UTF-8", "P2P"}


def roff_text(roff: str) -> str:
    """What a reader sees of a man page: escapes and font changes gone."""
    for a, b in ((r"\-", "-"), (r"\(em", "—"), (r"\[", "["), (r"\]", "]"), (r"\(aq", "'")):
        roff = roff.replace(a, b)
    return re.sub(r"\\f[BRIP]", "", roff)


def check_help() -> list[str]:
    import subprocess
    import tempfile

    problems = []
    with tempfile.TemporaryDirectory() as out:
        subprocess.run(
            [meshnode_binary(), "manpage", "--dir", out], check=True, capture_output=True
        )
        for page in sorted(Path(out).glob("*.1")):
            for line in roff_text(page.read_text()).splitlines():
                for m in HELP_CODE.finditer(line):
                    if m.group(0).strip("()") in NOT_A_CODE:
                        continue
                    problems.append(f"{page.stem}: {m.group(0)} — {line.strip()[:90]}")
    return problems


# A doc's own numbered section — `§7`, `(§2)`, `§6b` — is a cross-reference,
# not a plan code.
SECTION_REF = re.compile(r"\(?§\s*[\d.]+[a-z]?\)?")


def check_codes() -> list[str]:
    problems = []
    for doc in SHIPPED + LINKS_ONLY:
        if not doc.exists():
            continue
        for n, line in prose_lines(doc.read_text()):
            for m in HELP_CODE.finditer(line):
                code = m.group(0)
                if code.strip("()") in NOT_A_CODE or SECTION_REF.fullmatch(code):
                    continue
                problems.append(f"{doc.relative_to(ROOT)}:{n}: {code} — {line.strip()[:80]}")
    return problems


CHECKS = {
    "links": check_links,
    "commands": check_commands,
    "config": check_config,
    "deployment": check_deployment_keys,
    "help": check_help,
    "codes": check_codes,
}


def main(argv: list[str]) -> int:
    wanted = argv[1:] or list(CHECKS)
    failed = 0
    for name in wanted:
        if name not in CHECKS:
            print(f"unknown check: {name} (have: {', '.join(CHECKS)})", file=sys.stderr)
            return 2
        problems = CHECKS[name]()
        if problems:
            failed += 1
            print(f"✗ {name}: {len(problems)} problem(s)")
            for p in problems:
                print(f"    {p}")
        else:
            print(f"✓ {name}")
    return failed


if __name__ == "__main__":
    sys.exit(main(sys.argv))
