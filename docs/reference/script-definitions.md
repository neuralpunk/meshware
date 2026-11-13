# Script definitions reference

A **script** is a file — a shell script, a Python program, an Ansible playbook — that you add
to the mesh once and anybody allowed can then run by name, anywhere, with its arguments filled
in by a form. What a script says about itself — its name, its arguments, what one run needs —
decides what that form shows and how the mesh runs it.

```console
$ meshnode add ./transcode-media.sh                  # add it (or: drop the file on Director)
$ meshnode script inspect ./transcode-media.sh       # what it would register, adding nothing
$ meshnode script definitions ./transcode-media.sh   # write its definitions file for you
```

A script can describe itself in two ways, and a script with neither still works:

1. **A definitions file** beside it — `transcode-media.sh.definitions`, JSON. It has room for
   bounds, choices and run settings, and it is what Director writes.
2. **A `#meshware:` header** in the script's own comments. One line per fact; it travels inside
   the file.

With neither, meshware **infers** arguments from the flags the script visibly parses, and marks
every one of them *inferred* wherever it is shown — check them. Both declared forms can be used
together; the definitions file wins where they overlap.

---

## The definitions file

```json
{
  "name": "transcode-media",
  "about": "Re-encode one video. One item per file.",
  "item": "path",
  "needs": { "cores": 2 },
  "tools": ["ffmpeg"],
  "reads": ["$path"],
  "writes": ["$outdir"],
  "args": [
    { "name": "codec", "type": "choice", "choices": ["libx265", "libx264"],
      "default": "libx265", "description": "Video encoder" },
    { "name": "crf", "type": "int", "default": 28, "min": 0, "max": 51,
      "description": "Quality — lower is bigger" },
    { "name": "path", "type": "text", "required": true,
      "description": "The video to encode" }
  ]
}
```

| Key | Type | What it does |
|---|---|---|
| `name` | text | The name it registers as. Default: the file name without its extension |
| `about` | text | One line for people, shown under its name. `description` is accepted too |
| `item` | argument name | For a **per-item** run: which argument each item of the list fills in. `each` and `per_item` are accepted too |
| `needs` | object | What **one** run (one item) needs: `cores`, `mem` (`4G`, `512M`), `disk`. The Run screen fills these in, and a machine without them is not used. `cores`, `cpus`, `mem`, `memory` and `disk` are also accepted at the top level |
| `tools` | list of commands | Programs the script needs on the machine: `["ffmpeg", "ffprobe"]`. A machine without one is refused **before** dispatch, by name. `needs_tools` is accepted too |
| `reads` | list of paths | What the script reads, so its sandbox can allow it. `$name` refers to an argument's value |
| `writes` | list of paths | What the script writes, likewise |
| `args` | list | Its arguments — below. `"args": []` says *this script takes nothing* |
| `run` | object | How it was last set up to run — below. Optional |

Keys this version does not know are kept, and written back unchanged, rather than refused — so
a file written for a later version still registers here.

### Each argument

| Key | Type | What it does |
|---|---|---|
| `name` **required** | text | The argument's name: letters, digits, `-`, `_` |
| `type` | `text` · `int` · `number` · `bool` · `choice` | What it takes. Default `text` |
| `choices` | list | For `choice`: the values it may take |
| `required` | true / false | The run is refused without it |
| `default` | value | Used when the person running it gives none |
| `min`, `max` | number | Bounds, for `int` and `number` |
| `pattern` | regular expression | For `text`: what a value must match |
| `description` | text | Shown beside the field |

### The `run` block

Remembered run settings, so the second person to run a script does not re-derive how it wants
to be run. Every key is optional, and a missing key means *not remembered*, never *off*.

| Key | What it remembers |
|---|---|
| `mode` | `once`, `peritem` or `together` — the three shapes of Run › New run |
| `on` | Where: `any`, `name=…`, `tag=…`, or a selector |
| `spread` | `one` (the mesh picks), `all` (every match) or `some` (the best `count`) |
| `count` | How many, with `spread = "some"` |
| `batch` | At most this many at a time |
| `halt_on_fail` | Stop after this many failures: `3` or `10%` |
| `priority` | `low`, `normal` or `high` |
| `timeout` | A time limit: `30s`, `5m`, `2h` |
| `item_input` | Which argument each item fills, when it differs from `item` |
| `chunk` | Items per chunk, for a per-item run |
| `gang`, `gang_placement`, `gang_restart` | For a *together* run: how many machines, `pack` or `spread`, and whether to re-run them all if one dies |
| `gpus`, `vram`, `device`, `gpu_share` | Accelerators: how many, memory on each (`24G`), a model to match, and whether a card may be shared |
| `keep_output`, `reduce`, `reduce_on` | Leave each item's output where it was made, and the command that merges it |
| `capture`, `expect` | Collect every target's answer, and an expectation checked row by row |
| `env` | Environment variables for the run |
| `inputs` | Inputs the arguments do not declare, passed anyway |

## The `#meshware:` header

The same facts, one per comment line, anywhere near the top of the script:

```bash
#!/usr/bin/env bash
#meshware: name transcode-media
#meshware: about Re-encode one media file
#meshware: item path
#meshware: needs cores=2 mem=4G
#meshware: tools ffmpeg, ffprobe
#meshware: reads $path
#meshware: writes $outdir
#meshware: arg path!                                      "The file to encode"
#meshware: arg codec : one of libx265|libx264 = libx265   "Video encoder"
#meshware: arg crf : int = 28                             "Lower is bigger"
#meshware: arg strip : bool = true                        "Remove metadata"
```

| Directive | Takes |
|---|---|
| `name` | The name it registers as |
| `about` (or `description`) | One line for people |
| `item` | The argument each item fills in |
| `needs` | `cores=N`, `mem=SIZE`, `disk=SIZE`, space-separated |
| `tools` | Commands it needs, separated by commas or spaces |
| `reads`, `writes` | One path per line; repeat the directive for more |
| `arg` | One argument: `NAME[!] [: TYPE] [= DEFAULT] ["description"]` |

In an `arg` line, `!` after the name makes it required; the type is `int`, `number`, `bool`,
`text`, or `one of a|b|c`; the description is quoted and comes last. A type it does not
recognise becomes `text` rather than a refusal. Unknown directives are kept and ignored.

## How the script receives its arguments

Every argument arrives on the command line as a `--name value` pair:

```console
./transcode-media.sh --codec libx265 --crf 28 --path /srv/media/a.mkv
```

So the script parses its own flags, the way it would if a person ran it, and should not depend
on their order. A per-item run adds
the item as the argument `item` names, and the item is also in `$MESHWARE_ITEM`; see the
[environment reference](environment.md) for everything else a job can read.
