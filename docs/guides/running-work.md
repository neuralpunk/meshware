# Running work

A **job** is one command, run once. You say what to run and where; Meshware picks the machine,
sends the command there, and streams the output back to you as it happens. When the job ends,
its output and exit code are kept, so you can look at them later.

## Run a command

```console
$ meshnode run 'df -h /' --on any
→ running on beta (job-agqljao2xx)
Filesystem      Size  Used Avail Use% Mounted on
/dev/nvme0n1p3  476G  201G  275G  43% /
← succeeded
```

`meshnode run` waits for the job and exits with the command's own exit code, so it fits into a
script like any other command.

**In Director:** *Run › New run*. Type the command, choose where, and press **Run**. The output
appears live on the right.

## Choose where it runs

`--on` says which machines may take the job. Without `--fanout`, it runs on **one** of them: the
one with the most room.

| To run… | Use |
|---|---|
| On any machine | `--on any` |
| On one machine, by name | `--on web-01` |
| On a machine with a tag | `--on tag=gpu` |
| On a machine with either tag | `--on 'tag=gpu\|tag=cuda'` |
| On a machine with both tags | `--on 'tag=gpu,tag=dc=east'` |

Tags are labels you give machines, such as `gpu` or `dc=east`:
[Adding and managing machines](machines.md#tag-machines) says how.

```console
$ meshnode run 'nvidia-smi' --on tag=gpu
```

## Run on many machines at once

`--fanout all` runs the command on **every** matching machine, as one job with a result for each:

```console
$ meshnode run 'uptime' --fanout all
$ meshnode run 'systemctl is-active nginx' --on tag=web --fanout all
$ meshnode run './smoke-test.sh' --on tag=web --fanout 3      # on three of them
```

Each line of output is prefixed with the machine it came from, and at the end you get one line
saying whether every machine succeeded.

To keep each machine's answer as a row you can look at again, add `--json`. Add `--expect` to check
every answer and fail if any does not match:

```console
$ meshnode run 'sshd -T' --fanout all --expect 'contains(stdout, "permitrootlogin no")'
$ meshnode history results fan-agqljao25j          # one row per machine, by the id run printed
$ meshnode history results fan-agqljao25j --failed # only the machines where it failed
```

Two options help with a large fleet: `--max-parallel 10` runs at most ten at a time, and
`--halt-on-fail 5%` stops starting new ones when more than 5% have failed.

## Give it what it needs

```console
$ meshnode run './train.py' --on tag=gpu --gpus 1 --cpus 8 --mem 32G
$ meshnode run './deploy.sh' --env TARGET=staging --env 'TOKEN={{secret:deploy-token}}'
$ meshnode run './long-job.sh' --timeout 3600
```

- `--cpus`, `--mem` and `--gpus` choose a machine with that much free, and hold the job to it
  where the machine can enforce it.
- `--env NAME=VALUE` sets an environment variable. `{{secret:NAME}}` puts in a secret, opened only
  on the machine that runs the job: [Secrets](secrets.md).
- `--timeout` stops the job after that many seconds.

Check where a job would go without running it:

```console
$ meshnode run 'make test' --on tag=build --dry-run
$ meshnode plan --on tag=gpu --gpus 2         # where would a two-GPU job land, and when
```

## Watch, wait and cancel

A job keeps running if you close your terminal. From anywhere in the mesh:

```console
$ meshnode jobs                      # recent jobs, newest first
$ meshnode jobs logs job-agqljao2xx  # its output
$ meshnode jobs wait job-agqljao2xx  # wait for it, then exit with its code
$ meshnode jobs cancel job-agqljao2xx
$ meshnode history jobs              # finished jobs, searchable
```

> [!NOTE]
> **`meshnode history` is the one verb for anything that already happened** —
> `history jobs` to search, `history results` for a fan-out's rows,
> `history diff` to compare two runs, `history replay` to read an old job's
> output back from wherever a sink put it, and `history stats` for how far
> back this machine goes. `meshnode jobs` is the live view; `history` is the
> record. (`meshnode results` still works and is no longer advertised.)

`meshnode jobs wait … --notify` sends a desktop notification when it ends. Every one of these
takes enough of the start of a job's id to be unique.

**In Director:** *Run › Activity* lists every job, live. Select one to see its output, where it
ran and why, and to cancel it.

## One command, many items

To process a list of things — files, hosts, numbers — with the work spread across every machine
that has room:

```console
$ meshnode parallel 'gzip -t {}' ::: logs/*.gz
$ seq 1 1000 | meshnode parallel 'process {}'
```

`{}` is replaced by each item. Machines that finish early take more items, and an item whose
machine stops responding is run again elsewhere. `meshnode map` does the same and keeps each
result on the machine that made it, for large outputs:
`meshnode map 'gzip -dc {} | grep -c ERROR' ::: logs/*.gz`.

## Compiling

A build is the one workload with a shape of its own, and it has a wrapper: put `meshnode cc` in
front of your compiler and each file is built on whichever machine has room, and any file the mesh
has already built is not built again. [Compiling across the mesh](compiling.md).

## Saved scripts

Scripts you run often can be kept in the mesh and run by name, with their inputs described.

**In Director:** *Run › Scripts* lists them. Choose one, fill in its inputs, and run it. Drop a
script file onto the window to add one. [Script definitions](../reference/script-definitions.md)
describes the file that says what inputs a script takes.

## Things that keep running, or run on a clock

A job ends. For something that should stay up — a web server, a worker — use a
[deployment](../deployments.md). For something that runs every hour or every night, use a
[schedule](../deployments.md#6-something-once-on-a-clock-or-on-every-machine).

## When a job will not run

```console
$ meshnode jobs explain job-agqljao2xx   # why it went where it went, or nowhere
$ meshnode queue                         # what is waiting on this machine, and why
$ meshnode runtimes                      # what this machine can run
```

The usual reasons: no machine matches `--on`; none has the CPUs, memory or GPUs asked for; or the
job needs something, such as Python or a container engine, that no matching machine has.
[Troubleshooting](../operating/troubleshooting.md#my-job-will-not-schedule) goes further.
