#!/usr/bin/env python3
"""Generate the reference pages from the code, so they cannot drift (DOC-6).

    scripts/gen-reference.py config                 # docs/reference/config.md, from config.rs
    scripts/gen-reference.py metrics --scrape FILE  # docs/reference/metrics.md, from a /metrics scrape
    scripts/gen-reference.py --check                # CI: fail on any drift

`config` reads `crates/mesh-core/src/config.rs`: every table of `ConfigFile`,
every key, its type and the doc comment on it. What reloads on SIGHUP is the
one thing the declarations do not say, so it is the table `RELOAD` below,
which mirrors `Node::apply_config` in `daemon.rs`.

`metrics` takes a scrape of a running node's `/metrics` — the only place a
family's help text, type and labels are all spelled out, because several
names are assembled with `format!` — and adds the families a lone node does
not emit (sinks, accelerators, peers) from the `# HELP` lines in the source.

`--check` needs no node: it regenerates the config page and compares it,
and it checks that every metric name in the source that writes metrics is
on the metrics page, and every `MESHWARE_*` variable a user can set or a
job can read is on the environment page.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CONFIG_RS = ROOT / "crates" / "mesh-core" / "src" / "config.rs"
DOCS = ROOT / "docs" / "reference"

# ---------------------------------------------------------------------------
# Shared
# ---------------------------------------------------------------------------

# Internal plan codes — `(M12 §6)`, `(SEC-21)`, `(E1-P6)`, `MASTERPLAN §13` —
# mean nothing to a reader of the shipped docs (DOC-9), so they are taken out
# of anything copied from a doc comment.
PLAN_CODE = re.compile(
    r"\s*\((?=(?:[A-Z]{1,3}\d|C0[–-]C9|MASTERPLAN|§|[A-Z]{1,5}-[A-Z]?\d))[^()]*\)"
)
MASTERPLAN = re.compile(r"\s*\bMASTERPLAN\s*(?:§\s*[\d.]+|[A-Z]\d+)")


def clean(text: str) -> str:
    text = PLAN_CODE.sub("", text)
    text = MASTERPLAN.sub("", text)
    return re.sub(r"[ \t]+\n", "\n", text).strip()


def paragraphs(lines: list[str]) -> str:
    """Doc-comment lines → Markdown paragraphs (a blank `///` splits)."""
    out, cur = [], []
    for line in lines:
        if not line.strip():
            if cur:
                out.append(" ".join(cur))
                cur = []
        else:
            cur.append(line.strip())
    if cur:
        out.append(" ".join(cur))
    return clean("\n\n".join(out))


# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

# What a SIGHUP (`systemctl reload meshnode`) applies, from `apply_config`.
# `True` for a whole table, or a set of the keys in it that reload. Anything
# not named needs a restart.
RELOAD: dict[str, object] = {
    "": {"tags"},
    "log": {"level"},
    "secrets": True,
    "limits": True,
    "retention": True,
    "wan": True,
    "compute": {"placement"},
    "network": {"dynamic_ports", "reserved_ports", "advertise_interfaces", "proxy", "proxy_prefix"},
    "volume": True,
    "runtime": {"sandbox", "container_plugin"},
    "sessions": True,
    "disk": True,
}
# Read once and never again, or not by the daemon at all.
SPECIAL = {
    ("mesh", "name"): "used once, when this node founds a mesh",
    ("network", "proxy"): "retired: loads, and is ignored with a warning",
    ("network", "proxy_prefix"): "retired: loads, and is ignored with a warning",
}

TYPE_NAMES = [
    (r"Option<(.+)>", r"\1"),
    (r"Vec<String>", "list of strings"),
    (r"Vec<u16>", "list of port numbers"),
    (r"Vec<(.+)>", r"list of \1"),
    (r"String", "string"),
    (r"PathBuf", "path"),
    (r"bool", "boolean"),
    (r"u16|u32|u64|usize", "integer"),
    (r"f64", "number"),
    (r"QuietSpec", "string, or list of strings"),
    (r"BTreeMap<String, ProviderEntry>", "tables, one per provider"),
]


def toml_type(rust: str) -> str:
    t = rust.strip()
    for pat, rep in TYPE_NAMES:
        new = re.sub(rf"^{pat}$", rep, t)
        if new != t:
            t = new
            if not t.startswith("list of") and "<" in t:
                continue
    return t


def rust_structs(src: str) -> dict[str, tuple[list[str], list[tuple[str, str, list[str]]]]]:
    """name → (doc lines, [(field, type, doc lines)])."""
    lines = src.splitlines()
    out = {}
    i = 0
    while i < len(lines):
        m = re.match(r"^pub struct (\w+) \{$", lines[i])
        if not m:
            i += 1
            continue
        name = m.group(1)
        # The struct's doc comment: the `///` block above its attributes.
        j = i - 1
        while j >= 0 and lines[j].startswith("#["):
            j -= 1
        doc = []
        while j >= 0 and lines[j].startswith("///"):
            doc.insert(0, lines[j][3:])
            j -= 1
        fields, pending = [], []
        i += 1
        while i < len(lines) and lines[i] != "}":
            line = lines[i].strip()
            if line.startswith("///"):
                pending.append(line[3:])
            elif line.startswith("#[") or line.startswith("//") or not line:
                pass
            else:
                fm = re.match(r"^pub (\w+):\s*(.+?),?$", line)
                if fm:
                    fields.append((fm.group(1), fm.group(2).rstrip(","), pending))
                pending = []
            i += 1
        out[name] = (doc, fields)
    return out


def reload_of(section: str, key: str) -> str:
    if (section, key) in SPECIAL:
        return SPECIAL[(section, key)]
    rule = RELOAD.get(section)
    if rule is True or (isinstance(rule, set) and key in rule):
        return "reloads"
    return "restart"


def render_config() -> str:
    src = CONFIG_RS.read_text()
    structs = rust_structs(src)
    _, top = structs["ConfigFile"]
    lines = [
        "# Configuration reference",
        "",
        "<!-- GENERATED by scripts/gen-reference.py from crates/mesh-core/src/config.rs."
        " Do not edit by hand; CI fails when this file is stale. -->",
        "",
        "Every key a node's `meshnode.toml` accepts. The file is optional, and so is every key in it:",
        "a node with no file runs on defaults, and that is how most nodes run.",
        "",
        "| | Where the file is |",
        "|---|---|",
        "| A node you started with `meshnode up` | `~/.meshware/meshnode.toml`, or `$MESHWARE_HOME/meshnode.toml` |",
        "| A packaged node (`.deb`, `.rpm`, `meshnode adopt`) | `/etc/meshware/meshnode.toml` |",
        "| The node Director drives | `~/.meshware/meshnode.toml` — the same file, since Director runs no node of its own |",
        "",
        "**An unknown key is refused**, at every level, rather than ignored: a misspelling would",
        "otherwise be a setting that silently does nothing. Check a file before a node reads it:",
        "",
        "```console",
        "$ meshnode config check              # parse and validate this node's file",
        "$ meshnode config show               # the configuration after flags, environment and defaults",
        "$ meshnode config paths              # every path the node uses",
        "```",
        "",
        "**Precedence**, highest first: a command-line flag, then an environment variable",
        "([environment reference](environment.md)), then this file, then the default.",
        "",
        "**Reloading.** `systemctl reload meshnode` (or `SIGHUP`) applies the keys marked *reloads*",
        "below while the node keeps running; the node logs what it applied and names anything that",
        "needs a restart. Keys marked *restart* take effect the next time the node starts.",
        "",
        "## At a glance",
        "",
        "| Table | What it is for | On reload |",
        "|---|---|---|",
    ]
    sections = []
    for field, ty, doc in top:
        if ty.endswith("Section") or ty.startswith("Vec<"):
            struct = re.sub(r"^Vec<(\w+)>$", r"\1", ty)
            header = f"[[{field}]]" if ty.startswith("Vec<") else f"[{field}]"
            sections.append((field, header, struct, doc))
    top_keys = [(f, ty, doc) for f, ty, doc in top if not (ty.endswith("Section") or ty.startswith("Vec<"))]

    def summary(struct_doc: list[str]) -> str:
        text = paragraphs(struct_doc)
        first = text.split("\n\n")[0]
        first = re.sub(r"^`\[\[?[\w.]+\]\]?`\s*[—-]\s*", "", first)
        sentence = re.split(r"(?<=[.!?])\s", first)[0]
        sentence = sentence[:1].upper() + sentence[1:]
        return sentence.replace("|", "\\|")

    lines.append("| (top level) | This node's name, data directory and tags | `tags` reloads |")
    for field, header, struct, _ in sections:
        doc, fields = structs[struct]
        rule = RELOAD.get(field)
        if rule is True:
            when = "reloads"
        elif isinstance(rule, set) and rule:
            keys = [k for k, _, _ in fields if k in rule]
            when = ", ".join(f"`{k}`" for k in keys) + " reload" + ("s" if len(keys) == 1 else "") if keys else "restart"
            if len(keys) == len(fields):
                when = "reloads"
        else:
            when = "restart"
        anchor = header.strip("[]")
        lines.append(f"| [`{header}`](#{anchor}) | {summary(doc) or '—'} | {when} |")
    lines.append("")

    def key_block(section: str, key: str, ty: str, doc: list[str]) -> list[str]:
        desc = paragraphs(doc) or "*No description.*"
        head = f"- **`{key}`** · {toml_type(ty)} · *{reload_of(section, key)}*"
        body = desc.replace("\n\n", "\n\n  ")
        return [head, "", f"  {body}", ""]

    lines += ["## Top-level keys", "", "Outside any table, at the top of the file.", ""]
    for key, ty, doc in top_keys:
        lines += key_block("", key, ty, doc)
    for field, header, struct, _ in sections:
        doc, fields = structs[struct]
        lines += [f"## {header}", "", f'<a id="{header.strip("[]")}"></a>', ""]
        text = paragraphs(doc)
        if text:
            lines += [text, ""]
        if field == "secrets":
            lines += [
                "Each provider is its own table, named for the provider:",
                "",
                "```toml",
                "[secrets.providers.vault]",
                'plugin = "/usr/local/libexec/meshware-vault"',
                'config = { addr = "https://vault.internal:8200" }',
                "```",
                "",
            ]
            _, entry = structs["ProviderEntry"]
            for key, ty, kdoc in entry:
                lines += key_block("secrets", key, ty, kdoc)
            continue
        for key, ty, kdoc in fields:
            lines += key_block(field, key, ty, kdoc)
    return "\n".join(lines).rstrip() + "\n"


# ---------------------------------------------------------------------------
# Metrics
# ---------------------------------------------------------------------------

# Families a lone node with no sinks, no accelerators and no peers does not
# emit, with the condition that turns each on.
CONDITIONAL = {
    "meshware_gpu_": "on a node with accelerators whose telemetry has arrived",
    "meshware_sink_": "on the node exporting for at least one sink",
    "meshware_peer_sync_": "once this node has peers",
    "meshware_oplog_": "once the op-log census has been taken (shortly after start)",
}


def labels_in_source() -> dict[str, set[str]]:
    """Label names each family is written with, from the format strings —
    for the families a lone node emits no samples of."""
    out: dict[str, set[str]] = {}
    for path in (ROOT / "crates" / "mesh-core" / "src").rglob("*.rs"):
        text = path.read_text()
        if "# TYPE" not in text:
            continue
        for m in re.finditer(r"((?:meshware|process)_[a-z0-9_]+)\{\{(.*?)\}\}", text):
            for label in re.findall(r"([a-z_]+)=\\?\"", m.group(2)):
                if label != "le":
                    out.setdefault(m.group(1), set()).add(label)
        # A family written through a `{labels}` placeholder names its labels
        # where the placeholder is built.
        for m in re.finditer(r"((?:meshware|process)_[a-z0-9_]+)\{\{\{labels\}\}\}", text):
            built = re.search(r'let labels = format!\(\s*"((?:[^"\\]|\\.)*)"', text)
            if built:
                for label in re.findall(r"([a-z_]+)=", built.group(1)):
                    out.setdefault(m.group(1), set()).add(label)
        # A table of families written in one loop through `{name}{{…}}`.
        for m in re.finditer(r"\{name\}\{\{(.*?)\}\}", text):
            labels = [l for l in re.findall(r"([a-z_]+)=\\?\"", m.group(1)) if l != "le"]
            loop = text[: m.start()].rfind("for (name,")
            if loop >= 0:
                for name in re.findall(r'"((?:meshware|process)_[a-z0-9_]+)"', text[loop : m.start()]):
                    out.setdefault(name, set()).update(labels)
    return out


def metric_names_in_source() -> dict[str, str]:
    """Every metric family name the metrics-writing source spells out, with
    the help text of any `# HELP` line that names it."""
    names: dict[str, str] = {}
    for path in (ROOT / "crates" / "mesh-core" / "src").rglob("*.rs"):
        text = path.read_text()
        if "# TYPE" not in text:
            continue
        cut = text.find("#[cfg(test)]")
        body = text if cut < 0 else text[:cut]
        # Rust string continuations: a backslash, the newline, the indent.
        flat = re.sub(r"\\\n\s*", "", body)
        for m in re.finditer(r"# HELP ((?:meshware|process)_[a-z0-9_]+) ([^\"\\{]+)", flat):
            names[m.group(1)] = m.group(2).strip()
        # `gauge("name", "help", …)` and `("name", "help", "kind")` tables.
        for m in re.finditer(r'"((?:meshware|process)_[a-z0-9_]+)",\s*"((?:[^"\\]|\\.)+)"', flat):
            names.setdefault(m.group(1), m.group(2).strip())
        for m in re.finditer(r'"((?:meshware|process)_[a-z0-9_]+)"', flat):
            name = m.group(1)
            if name.endswith(("_bucket", "_count", "_sum")) or name.endswith("_"):
                continue
            names.setdefault(name, "")
    return names


def parse_scrape(text: str) -> list[dict]:
    fams: dict[str, dict] = {}
    order = []
    for line in text.splitlines():
        if line.startswith("# HELP "):
            _, _, name, help_ = line.split(" ", 3)
            fams.setdefault(name, {"name": name, "labels": set()})["help"] = help_
            if name not in order:
                order.append(name)
        elif line.startswith("# TYPE "):
            _, _, name, kind = line.split(" ", 3)
            fams.setdefault(name, {"name": name, "labels": set()})["type"] = kind
            if name not in order:
                order.append(name)
        elif line and not line.startswith("#"):
            m = re.match(r"^([a-z_0-9]+?)(?:_bucket|_sum|_count)?(?:\{(.*)\})?\s", line)
            if not m:
                continue
            base = m.group(1)
            fam = fams.get(base) or fams.get(re.sub(r"_(bucket|sum|count)$", "", base))
            if fam and m.group(2):
                for pair in re.findall(r'(\w+)="', m.group(2)):
                    if pair != "le":
                        fam["labels"].add(pair)
    return [fams[n] for n in order]


def render_metrics(scrape: str) -> str:
    fams = parse_scrape(scrape)
    seen = {f["name"] for f in fams}
    source = metric_names_in_source()
    source_labels = labels_in_source()
    for f in fams:
        f["labels"] |= source_labels.get(f["name"], set())
    extra = []
    for name, help_ in sorted(source.items()):
        if name in seen or f"{name}_seconds" in seen or name.startswith("process_"):
            continue
        cond = next((c for p, c in CONDITIONAL.items() if name.startswith(p)), None)
        extra.append(
            {"name": name, "help": help_ or "", "type": "", "labels": source_labels.get(name, set()), "cond": cond}
        )
    lines = [
        "# Metrics reference",
        "",
        "<!-- GENERATED by scripts/gen-reference.py from a running node's /metrics and the"
        " source. Do not edit by hand; CI fails when a metric is missing from it. -->",
        "",
        "Every node serves Prometheus metrics at `http://127.0.0.1:7776/metrics` — loopback only",
        "unless `[http] addr` says otherwise ([configuration](config.md#http)) — with no exporter",
        "to install. `/healthz` on the same port answers `200` with a JSON summary while the node",
        "is healthy and `503` with the reasons in `\"problems\"` when it is not.",
        "",
        "```console",
        "$ curl -s localhost:7776/metrics | grep meshware_jobs_running",
        "$ curl -s localhost:7776/healthz",
        "```",
        "",
        "Every family is **this node's own view**: scrape every node, and aggregate in Prometheus.",
        "",
        "| Metric | Type | Labels | What it measures |",
        "|---|---|---|---|",
    ]
    for f in fams:
        labels = ", ".join(f"`{l}`" for l in sorted(f["labels"])) or "—"
        lines.append(f"| `{f['name']}` | {f.get('type', '')} | {labels} | {clean(f.get('help', '')).replace('|', '/')} |")
    if extra:
        lines += [
            "",
            "## Emitted only when there is something to report",
            "",
            "A node leaves these out entirely, rather than reporting a zero, until the condition",
            "holds — a saturated GPU that reads as idle is worse than no series at all.",
            "",
            "| Metric | When | Labels | What it measures |",
            "|---|---|---|---|",
        ]
        for f in extra:
            labels = ", ".join(f"`{l}`" for l in sorted(f["labels"])) or "—"
            lines.append(
                f"| `{f['name']}` | {f['cond'] or '—'} | {labels} | {clean(f['help']).replace('|', '/') or '—'} |"
            )
    return "\n".join(lines).rstrip() + "\n"


# ---------------------------------------------------------------------------
# Checks
# ---------------------------------------------------------------------------


def check() -> int:
    failed = 0
    want = render_config()
    have = (DOCS / "config.md").read_text() if (DOCS / "config.md").exists() else ""
    if want != have:
        print("docs/reference/config.md is stale — run scripts/gen-reference.py config", file=sys.stderr)
        failed += 1
    else:
        print("reference: config.md is current")

    metrics_md = (DOCS / "metrics.md").read_text() if (DOCS / "metrics.md").exists() else ""
    missing = [
        n
        for n in metric_names_in_source()
        if f"`{n}`" not in metrics_md and f"`{n}_seconds`" not in metrics_md
    ]
    if missing:
        print(f"docs/reference/metrics.md is missing {missing} — re-scrape and run "
              "scripts/gen-reference.py metrics --scrape FILE", file=sys.stderr)
        failed += 1
    else:
        print("reference: every metric in the source is in metrics.md")

    # The hand-written format pages, held to the parsers they describe.
    schedules_rs = (ROOT / "crates" / "mesh-core" / "src" / "schedules.rs").read_text()
    header = re.search(r"pub struct ScheduleHeader \{(.*?)\n\}", schedules_rs, re.S).group(1)
    schedule_keys = re.findall(r"^\s*pub (\w+):", header, re.M)
    script_rs = (ROOT / "crates" / "mesh-core" / "src" / "script_spec.rs").read_text()
    definition_keys = re.findall(r'"(\w+)"', re.search(r"const DEFINITION_KEYS[^=]*=\s*&\[(.*?)\];", script_rs, re.S).group(1))
    directives = re.findall(r'^\s*"(\w+)"(?:\s*\|\s*"[\w-]+")*\s*=>\s*(?:spec|parse_needs|\{)', script_rs, re.M)
    for page, keys in [
        ("schedule-file.md", schedule_keys),
        ("script-definitions.md", definition_keys + directives),
    ]:
        text = (DOCS / page).read_text() if (DOCS / page).exists() else ""
        absent = sorted({k for k in keys if f"`{k}`" not in text})
        if absent:
            print(f"docs/reference/{page} does not describe {absent}", file=sys.stderr)
            failed += 1
        else:
            print(f"reference: {page} covers every key its parser accepts")

    env_md = (DOCS / "environment.md").read_text() if (DOCS / "environment.md").exists() else ""
    # A name is covered by being written out, or by a family it belongs to —
    # `MESHWARE_PORT_<LABEL>` covers `MESHWARE_PORT_`, and the prefix
    # constant the code spells `MESHWARE_TASK_`. A plain name covers only
    # itself: `MESHWARE_NODE` is not documentation for `MESHWARE_NODE_ID`.
    written = set(re.findall(r"`(MESHWARE_[A-Z0-9_]+)", env_md))
    families = {w for w in written if w.endswith("_")}

    def covered(n: str) -> bool:
        return n in written or any((n + "_").startswith(f) for f in families)

    unknown = sorted(n for n in public_env_names() if not covered(n))
    if unknown:
        print(f"docs/reference/environment.md does not mention {unknown}", file=sys.stderr)
        failed += 1
    else:
        print("reference: every public MESHWARE_* variable is in environment.md")
    return failed


# Families that are meshware's own bookkeeping, documented as families
# (`MESHWARE_TASK_*`), plus names only tests use. Anchored on the `_` so a
# family does not swallow a public name that merely starts the same way:
# `OUT` is a family and `MESHWARE_OUT` is a variable a map item reads.
INTERNAL_ENV = re.compile(
    r"^MESHWARE_((TASK|ACT|INV|PY|ANON|OUT|WF|TEST|STAGED)_|X$)"
)

# The test module, not the first `#[cfg(test)]`: a file with one test-only
# helper near the top would otherwise hide every name after it.
TEST_MODULE = re.compile(r"#\[cfg\(test\)\]\s*\n\s*mod \w+")


def public_env_names() -> set[str]:
    names = set()
    roots = [
        ROOT / "crates",
        ROOT / "apps" / "director" / "src-tauri" / "src",
        ROOT / "sdk" / "python" / "meshware",
    ]
    for root in roots:
        for path in list(root.rglob("*.rs")) + list(root.rglob("*.py")):
            if "/tests/" in str(path) or path.name.startswith("test_"):
                continue
            text = path.read_text()
            cut = TEST_MODULE.search(text) if path.suffix == ".rs" else None
            body = text if cut is None else text[: cut.start()]
            for m in re.finditer(r'"(MESHWARE_[A-Z0-9_]+)', body):
                name = m.group(1).rstrip("_")
                if not INTERNAL_ENV.match(name) and "SOMETHING_FUTURE" not in name:
                    names.add(name)
    return names


def main(argv: list[str]) -> int:
    if "--check" in argv:
        return check()
    if len(argv) < 2:
        print(__doc__)
        return 2
    what = argv[1]
    DOCS.mkdir(parents=True, exist_ok=True)
    if what == "config":
        (DOCS / "config.md").write_text(render_config())
        print("docs/reference/config.md")
        return 0
    if what == "metrics":
        if "--scrape" not in argv:
            print("metrics needs --scrape FILE: `curl -s localhost:7776/metrics > FILE` on a node", file=sys.stderr)
            return 2
        scrape = Path(argv[argv.index("--scrape") + 1]).read_text()
        (DOCS / "metrics.md").write_text(render_metrics(scrape))
        print("docs/reference/metrics.md")
        return 0
    print(f"unknown: {what}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
