# Schedule file reference

A **schedule** runs a published script or service on its own, on an interval or at a time of
day, with nobody watching. Every key a schedule file accepts is below. For how schedules fit
with everything else, see [Deployments §6](../deployments.md#6-something-once-on-a-clock-or-on-every-machine).

```toml
# nightly-backup.toml
[schedule]
name     = "nightly-backup"
service  = "backup@4"
at       = "03:00"
days     = "mon-fri"
tz       = "+02:00"
targets  = "tag=db"
timeout  = "45m"
catch_up = "once"
```

```console
$ meshnode schedule put --dry-run nightly-backup.toml   # when it would next fire, and where
$ meshnode schedule put nightly-backup.toml             # publish, or replace, it
$ meshnode schedule ls                                  # last and next firing of each
$ meshnode schedule show nightly-backup
$ meshnode schedule pause nightly-backup                # keep it, stop it firing
$ meshnode schedule resume nightly-backup
$ meshnode schedule rm nightly-backup                   # jobs it already fired are untouched
```

In Director: **Services › Schedules › New schedule**, which takes the same file.

TOML, YAML and JSON all work, with the same keys, told apart the way a
[deployment file](deployment-file.md#yaml-toml-or-json) is. In YAML the table is a top-level
`schedule:` mapping. **An unknown key is refused**: a typo in something that runs unattended
at 03:00 is a schedule that silently does the wrong thing.

---

## What it runs

| Key | Type | Default | What it does |
|---|---|---|---|
| `name` **required** | text | — | The schedule's handle, in every command and in Director |
| `service` **required** | `name` or `name@version` | — | The published script or service it fires. `backup@4` pins a version; `backup` alone runs whatever version is current when it fires — a floating head is allowed here, because a nightly job is a thing that happens once and may be whatever the latest is |
| `inputs` | table | none | Arguments for the service, as its contract declares them. Numbers and booleans are passed as text |
| `targets` | selector | `any` | Which machines may run it, in the [selector grammar](deployment-file.md#where-and-how-many). Each firing runs **one job**, on whichever matching machine the mesh picks |
| `priority` | `low` · `normal` · `high` | `normal` | Its place in a busy machine's queue |
| `timeout` | duration | none | Stop the job if it runs longer than this: `45m`, `2h` |
| `description` | text | empty | A sentence for people |
| `paused` | true / false | `false` | Published, but not firing. `meshnode schedule pause` and `resume` do the same without editing the file |

## When it fires

A schedule has exactly one of `every` and `at`.

| Key | Type | Default | What it does |
|---|---|---|---|
| `every` | duration | — | An interval, anchored on the Unix epoch: `15m`, `2h`, `1d`. At least a minute |
| `at` | `HH:MM` | — | A time of day, 24-hour: `03:00`, `18:30` |
| `days` | days | every day | With `at` only: `mon-fri`, `sat,sun`, `mon,wed,fri`, or `*` |
| `tz` | fixed offset | `UTC` | With `at` only: `+02:00`, `-0500` or `UTC`. **Never a zone name** — a schedule does not move with daylight saving time, because a production job that silently shifts by an hour twice a year is worse than one you adjust yourself |

Schedules fire **at most once a minute** — cron's own resolution, and every firing costs a job
record on every node.

## When a firing is missed

A firing is *missed* when no machine that could fire it was up at the time — a mesh that was
down overnight, say.

| Key | Type | Default | What it does |
|---|---|---|---|
| `catch_up` | `skip` · `once` | `skip` | `skip` forgets missed firings. `once` runs **one** of them when the mesh is back — never a backlog: a mesh down for a weekend does not wake up and run 192 backups |
| `grace` | duration | `1h` | With `catch_up = "once"`: how late a missed firing may still run |
| `overlap` | `skip` · `allow` | `skip` | Whether to fire while the previous firing's job is still running. `skip` waits for the next time |

## Who fires it

No one machine owns a schedule. Whichever live machine the mesh elects for its **name** fires
it, recomputed every tick, so a schedule outlives the laptop that created it. If two machines
both fire the same occurrence, they converge on one job: the job's id is derived from the
schedule and the occurrence.

A schedule whose `service` has been removed is **orphaned**, and says so in red in Director
and in `meshnode schedule ls` — it has quietly stopped doing anything, which is the one failure
a schedule has.

## When it is refused

| The message says | Change this |
|---|---|
| `a schedule needs a \`service\`` | Name the script or service to run |
| `a schedule declares either \`every\` … or \`at\` …, not both` | Keep one |
| `a schedule needs \`every = "15m"\` or \`at = "03:00"\`` | Add one |
| `\`days\` and \`tz\` narrow a time of day, so they need \`at\`, not \`every\`` | Remove them, or use `at` |
| `a schedule may fire at most once every 60s` | A longer interval |
| `\`at = …\` is not a time of day like "03:00"` | `HH:MM`, 24-hour |
| an unknown `catch_up`, `overlap` or `priority` | One of the words above, exactly |
