# What to put in every Director field

## Adding them, with their arguments

**Scripts → + Add a script.** Three steps: *Source*, *File*, *Details*.

1. **Source** — pick the folder or repo the scripts are in. Not there yet?
   **Add a folder** (a native picker) or **Connect a repo**. You can also just
   **drop the `.sh` file onto the window** and skip to step 3.
2. **File** — pick e.g. `transcode-media.sh`.
3. **Details** — the Name, Description and **every argument are already
   filled in**, read from `transcode-media.sh.definitions`. A line under the
   Arguments heading says where they came from:

   > Read from `transcode-media.sh.definitions`. Each item of a per-item run
   > fills in `--path`.

   Check them and press **Save new version**.

### Where the arguments come from

Three places, in this order. The wizard always says which one it used, because
they are three quite different levels of confidence.

| It says | It means |
|---|---|
| *"Read from `NAME.definitions`"* | the arguments are the author's, written down deliberately — trust them |
| *"Read from the script's own `#meshware:` header"* | the same, in the older one-line form |
| *"N of these were inferred by reading the script"* | nobody wrote them down; these were read out of the flags the script parses, so **check them** |
| *"This script declares no arguments"* | the Run screen will have no fields — nothing was declared and nothing could be read |

A script with both a definitions file and a header is described by the
definitions file wherever the two disagree; anything the JSON does not mention
still comes from the header, so adding one never silently deletes the other.

### The definitions file

Every script in this folder has one — `burn-in.sh` and
`burn-in.sh.definitions` — and it is plain JSON:

```json
{
  "name": "transcode-media",
  "about": "Re-encode one video. One item per file.",
  "item": "path",
  "needs": { "cores": 2, "mem": "512M" },
  "tools": ["ffmpeg"],
  "reads": ["$path"],
  "writes": ["$outdir"],
  "args": [
    { "name": "path", "type": "text", "required": true,
      "description": "The video to encode" },
    { "name": "crf", "type": "int", "default": 28, "min": 0, "max": 51,
      "description": "Quality — lower is bigger" },
    { "name": "codec", "type": "choice",
      "choices": ["libx265", "libx264"], "default": "libx265" }
  ]
}
```

`type` is one of `text`, `int`, `number`, `bool`, `choice`. Everything else is
optional — `{ "args": [] }` is a complete file. `min`, `max` and `pattern`
become real constraints on the Run screen, which is the thing a one-line
`#meshware:` comment had nowhere to put.

**You can edit it in Director.** On the Details step, open **Definitions
file**. *Use this — fill the fields above* compiles the JSON into the argument
rows; *Take the fields above* goes the other way; **Save to disk** writes it
back beside the script. A script that has no definitions file yet gets one
generated from whatever it does declare, so *Write the file* is the whole
upgrade path from a header — or from a script that declares nothing at all.

Saving needs Director to know where the source lives on disk, which it does
for a folder you added (or a file you dropped) and not for a Git repo — for a
repo, copy the JSON into the file yourself and commit it.

> **The same thing from a terminal**, if you prefer: `meshnode add
> ./transcode-media.sh`, and `meshnode script definitions ./thing.sh` to write
> the companion file. Same parser either way, so a script added in Director
> and one added from a terminal register identically.

---



Field names below are exactly what is on the screen. Anything not mentioned
can be left alone.

**The check that saves you every time** is the sentence at the top of the Run
page. Before pressing Run it should read something like:

> Run **transcode-media** once for **each of 412 items**, filling in
> **--path**, spread across **any node with room**, **2 cores each**, writing
> results to **/srv/media/encoded**.

Every bold word is a control. If one is wrong, click it. If it says *"once on
every matching node — the same job on each"* while you meant a list of files,
you are on the wrong tab and the sentence is telling you so.

---

## burn-in — **Together**

### 1 · What do you want to run?
*A saved script* → **burn-in**. Its three arguments appear:

| Field | Put | Why |
|---|---|---|
| `seconds` | **120** | 60 finds throttling; 30 minutes finds cooling and power problems |
| `workers` | **0** | one per hardware thread |
| `align` | **5** | leave it — members wait for the next multiple of 5 on the wall clock |

### 2 · How many machines, together?

| Field | Put | Why |
|---|---|---|
| **Gang size** | all of your machines | a burn-in that is not everything is not a burn-in |
| **Layout** | **spread — one per node** | ⚠️ the default is `pack` |
| **Restart if a member dies** | **unticked** | a member dying should end this run and tell you |

