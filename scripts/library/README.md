# Five workloads, and what each one is testing

Each script carries its own arguments in a companion `.definitions` file —
`burn-in.sh` and `burn-in.sh.definitions` — so adding it takes no installer,
no hand-written JSON Schema and no `service create`, whichever way you add it.
(A `#meshware:` header inside the script works too, and still does; the
definitions file is what these five use, because it has room for the bounds
and choices a one-line comment does not.)

## Adding them in Director

**Scripts → + Add a script.** Three steps: *Source*, *File*, *Details*.

1. **Source** — the folder or repo the scripts are in. Not there yet?
   **Add a folder** (a native picker) or **Connect a repo**. You can also
   **drop the `.sh` file straight onto the window** and skip to step 3.
2. **File** — pick e.g. `transcode-media.sh`.
3. **Details** — Name, Description and **every argument are already filled
   in**, read from the script's definitions file. A line under the Arguments
   heading says where they came from:

   > Read from `transcode-media.sh.definitions`. Each item of a per-item run
   > fills in `--path`.

   Check them, press **Save new version**. Open **Definitions file** on the
   same step to see or edit the JSON, and to save it back beside the script.

Four things it will tell you rather than let you find out later:

| It says | It means |
|---|---|
| *"Read from `NAME.definitions`"* | the arguments are the author's, and can be trusted |
| *"Read from the script's own `#meshware:` header"* | the same, in the older one-line form |
| *"N of these were inferred by reading the script"* | nobody declared them; these were read out of the flags the script parses, so **check them** |
| *"This script declares no arguments"* | the Run screen for it will have no fields — nothing is declared |

## Adding them from a terminal

```console
$ for s in transcode-media archive-check log-scan fleet-audit burn-in; do
    meshnode add scripts/library/$s.sh
  done
added transcode-media (version 1, bash)
  5 argument(s), from transcode-media.sh.definitions
  per item: each item fills in --path
```

`meshnode script inspect <file>` shows what would be registered without
registering anything, and `meshnode script definitions <file>` writes the
companion file from whatever the script already declares — the upgrade path
for a script that has only a header, or nothing at all.

Both routes use the same parser, so a script added in Director and the same
file added with `meshnode add` register identically.

**Then what?** Every Director field, per script, is in
[`DIRECTOR.md`](DIRECTOR.md) — including the one setting on the Together tab
that is wrong by default.

---

## What to run, and what it is proving

| Script | Tab | What it does | What it is testing |
|---|---|---|---|
| `transcode-media` | **Per item** | Re-encodes a folder of video | Contracts, per-item shape, cores from the script, sandbox grants, tool check, output destination |
| `archive-check` | **Per item** | Verifies backups are really intact | Circuit breaker, a failure that is a *finding* rather than a crash |
| `log-scan` | **Per item** + merge | Counts patterns across a log estate | The reduce, and the `{}` bug that used to break it |
| `fleet-audit` | **Once**, all nodes | Health of every machine | Fan-out, rows, `--expect` as a pipeline gate |
| `burn-in` | **Together** | Every machine flat out in the same second | A gang running a *saved script* — impossible before |

### 1 · transcode-media

```console
$ find /srv/media -name '*.mkv' > media.txt
$ meshnode map -a media.txt --service transcode-media --item-input path \
      --set crf=28 --set outdir=/srv/media/encoded --cpus 2
```

In Director: **Run → Per item**, pick it, paste the list. Watch for:

- the plan sentence naming the shape, the argument and the destination;
- `2 cores each` filled in without you typing it — the script declared it;
- a node without ffmpeg refused **before** dispatch, by name;
- the output directory written to with **nothing added to any
  `meshnode.toml`**. If a machine cannot see the source files, the bytes are
  sent to it instead, up to `[compute] stage_max_mb`.

Leave `outdir` blank to keep results on the machines that made them —
then **Save results…** in Director, or `--out DIR` on the CLI. They no longer
expire while you decide.

### 2 · archive-check

```console
$ find /srv/backup -type f > backups.txt
$ meshnode parallel -a backups.txt --service archive-check --item-input path \
      --set hash=sha256 --halt-on-fail 5%
```

It picks the verifier from the file's **bytes**, not its name, so a
`.tar.gz` that is really something else is caught. Put a text file called
`something.tar.gz` in the directory to see it.

### 3 · log-scan

```console
$ find /var/log -name '*.gz' > logs.txt
$ meshnode map -a logs.txt --service log-scan --item-input path \
      --set patterns='ERROR,WARN,timeout' \
      --reduce 'grep -h "^{" "$MESHWARE_INPUTS"/* | jq -s "{files: length, lines: (map(.lines)|add), counts: ([.[].counts|to_entries[]]|group_by(.key)|map({key:.[0].key, value:(map(.value)|add)})|from_entries)}"'
```

Terabytes in, a few lines out. The merge runs on whichever node already holds
most of the output.

### 4 · fleet-audit

```console
$ meshnode service run fleet-audit --fanout all --json
$ meshnode service run fleet-audit --fanout all --json \
      --expect 'equals(compliant, "yes")'      # exit 1 and the names, for CI
```

Run it twice and Director will diff the two.

### 5 · burn-in

```console
$ meshnode service run burn-in --gang 3 --gang-placement spread --set seconds=120
```

Every machine starts in the same second, so this tests what they *share* —
the circuit, the UPS, the room's air. A machine that quietly throttles shows
up as a low `rate_per_worker` beside its siblings. Use `spread` so members
land on different machines; `pack` is the default and is wrong here.

---

## Things worth deliberately breaking

- **Point `outdir` somewhere no machine can write.** The run should say which
  machines and why, not fail per item.
- **Ask for a tool nobody has** — add `"tools": ["nonesuch"]` and re-add.
  Refused before dispatch, naming the tool.
- **Unmount the share on one machine.** That node should be skipped with
  `cannot see the data`, and the run should still finish on the others.
- **Pick a folder only one machine can see and force it onto another** with
  `--on <that node>`. Under `stage_max_mb` the bytes are sent; over it you get
  a refusal naming both fixes.
- **Run a transcode, tick "keep output in the mesh", and walk away.** The
  results should still be there — `meshnode objects` marks them
  *waiting for you*.
