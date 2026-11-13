# Deployment file reference

Every key a deployment file accepts, what it defaults to, and the mistake each one is there to
catch. For a walkthrough, start with [Deployments](../deployments.md).

A deployment file says three things: **what to run**, **where and how many copies**, and **how a
change rolls out**. Everything past the name and the thing to run is optional and has a default, so
the smallest file that works is four lines long:

```yaml
deployment:
  name: hello
  container:
    image: docker.io/library/nginx:alpine
```

That runs one copy on any machine that can run a container, and restarts it if it dies. It has no
port, no name on the network and no health check; the rest of this page is how to ask for those.

> [!IMPORTANT]
> **A typo is an error, not a no-op.** The parser refuses any key it does not recognise, at every
> level: `max_unavailble: 2` stops the publish, names the key, and lists the keys allowed there. A
> deployment runs with nobody watching, and a misspelling that looked like it worked would stay
> wrong forever.

---

## From a blank page to running

1. **Start from a file that works.**
   `meshnode deployment init --name web --image docker.io/library/nginx:alpine > web.yaml` prints a
   commented file with every block a first deployment usually wants. Director's **Deploy an
   example** opens the same file in its editor.
2. **Check it without a mesh.** `meshnode deployment validate web.yaml` parses the file and applies
   every rule that does not need machines to answer — the refusals listed at
   [the end of this page](#when-it-is-refused). It works offline and exits non-zero on the first
   problem, so it can run in CI.
3. **Ask the mesh what would happen.** `meshnode deployment put --dry-run web.yaml` reports how many
   machines match and whether anything can take it. For a change to a published deployment, it
   lists every field that differs and whether that field restarts copies.
4. **Publish.** `meshnode deployment put web.yaml` publishes and follows the rollout until it
   settles; add `--detach` to return at once. `meshnode deployment show web --output toml` reads a
   published deployment back as a file.

## Autocomplete in your editor

`meshnode deployment schema` prints a JSON Schema for the file. Save it next to your deployments and
point your editor at it: VS Code with the Red Hat YAML extension, and any other editor built on
`yaml-language-server`, then completes keys, shows each key's description on hover, and underlines
unknown keys as you type. Director's editor has **Copy JSON Schema** for the same file.

```console
$ meshnode deployment schema > deployment.schema.json

# then make this the first line of web.yaml:
# yaml-language-server: $schema=./deployment.schema.json
```

The schema knows every key and its type. It does not know the rules that connect two keys — a
static port with more than one copy, a name registered with no port to publish — so still run
`validate`.

## The whole shape on one page

Every key, with its default where it has one. This is a map, not a file to publish: it shows both
ways of saying what runs, and every option at once.

```yaml
deployment:
  name: web                          # required
  description: ""
  paused: false

  # What runs — exactly one of these two
  service: api@3                     # a published service, pinned to a version
  container:                         # …or an image, described inline
    image: docker.io/library/nginx:alpine   # required in this block
    command: /usr/sbin/nginx         # default: what the image runs
    args: ["-g", "daemon off;"]
    working_dir: /usr/share/nginx/html
    env:
      LOG_LEVEL: info
  inputs:                            # arguments for a published service
    region: eu-west

  # Where, and how many
  count: 1                           # a number, or system
  targets: any

  network:
    mode: host
    port:
      - label: http                  # required
        to: 80
        static: 8080                 # default: a free port from the machine's range

  register:
    name: web                        # required in this block
    tags: [v2]
    port: http                       # default: every declared port
    shutdown_delay: 10s
    kill_timeout: 30s

  health:
    kind: http                       # required: exec, tcp or http
    port: http
    path: /
    command: "true"                  # exec only
    interval: 10s
    grace: 30s
    timeout: 5s
    failures: 3

  restart:
    backoff: 1s..5m
    budget: 5 in 10m

  update:
    strategy: rolling
    max_unavailable: 1
    gate: health
    canary: 0
    auto_promote: true
    auto_revert: false
    progress_deadline: 10m           # default: none, or 10m with canary / auto_revert
    min_healthy_time: 0s

  disconnect:
    lost_after: 60s                  # default: 60s for a number, never for system
    replace: false
    reconcile: keep_original

  volume:
    - name: data                     # required
      path: /var/lib/data
      mode: rw

  resources:
    cpus: 1
    memory_mb: 256
    disk_mb: 1024

  # Which machines it prefers
  distinct_hosts: false
  distinct_property:
    attribute: tag.rack              # required in this block
    limit: 1
  affinity:
    - select: tag=ssd                # required
      weight: 50
  spread:
    attribute: tag.site              # required in this block
    targets: { home: 60 }            # default: an even share
  priority: 50

  migrate:
    max_parallel: 1
    min_healthy_time: 10s
    healthy_deadline: 5m
```

## YAML, TOML or JSON

All three take the same keys. The start of the file decides which it is: a file beginning with `{`
is JSON; one whose first line that is not a comment contains `=` (and does not end in `:`) is TOML;
anything else is YAML. This page writes YAML. In TOML, each block is a table under `deployment`,
and a list of blocks is an array of tables:

| YAML | TOML |
|---|---|
| `deployment:` then `name: web` | `[deployment]` then `name = "web"` |
| `update:` then `canary: 1` | `[deployment.update]` then `canary = 1` |
| `container:` → `env:` → `LOG_LEVEL: info` | `[deployment.container.env]` then `LOG_LEVEL = "info"` |
| `network:` → `port:` → `- label: http` | `[[deployment.network.port]]` then `label = "http"`, once per port |
| `volume:` → `- name: data` | `[[deployment.volume]]` then `name = "data"`, once per volume |

## How values are written

| Kind of value | Written as | Watch for |
|---|---|---|
| Duration | `30s`, `10m`, `2h`, `1h 30m` | Kept in whole seconds, so smaller units round down: `1500ms` is 1 s and `500ms` is 0 s, which a health interval refuses |
| Timer | A duration, or `never` | Only `progress_deadline` and `lost_after` are timers. `never` turns the timer off, including a default one |
| Backoff | `MIN..MAX`, e.g. `1s..5m` | Two dots, no spaces needed. MAX must not be smaller than MIN |
| Budget | `N in WINDOW`, e.g. `5 in 10m` | The spaces around `in` are part of the syntax |
| Deployment name | 1–64 letters, digits, `.`, `-`, `_` | It is also the name of the service a `container` block publishes |
| Port label | 1–32 letters, digits, `-`, `_`, starting with a letter or digit | It becomes part of an environment variable name |
| Volume name | 1–48 letters, digits, `-`, `_`, starting with a letter or digit | It must match a name some machine lends |
| Name in DNS | 1–63 letters, digits, `-` | Not starting or ending with `-`, and not all digits |
| Numbers in text fields | `"80"`, `"true"` | Quote values under `container.env` and `inputs` that YAML would otherwise read as a number or a boolean. `count` and a health `port` take a number either way |

## Naming it

| Key | Type | Default | What it does |
|---|---|---|---|
| `name` **required** | text | — | The handle every command, log line and Director link uses. Two deployments cannot share one |
| `description` | text | empty | A sentence for people. Changing it restarts nothing |
| `paused` | true / false | `false` | Published but left alone: nothing is started, stopped or replaced until it is resumed. `meshnode deployment pause` and `resume` do the same without editing the file |
| `generation` | number | set for you | Leave it out. `put` numbers each version itself; the key exists because `show --output toml` prints it back |

## What runs

A file has exactly one of `service` and `container`. With neither, there is nothing to run; with
both, there would be two answers to what runs. Either one is refused.

| Key | Type | Default | What it does |
|---|---|---|---|
| `service` | `name@version` | — | A service already published (`meshnode service create`, `meshnode add`, or *Run › Scripts* in Director), pinned to one version: `api@3`. A bare `api` is refused, so what stays up can never change underneath you. Moving to `api@4` is an edit, or `meshnode deployment rollout web api@4` |
| `container` | block | — | An image, described right here. The keys below |
| `inputs` | map | none | Arguments for a published service that takes them, each handed over as text. Changing one rolls the deployment |

### `container`

| Key | Type | Default | What it does |
|---|---|---|---|
| `image` **required** | text | — | Anything podman or docker can pull: `nginx:alpine`, `ghcr.io/acme/api:1.4`. Its shape is checked when you publish, so a typo fails once instead of as a pull error on every machine |
| `command` | text | the image's | Run this instead of the image's entry point |
| `args` | list of text | none | Arguments, one list item each: `["-g", "daemon off;"]` |
| `working_dir` | path | the image's | A directory inside the image. Must start with `/` |
| `env` | map of text | none | Environment variables for every copy |

Meshware turns a `container` block into a service named after the deployment and numbers its
versions by content. Publishing an unchanged file reuses the version and restarts nothing; changing
anything in the block — the tag, one variable — publishes the next version and rolls once.

> [!NOTE]
> **The image's own filesystem is read-only.** A container copy runs with a read-only root
> filesystem. These are writable scratch space, held in memory and emptied when the copy stops:
> `/tmp` (64 MB), `/var/cache` (64 MB), `/var/log` (32 MB), `/run` and `/var/run` (16 MB each).
> Stock images like nginx and redis work as they are. Anything a program must write elsewhere, or
> keep, needs a [volume](#volume).

A machine only takes a container deployment if it has podman or docker, whatever `targets` says. See
[Runtimes](../operating/runtimes.md).

> [!WARNING]
> **`inputs` are not checked when you publish.** This build does not compare `inputs` against the
> inputs a service declares. A misspelled input is published as written, so look at the first
> copy's logs after changing one (`meshnode deployment logs web`).

## Where, and how many

| Key | Type | Default | What it does |
|---|---|---|---|
| `count` | number or `system` | `1` | A number is that many copies. They prefer different machines, but two can share one when that is all there is. `system` is exactly one copy on every matching machine, including machines that join later. `0` is refused; remove the deployment instead |
| `targets` | selector | `any` | Which machines may run it, in the grammar below |

| Selector | Matches |
|---|---|
| `any` | Every machine. Leaving `targets` out, or empty, means the same |
| `tag=edge` | Machines carrying the tag `edge` |
| `tag=role=db` | Machines carrying the key-value tag `role=db` |
| `name=web-1`, or just `web-1` | One machine, by name |
| `id=mw1…` | One machine, by the start of its node id |
| `fact.os_id=debian` | A fact the machine reports about itself. Operators: `=` `!=` `<` `<=` `>` `>=`, and `~=` for "contains". Two numbers compare as numbers; anything else compares as text |
| `fact.os.version~/^24\./` | A fact that matches a regular expression, anywhere in the value. A `|` or `,` inside the slashes belongs to the pattern |
| `fact.kernel_release:version>=6.1` | The fact compared as a version: its leading numbers, one by one, whatever follows them ignored. `6.10` is above `6.9`, and `6.1.0-18-amd64` counts as `6.1.0` |
| `fact.agent:semver>=0.7.3` | Semantic versioning, where a pre-release is below its release: `0.7.3-rc.1` does not match |
| `fact.gpu.vendor?` · `!fact.gpu.vendor?` | The machine has that fact at all — or has not |
| `tag=gpu, tag=canary` | A comma means all of the parts |
| `name=anvil\|name=edge-7` | A bar means any of the parts. The comma binds tighter, so `tag=gpu, tag=canary\|name=anvil` is (gpu and canary) or anvil |

Besides the selector, a machine has to be able to run the thing: the runtime, the ports, the volumes
and the resources all narrow the list.

> [!NOTE]
> **Matching nothing is not an error in the file.** A misspelled tag is a valid tag that no machine
> has, so it is not refused; the dry run's matching-machine count is where you find out. A malformed
> `fact.` test is different: it is refused when the file is read.

## Which machines it prefers: placement rules

`targets` says which machines *may* run a copy. These say how the copies are shared out among them.
The first two are **rules**: a machine that breaks one is never used, and a copy with nowhere left to
go waits and says why. The other two are **preferences**, weighed alongside how busy each machine
is, and they never make a machine that cannot run the copy eligible.

| Key | Type | Default | What it does |
|---|---|---|---|
| `distinct_hosts` | true / false | `false` | At most one copy on any machine. Without it copies still prefer different machines, but share one when that is all there is. A count above the number of machines that could take one is refused before publishing. It is also what lets a static port run more than one copy |
| `distinct_property` | block | none | `attribute`, and `limit` (default 1): at most that many copies on machines sharing one value — one per rack, two per house. A machine without the attribute is not used |
| `affinity` | list | none | Each entry is a `select` selector and a `weight` from -100 to 100 — default 50, never 0. Machines it selects are preferred, or with a negative weight avoided. Weights add up |
| `spread` | block | none | `attribute`, and optional `targets` of `value: percent`. Copies are shared across the attribute's values, evenly when there are no targets |
| `priority` | 1 to 100 | `50` | Orders a copy in a busy machine's queue. Above 50, a copy no machine has room for may take the place of a job somebody started with `--preemptible`, if that job's priority is at least 10 lower |

An **attribute** is `name`; `tag.<key>`, the value of a key-value tag (`tag.rack` reads `rack=b`); or
`fact.<key>`.

```yaml
deployment:
  name: api
  service: api@3
  count: 4
  distinct_hosts: true               # never two on one machine
  distinct_property:
    attribute: tag.rack
    limit: 2                         # at most two per rack
  affinity:
    - select: tag=ssd
      weight: 80
    - select: fact.os_id=ubuntu
      weight: -30
  spread:
    attribute: tag.site
    targets: { home: 75, office: 25 }
  priority: 70
```

Changing a placement rule moves nothing already running: it decides where the next copy goes.
`meshnode jobs explain <job>` on a copy shows how each machine scored, and
`meshnode deployment placement` (*Mesh › Placement* in Director) draws every machine with its copies.

## Ports: `network`

| Key | Type | Default | What it does |
|---|---|---|---|
| `mode` | `host` | `host` | The only mode. Any other word is refused rather than quietly given host networking |
| `port` | list | none | The ports each copy needs, up to 16. Each entry has the keys below |

### Each entry in `port`

| Key | Type | Default | What it does |
|---|---|---|---|
| `label` **required** | name | — | What this port is called. The number a copy was given is in `MESHWARE_PORT_<LABEL>`: upper-cased, with anything that is not a letter or digit turned into `_`, so `admin-api` is `MESHWARE_PORT_ADMIN_API`. Health checks and `register` refer to ports by label |
| `to` | 0–65535 | none | The port the program listens on inside a container — 80 for nginx. The machine publishes the copy's own port to it on every interface. Outside a container it has no effect: a process listens on `MESHWARE_PORT_<LABEL>`, at `MESHWARE_IP_<LABEL>` |
| `static` | 1–65535 | dynamic | A fixed port on the machine. Leave it out, and each copy gets a free port from its machine's range — 20000–32000 unless that machine's `[network] dynamic_ports` says otherwise |

> [!TIP]
> **Prefer dynamic ports.** A `static` port with `count` above 1 is refused: two copies on one
> machine cannot both bind it, and nothing guarantees they land apart. With `count: system` there is
> never a second copy on a machine, so a static port is safe there. A machine where the static port
> is already taken, or listed in its `reserved_ports`, is skipped and the copy goes elsewhere.

## A name on the network: `register`

Opt in to the registry and `<name>.mesh`. Every node answers the name with the machines running a
copy that is passing its health check, and SRV with each copy's port.

| Key | Type | Default | What it does |
|---|---|---|---|
| `name` **required** | DNS name | — | Copies answer at `<name>.mesh` |
| `tags` | list of text | none | Labels for filtering the registry, such as `v2` or `canary` |
| `port` | port label | every port | Which declared port to publish, when the deployment has several |
| `shutdown_delay` | duration, ≤ 300 s | none | Meant as the wait between leaving DNS and being stopped — see the note |
| `kill_timeout` | duration | none | Meant as how long a stopping copy has before it is killed — see the note |

`register` needs at least one port under `network`: a name with no port has no address to publish,
and is refused.

> [!WARNING]
> **`shutdown_delay` and `kill_timeout` are accepted but not acted on yet.** They are checked,
> stored with the deployment and printed back by `show`, but nothing in this build reads them when a
> copy stops. Setting them does no harm; do not count on a drain window from them today.

## Health checks: `health`

Without a `health` block, a copy counts as up for as long as its process runs. With one, the machine
running the copy probes it on a timer. The result decides three things: whether the copy is in DNS,
whether a rollout can move past it, and whether it is replaced.

| Key | Type | Default | What it does |
|---|---|---|---|
| `kind` **required** | `exec` · `tcp` · `http` | — | `exec` passes when a command exits 0. `tcp` passes when a connection is accepted. `http` sends `GET` to `path` and passes on a 2xx status. `tcp` and `http` connect to 127.0.0.1 on the copy's own machine |
| `command` | text | — | For `exec`, and required there. Run with `sh -c`; for a container copy it runs inside the container, so the image needs a shell |
| `port` | number or label | the only port | For `tcp` and `http`. Use a label with dynamic ports: it follows the port the copy was actually given. It can be left out only when the deployment declares exactly one port |
| `path` | text | `/` | For `http` |
| `interval` | duration | `10s` | Time between probes. At least 1 s |
| `grace` | duration | `30s` | After a copy starts, failed probes do not count for this long, and it shows as starting. Set it to how long the program takes to warm up |
| `timeout` | duration | `5s` | A probe that takes longer fails |
| `failures` | number | `3` | Failed probes in a row before the copy is marked unhealthy and replaced. `0` is treated as 1 |

A copy replaced for failing its check counts against the restart budget below, so a service that
keeps failing ends up **degraded** instead of restarting forever.

## Restarts: `restart`

| Key | Type | Default | What it does |
|---|---|---|---|
| `backoff` | `MIN..MAX` | `1s..5m` | The wait before restarting a copy that failed: MIN the first time, doubling each time after, never more than MAX |
| `budget` | `N in WINDOW` | `5 in 10m` | How many failures one copy may have within the window. Past it, the deployment shows **degraded**, and that copy backs off and is retried on a machine it has not already failed on. `0 in …` is refused |

## Rolling out changes: `update`

A change to what runs creates a new generation, and copies are replaced a few at a time. This block
says how many at once, what counts as ready, and what happens when a rollout goes wrong. Changing
anything in this block restarts nothing: it applies to the next rollout.

| Key | Type | Default | What it does |
|---|---|---|---|
| `strategy` | `rolling` | `rolling` | The only strategy |
| `max_unavailable` | number | `1` | How many copies may be mid-replacement at once. The new copy starts before the old one stops, so this sets the pace rather than the downtime. `0` is treated as 1 |
| `gate` | `health` · `running` | `health` | When a new copy counts as ready: passing its health check, or merely running. With no health check, running is all there is. Any word other than `health` means running, so spell it exactly |
| `canary` | number | `0` | Start this many new copies beside the old ones, and cut nothing over until all of them are ready. `canary` equal to `count` is a blue/green deployment |
| `auto_promote` | true / false | `true` | `false` holds ready canaries until someone runs `meshnode deployment promote web` or presses **Promote** in Director. Needs `canary` |
| `auto_revert` | true / false | `false` | When a rollout stalls, go back to the newest earlier version whose own rollout never stalled |
| `progress_deadline` | timer | none · `10m` | How long a rollout may go without any copy becoming healthy before it is marked stalled. The clock restarts every time a copy becomes healthy, and when canaries are promoted. There is none by default, but `canary` or `auto_revert` without one gets `10m`; `never` declines that |
| `min_healthy_time` | duration | `0s` | How long a copy must stay healthy before it counts as ready, which catches a program that passes one probe and then crashes. Applies with `gate: health`. Must be shorter than `progress_deadline` |

> [!NOTE]
> **A held canary still has a deadline.** With `auto_promote: false`, ready canaries left
> unpromoted past `progress_deadline` stall the rollout — and with `auto_revert`, it goes back. That
> is deliberate: a hold nobody was told about ends. Set a deadline as long as you need to look, or
> `never` to wait indefinitely.

> [!NOTE]
> **`auto_revert` needs a key it can use.** Going back is a signed publish, made by whichever
> machine is looking after the deployment. If that machine has no user key, or its key is behind a
> passphrase nobody has entered, it logs that and does not revert.

## Which edits restart copies

| You change | What happens |
|---|---|
| Anything in `container`; the service version; `targets`; `inputs`; `health`; `restart`; any port; `register`; `volume`; `resources`; `count` to or from `system` | A new generation. Copies are replaced under the `update` rules |
| `count` from one number to another | No new generation. Copies are added or retired; the ones that stay keep running. `meshnode deployment scale web 4` does the same without editing the file |
| `description`, `paused`, anything in `update` or `disconnect` | Nothing restarts. The dry run marks these *rollout knob* or *record only* |
| `distinct_hosts`, `distinct_property`, `affinity`, `spread`, `priority`, anything in `migrate` | Nothing restarts. They decide where the next copy goes, and how copies leave a drained machine |
| Nothing at all | Nothing happens. Publishing an unchanged file is safe |

## Changing it without editing the file

Some changes are commands rather than edits. Each re-publishes the deployment for you, so
`show --output toml` reflects it afterwards. If you keep the file in version control, copy the
change into it too, or the next `put` of the old file undoes it.

| Command | What it changes |
|---|---|
| `meshnode deployment scale web 4` | `count`, without restarting the copies that stay |
| `meshnode deployment restart web` | Nothing in the file. Every copy is replaced under the `update` rules; `--slot 1` replaces one |
| `meshnode deployment pause web` · `resume web` | `paused` |
| `meshnode deployment rollout web api@4` | The service version |
| `meshnode deployment rollback web` | The service version, one back. With `--generation 4` or `--tag golden`, the whole file as it was published then, keeping only the name and whether it is paused |
| `meshnode deployment promote web` | Nothing in the file. Lets canaries held by `auto_promote: false` cut over |
| `meshnode deployment tag web golden` · `untag web golden` | Nothing in the file. Names a version so it is kept past the last ten, and can be rolled back to by name |

`meshnode deployment history web -p` lists the versions a deployment keeps — its last ten, and any a
tag names — with what changed in each.

## When a machine drops: `disconnect`

When a machine stops answering, its copies become *unreachable*: they may be fine and simply cut
off. This block says how long to wait before replacing them elsewhere, and which copy stays when the
machine comes back and a slot has two.

| Key | Type | Default | What it does |
|---|---|---|---|
| `lost_after` | timer | `60s` · `never` | How long an unreachable machine's copies wait before they count as lost and are replaced on a machine that answers. `never` waits for the machine. The default is `60s` for a numbered `count` and `never` for `system`, whose copy belongs to its machine |
| `replace` | true / false | `false` | Start a replacement as soon as a machine is unreachable, without waiting for `lost_after`. Two copies of that slot may run while the machine is cut off |
| `reconcile` | one of four | `keep_original` | When the machine returns and a slot has both its original and a replacement, which one stays |

| `reconcile` | Keeps | Choose it when |
|---|---|---|
| `keep_original` | The copy that was there before | The original kept serving clients on its own side of the cut, and its state is the one people have been using |
| `keep_replacement` | The replacement | Clients could not reach the original either, so the replacement is what has actually been serving |
| `best_score` | Whichever is on the machine the scheduler would pick today | Placement matters more than history |
| `longest_running` | Whichever has been up longest | A warm cache or long-lived connections are worth keeping |

The wait starts once the mesh stops seeing the machine as alive, 5 to 10 seconds into a silence. So
with the defaults, a replacement starts about a minute and ten seconds after a machine drops.

## When a machine is drained: `migrate`

Draining a machine moves its copies somewhere else. A replacement starts first, and the copy on the
drained machine stops once the replacement has proved itself — so a service with both its copies on
one machine never has none.

| Key | Type | Default | What it does |
|---|---|---|---|
| `max_parallel` | number | `1` | How many of this deployment's copies may be moving at once |
| `min_healthy_time` | duration | `10s` | How long a replacement has to pass its health check — or, with none, keep running — before the copy it replaces stops |
| `healthy_deadline` | duration | `5m` | A replacement not healthy by then stops holding one of the `max_parallel` places, so the next copy can start moving. The copy it was replacing keeps running until the drain's own deadline |

| To | Run |
|---|---|
| Send nothing new to a machine, and leave what runs there | `meshnode nodes contribute web-7 --off` (also spelled `nodes eligibility`) |
| Empty a machine and keep it up | `meshnode nodes drain web-7`. Copies have an hour to move, then whatever is left stops: `--deadline 4h` or `--no-deadline` to change that, `--keep-system` to leave `count: system` copies, `--lift` to end it. Director: *Mesh › Fleet* → the machine → **Drain** |
| Shut a machine down | `meshnode drain` on that machine. Copies move the same way, with an hour; `--deadline 30m` also stops any other work still there when it passes |

## Volumes and resources

### `volume`

A list of directories each copy claims, up to 8.

| Key | Type | Default | What it does |
|---|---|---|---|
| `name` **required** | name | — | A volume some machine lends, by name. Copies go only to machines that lend it |
| `path` | path | none | Where a container mounts it. A process that is not in a container gets the machine's directory in `MESHWARE_VOLUME_<NAME>` instead |
| `mode` | `rw` · `ro` | `rw` | A writer excludes every other claim on that machine's volume, readers included. Readers share |

A volume is lent by each machine in its own `meshnode.toml` — the mesh cannot know what is on a
machine's disks, so the machine says:

```toml
# meshnode.toml, on each machine that has the data
[[volume]]
name = "data"
path = "/srv/meshware/data"    # absolute
read_only = false              # true refuses write claims, whatever a file asks for
```

> [!NOTE]
> **A volume is one directory on one machine.** Nothing copies, backs up or synchronises it. Two
> copies of a deployment with `mode: rw` need two machines that each lend the name, and each writes
> its own directory.

### `resources`

What each copy needs.

| Key | Type | Default | What it does |
|---|---|---|---|
| `cpus` | whole number | unstated | Cores. A machine with fewer cores than this is never used for the deployment, and one without them free right now is passed over for the next. The copy is held to them: `--cpus` for a container, the job's cgroup where the machine enforces one |
| `memory_mb` | number | unstated | Memory in megabytes, filtered and held the same way (`--memory` for a container) |
| `disk_mb` | number | unstated | Disk in megabytes. Machines without it are not candidates |

Leaving a value out, or writing `0`, says nothing about it: no machine is ruled out for that
dimension.

When every machine refuses a copy, its slot shows *absent* with the reason beside it, in
`meshnode deployment status` and in Director. When no machine is big enough at all, the status says
which requirement nobody meets.

## What a copy sees

Every copy starts with these in its environment, beside everything under `container.env`
(secrets already resolved):

| Variable | Holds |
|---|---|
| `MESHWARE_DEPLOYMENT` | The deployment's name |
| `MESHWARE_DEPLOYMENT_GENERATION` | The generation this copy belongs to |
| `MESHWARE_DEPLOYMENT_SLOT` | Its replica number (0, 1, …), or its machine's name for `count: system` |
| `MESHWARE_NODE_NAME` | The name of the machine it is running on |
| `MESHWARE_PORT_<LABEL>` | The port allocated for each declared label |
| `MESHWARE_IP_<LABEL>` · `MESHWARE_ADDR_<LABEL>` | The address to listen on for that port, and address and port together |
| `MESHWARE_VOLUME_<NAME>` | Where a claimed volume is: its `path` inside a container, or the machine's directory for a copy that is not one. Named the same way as ports |

Inside a container the same holds: the values are passed in by name (`--env NAME`), so a secret's
value is never on the engine's command line. Three variables a plain process also gets are left
out of a container, because they name files on the machine that the container cannot see:
`MESHWARE_SOCKET`, `MESHWARE_USER_IDENTITY` and `MESHWARE_BIN`. The full list is in
[Environment variables](environment.md#a-job-sees-it).

Variables starting `MESHWARE_TASK_` are Meshware's own bookkeeping. Do not set them yourself.

## Recipes

**A website with a name, a health check and careful rollouts.** Two copies, listed as `web.mesh`; a
change proves itself on one new copy before the rest follow, and goes back on its own if it never
becomes healthy.

```yaml
# web.yaml
deployment:
  name: web
  description: The public site
  count: 2

  container:
    image: docker.io/library/nginx:1.27-alpine

  network:
    port:
      - label: http
        to: 80

  register:
    name: web

  health:
    kind: http
    port: http
    path: /
    grace: 10s

  resources:
    cpus: 1
    memory_mb: 128

  update:
    canary: 1
    auto_revert: true
    min_healthy_time: 30s          # progress_deadline defaults to 10m here
```

**One copy on every machine.** A static port is safe here, because no machine ever has a second
copy. A new machine gets its copy when it joins.

```yaml
# status-page.yaml
deployment:
  name: status-page
  count: system
  targets: tag=edge

  container:
    image: docker.io/library/nginx:alpine

  network:
    port:
      - label: status
        static: 8081               # the same port on every machine
        to: 80

  health:
    kind: tcp                      # port left out: there is only one
```

**Data that lives on one machine.** Pinned by its volume to the machine that lends `redis-data`.
With nowhere else to run, it waits for that machine rather than being replaced.

```yaml
# cache.yaml
deployment:
  name: cache
  count: 1

  container:
    image: docker.io/library/redis:7-alpine

  network:
    port:
      - label: redis
        to: 6379

  register:
    name: cache

  volume:
    - name: redis-data
      path: /data

  resources:
    memory_mb: 512

  health:
    kind: exec
    command: "redis-cli ping | grep -q PONG"   # runs inside the container

  disconnect:
    lost_after: never
```

**A published service, with inputs.** For a service already published as `collector`, whose version
7 takes `region` and `interval`. Three copies, only on machines tagged for that region.

```yaml
# collector-eu.yaml
deployment:
  name: collector-eu
  service: collector@7
  count: 3
  targets: tag=region=eu

  inputs:
    region: eu-west
    interval: "30"                 # quoted: inputs are text

  restart:
    backoff: 5s..2m
    budget: 10 in 30m
```

**A change you approve by hand.** The canary comes up, becomes healthy, and waits. Look at it, then
`meshnode deployment promote web`. Leave it for two hours and the rollout stalls and goes back.

```yaml
  update:
    canary: 1
    auto_promote: false
    auto_revert: true
    progress_deadline: 2h          # how long you have to look
```

**The website again, in TOML.** Identical to `web.yaml` above; use whichever you prefer.

```toml
# web.toml
[deployment]
name        = "web"
description = "The public site"
count       = 2

[deployment.container]
image = "docker.io/library/nginx:1.27-alpine"

[[deployment.network.port]]
label = "http"
to    = 80

[deployment.register]
name = "web"

[deployment.health]
kind  = "http"
port  = "http"
path  = "/"
grace = "10s"

[deployment.resources]
cpus      = 1
memory_mb = 128

[deployment.update]
canary           = 1
auto_revert      = true
min_healthy_time = "30s"
```

## When it is refused

Every standing refusal, with the reasoning attached, is in
[By design](by-design.md) — including the ones that are about ports, volumes, `.mesh` or the
rollout rather than about the file.


`validate`, the dry run and `put` all stop at the first of these. The message names the key and,
usually, the fix.

| The message says | Change this |
|---|---|
| `unknown field …, expected one of …` | A misspelled key, or a key in the wrong block. The message lists the keys allowed at that level |
| `this deployment says nothing about what to run` | Add `service: name@version` or a `container` block |
| `names service … or carries a [deployment.container] block, not both` | Keep one of them |
| `a deployment must pin a service version` | Write `api@3`, not `api` |
| `… is not a usable image reference` | Check the image: `nginx:alpine`, `ghcr.io/acme/api:1.4` |
| `working_dir … must be absolute` | Start the path with `/` |
| `count must be at least 1` | To run nothing, remove it (`meshnode deployment rm web`), or pause it |
| `count = 2 with a static port (…) cannot work` | Drop `static` and read `MESHWARE_PORT_<LABEL>`, set `distinct_hosts: true`, or use `count: 1` or `count: system` |
| `targets: "…" is not a fact test` | Write `fact.<key><op><value>`, a pattern between slashes, or `fact.<key>?`. The message lists every form |
| `distinct_hosts puts at most one instance on a machine, and count = … but only … machines could take one` | Lower `count`, widen `targets`, or drop `distinct_hosts` |
| `affinity weight … must be between -100 and 100 and not 0` | Pick a weight in range; leave it out for 50 |
| `… attribute … must be name, tag.<key> … or fact.<key>` | Use one of the three forms: `tag.rack`, not `rack` |
| `spread targets add up to …%, more than all of the instances` | At most 100 in total |
| `count = "system" runs one instance on every matching machine, so there is nothing to place` | Remove the placement rule, and narrow `targets` instead |
| `migrate.max_parallel must be at least 1` · `migrate.min_healthy_time (…) must be shorter than migrate.healthy_deadline` | Move at least one at a time, and give a replacement longer to become healthy than it has to stay healthy |
| `priority … must be between 1 and 100` | Leave it out for 50 |
| `network mode must be "host"` | Remove `mode`, or write `host` |
| `port label … is declared twice` | Give each port its own label |
| `[deployment.register] names … but the deployment declares no port` | Add a port under `network` |
| `[deployment.register] publishes port …, which is not declared` | Use one of the labels the message lists |
| `shutdown_delay is …s; the ceiling is 300s` | Five minutes or less |
| `an exec health check needs a command` | Add `command` |
| `a tcp (or http) health check needs a port` | The deployment declares no ports: add one under `network`, or give the check a port number |
| `this deployment declares 2 ports (…), so the health check has to say which one to probe` | Add `port:` to `health` |
| `health check names port …, which this deployment does not declare` | Match a label under `network`, or give a number |
| `health interval must be non-zero` | At least `1s`. Durations are whole seconds |
| `backoff must be MIN..MAX` · `budget must be N in WINDOW` | Write `1s..5m` and `5 in 10m`, spaces included |
| `a budget of 0 failures means the service may never restart at all` | Allow at least one: `1 in 10m` |
| `min_healthy_time (…) must be shorter than progress_deadline (…)` | Shorten it, or set a longer `progress_deadline`. If the message says *the default*, the 10-minute default applied because `canary` or `auto_revert` is set |
| `auto_promote = false holds canaries for a person to promote, and this file has no canary` | Add `canary`, or remove `auto_promote` |
| `reconcile … is not one of keep_original, keep_replacement, best_score, longest_running` | Use one of the four words exactly |
| `volume name … must be 1-48 letters` · `volume … is claimed twice` | Fix the name, or list each volume once |
| `volume mode must be "rw" or "ro"` | One of those two |

**Accepted, but probably not what you meant:**

- `targets` that match no machine — a valid selector nobody fits. Check the dry run's matching count.
- `gate` spelled anything but `health` — it means `running`.
- `failures: 0` and `max_unavailable: 0` — both mean 1.
- A duration under a second — it rounds down, to 0 for anything below `1s`.
- `inputs` a service does not take — not checked when you publish.
- `register.shutdown_delay` and `kill_timeout` — stored, not yet acted on.