> **`pack` is the trap.** It packs the gang onto the fewest machines, so a
> six-node burn-in can land entirely on two boxes and test nothing about the
> room. The line under the field says which you chose — read it.

### 3 · Where should it run?
*Anywhere — the mesh picks a free node*. **Options**: leave closed.

**What you are testing:** a gang running a *saved script*. Until this release
Together accepted commands only, so the things people actually run as a gang —
published, versioned, reviewed — were the things it could not run.

---

## transcode-media — **Per item**

### 1 · What do you want to run?
*A saved script* → **transcode-media**.

| Field | Put |
|---|---|
| `path` | **blank** — the item fills this in |
| `codec` | `libx265` |
| `crf` | `28` |
| `preset` | `medium` |
| `outdir` | your shared output directory, e.g. `/srv/media/encoded` |
| **One item per line** | paste the output of `find /srv/media -name '*.mkv'` |
| **Each item becomes** | **path** — should already say so |

### 2 · Where should it run?
*Anywhere — the mesh picks a free node*.

### Options → What one item needs

| Field | Put |
|---|---|
| **Cores** | **2** — already filled in, because the script declared it |
| Memory / Scratch disk / Items per chunk | blank |

### Options → Output
Leave **"Keep each item's output in the mesh"** **unticked** when `outdir` is
set. Tick it only if you left `outdir` blank — then press **Save results…**
when the run finishes. Results no longer expire while you decide.

**What you are testing:** contracts from the script's own header, the item
input, cores you did not have to type, sandbox grants for `outdir` with
nothing added to any `meshnode.toml`, the ffmpeg check, and the destination
appearing in the sentence.

---

## archive-check — **Per item**

| Field | Put |
|---|---|
| `path` | **blank** |
| `hash` | `sha256` |
| `max_age_days` | `0`, or a number to catch a backup job that quietly stopped |
| **One item per line** | `find /srv/backup -type f` |
| **Each item becomes** | **path** |
| Options → **Cores** | **1** |
| Options → **Stop after failures** | **5%** — the circuit breaker |
| Options → Output | **unticked** — the answers are text |

**What you are testing:** the breaker, and a failure that arrives as a
*finding* rather than a crash. Drop a text file named `something.tar.gz` into
the directory: it is caught by its bytes, not its name.

---

## log-scan — **Per item**, with a merge

| Field | Put |
|---|---|
| `path` | **blank** |
| `patterns` | `ERROR,WARN,timeout` |
| `ignore_case` | `yes` |
| **One item per line** | `find /var/log -name '*.gz'` |
| **Each item becomes** | **path** |
| Options → **Cores** | **1** |
| Options → Output | ✅ **tick** "Keep each item's output in the mesh" |

Ticking it reveals **Then merge them with** — paste this on one line:

```
grep -h "^{" "$MESHWARE_INPUTS"/* | jq -s "{files: length, lines: (map(.lines)|add), counts: ([.[].counts|to_entries[]]|group_by(.key)|map({key:.[0].key, value:(map(.value)|add)})|from_entries)}"
```

Leave **Merge on** blank: it runs where most of the output already is, which
is the least data moved.

**What you are testing:** the reduce — and the bug that used to break it. `{}`
was substituted inside the merge command, so an ordinary jq expression came
back with a job id spliced into the middle of it.

---

## fleet-audit — **Once**

| Field | Put |
|---|---|
| `min_disk` / `min_mem` / `check_path` | `15` / `10` / `/` |
| **Where should it run?** | *Anywhere — the mesh picks a free node* |
| the **on** dropdown beneath it | **all N of them** |

That second dropdown is the fan-out. Without it you audit one machine and
learn nothing about the others.

**What you are testing:** rows rather than scrollback, and the same run as a
gate. On the CLI:

```console
$ meshnode service run fleet-audit --fanout all --json \
      --expect 'equals(compliant, "yes")'
```

Exit 1 and the names of the machines that failed. Run it twice and Director
diffs the two.

---

## Two behaviours to watch for

**The shape infers itself.** Paste a list into **One item per line** while on
the **Once** tab and it moves you to **Per item** by itself, adding
*"(because you gave it a list)"* to the sentence. Clicking that undoes it, and
the undo sticks.

**Skipped machines say why.** A node without ffmpeg, or one that cannot see
your media, is refused *before* dispatch and named:

```
! rack-3: ffmpeg is not installed
! rack-4: cannot see the data
```

If every candidate is refused, the run does not start and the error names both
fixes — mount it, or raise `[compute] stage_max_mb` so the bytes are sent
instead.
