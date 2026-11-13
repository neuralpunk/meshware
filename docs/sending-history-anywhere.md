# Sending meshware history anywhere — **Optional**

> **Nothing here is a setup step.** A meshware mesh keeps its own history,
> answers its own questions, and needs nothing else running. That is the
> product, and it is the reason there is no database in the install
> instructions. If you have not felt a need for this page, you do not have
> one.
>
> This is for the minority who do: the regulated, the audited, and the teams
> with a data warehouse that predates meshware and a mandate to keep every
> job record in it. Read it when somebody asks you for five years of job
> history in a system you already own.

## The one-paragraph version

meshware pushes; it never lets you query it back. A **sink** is a standing,
admin-signed instruction to copy finished job records — and, if you ask, the
audit trail and usage — to somewhere else, as batched NDJSON, from one
elected node. meshware writes to ClickHouse, VictoriaLogs and any webhook
itself. For anything else, a
**plugin** hands the rows to a program of yours: four lines of shell that
append them to a file let [Vector](https://vector.dev) or
[Fluent Bit](https://fluentbit.io) take them to S3, Loki, BigQuery, Kafka and
everything else those tools already speak, with **no meshware-side connector
to wait for**.

```console
$ meshnode sinks add warehouse --kind clickhouse --url https://warehouse.corp:8443 \
    --database meshware --credential '{{secret:ch-token}}'
sink warehouse saved.

$ meshnode sinks
NAME       KIND        TARGET                        LAG      SENT   SPOOLED   DROPPED  LAST
warehouse  clickhouse  https://warehouse.corp:8443     —         0       0 B         0  —

This node is the elected exporter for `warehouse`.
```

**What goes, and when.** By default a sink sends the `jobs` table: one row
per finished job. `--tables jobs,audit,usage` adds the audit trail and
resource usage. A row leaves about five minutes after its job finishes, so a
record that reaches this node late by gossip is not skipped. The node looks
for new rows every 30 seconds (`[integrations] export_interval_secs`).

## Recipe: rows in a file

The shortest way to get history out of meshware and into anything else. The
plugin is called with `write` and a batch of rows on standard input, on the
node elected to export:

```sh
#!/bin/sh
# /usr/local/bin/mw-to-file — append each batch of history to one file
set -eu
[ "$1" = "write" ] || { echo "usage: mw-to-file write" >&2; exit 2; }
cat >> /var/lib/meshware/export/history.ndjson
```

```console
$ sudo install -d -o meshnode -g meshnode /var/lib/meshware/export
$ meshnode sinks add files --kind plugin --plugin /usr/local/bin/mw-to-file --run-on tag=sink
```

It runs as the node's own account, which on a packaged node may only write
under `/var/lib/meshware`. Exit status 0 with no output means the whole batch
landed. Rotate the file with logrotate's `copytruncate`, and point Vector at
it ([below](#recipe-vector-into-clickhouse-loki-and-s3)).

`--run-on` matters for a file: the plugin runs on whichever node is elected,
and a file on one machine is not on another. Tag the machine that should hold
it.

## What a sink is, and what it is not

**A sink is write-only.** The list of jobs, its filters, its counts and the
record itself are always answered from this mesh's own tiers. Director will
never render a History page by querying your warehouse, and no meshware
command reads through a sink. That is a deliberate fence rather than an
unfinished feature: the moment a page needs a warehouse in order to paint,
you have a control plane that can be down, and meshware's whole claim is
that it has none.

**A sink cannot stall the mesh.** If the destination is unreachable, the
sink retries with a jittered backoff, spools to a bounded directory, and
then drops the oldest batches and counts them. Nothing about a full
warehouse queues a job, delays a schedule, or blocks a submission. When
somebody asks you to *guarantee delivery by blocking the job*, what they
almost always want is a guarantee about **deletion** — see
`meshnode retention` below.

**A sink is a governed entity, not a config line.** It is admin-signed and
replicated, so `meshnode audit` can answer *who pointed our history at that
host, and when*. Its credential is a `{{secret:name}}` reference — never a
literal — resolved on the machine that actually connects, so the token is
never in the op-log, never in Director's memory, and never in
`/proc/<pid>/cmdline`.

## Which machine connects

Not the one you configured it on. Records leave from **one elected node**,
chosen by rendezvous hashing over live members. If it dies, another takes
over within a gossip round; there is no handover, because the cursor is
derived from the data and the successor simply re-reads it.

In most real deployments only some machines have a route to the warehouse —
a firewall exception, a VPC, a proxy. Tag those nodes and say so:

```console
$ meshnode sinks add warehouse --kind clickhouse --url https://warehouse.corp:8443 --run-on tag=sink
```

`run_on` is the difference between a feature that works on your laptop and
one that works at your company. If no live node matches it, `meshnode sinks`
and `meshnode doctor` both say so rather than looking healthy.

## Governing where data may go

`[integrations] egress_allow` is a node-local allowlist of hosts this
machine may send data to. Empty means no restriction.

```toml
# meshnode.toml
[integrations]
egress_allow = ["warehouse.corp", "*.internal", "10.0.0.7"]
```

It is checked **on the exporting node, regardless of what the signed sink
record says**. That asymmetry is the point: the signed record is what the
mesh agreed to, and the allowlist is what this machine will actually do. A
sink pointed outside it is refused with the list named in the error.

## Recipe: Vector into ClickHouse, Loki and S3

The whole ecosystem, from the file [the plugin above](#recipe-rows-in-a-file)
writes. This config is what makes "does meshware integrate with X"
answerable as "yes, for all X".

```toml
# vector.toml
[sources.meshware]
type          = "file"
include       = ["/var/lib/meshware/export/history.ndjson"]
read_from     = "beginning"
ignore_older_secs = 604800

[transforms.parsed]
type   = "remap"
inputs = ["meshware"]
source = '''
. = parse_json!(.message)
# A job row carries `finished_at`; an audit row, `at`.
.timestamp = parse_timestamp(.finished_at, format: "%+") ?? parse_timestamp(.at, format: "%+") ?? now()
'''

# `table` is on every row, so jobs and the audit trail can go their
# separate ways from one source.
[transforms.jobs]
type      = "filter"
inputs    = ["parsed"]
condition = '.table == "jobs"'

[sinks.clickhouse_jobs]
type     = "clickhouse"
inputs   = ["jobs"]
endpoint = "https://warehouse.corp:8443"
database = "meshware"
table    = "jobs"
# meshware is at-least-once and deduplicates on the row id. Give the table
# a ReplacingMergeTree ordered by (mesh_id, job_id) and a retry is free.
auth.strategy = "basic"
auth.user     = "meshware"
auth.password = "${CLICKHOUSE_PASSWORD}"

[sinks.loki]
type     = "loki"
inputs   = ["parsed"]
endpoint = "https://loki.internal:3100"
labels.mesh  = "{{ mesh_id }}"
labels.table = "{{ table }}"
labels.state = "{{ state }}"
encoding.codec = "json"

[sinks.archive]
type   = "aws_s3"
inputs = ["parsed"]
bucket = "acme-meshware-history"
key_prefix = "date=%Y-%m-%d/"
compression    = "gzip"
encoding.codec = "ndjson"
```

Fluent Bit, Fluentd, Filebeat, Promtail, Cribl, rsyslog and a cron job with
`jq` all tail the same file. Nothing above is meshware-specific except the
path.

## Recipe: a plugin that posts somewhere

The same contract as the file recipe: NDJSON on stdin, an optional ack on
stdout, and a non-zero exit means retry.

```sh
#!/bin/sh
# /usr/local/bin/mw-ship — a complete, legitimate meshware sink.
set -eu
[ "$1" = "write" ] || { echo "usage: mw-ship write" >&2; exit 2; }
curl --fail --silent --show-error \
     --header 'Content-Type: application/x-ndjson' \
     --data-binary @- \
     "https://warehouse.corp/api/v1/ingest"
echo '{"accepted": -1}'   # -1 or an omitted ack means "all of it"
```

```console
$ meshnode sinks add shipper --kind plugin --plugin /usr/local/bin/mw-ship
```

Exit zero means the batch landed and the cursor advances. Exit non-zero
means retry, and the error message **names your plugin** — so a bug in it
does not arrive as a support ticket about meshware.

If your plugin accepted only part of a batch, say so and the cursor advances
exactly that far:

```json
{"accepted": 640}
```

## Reading the table

```console
$ meshnode sinks
NAME       KIND        TARGET                        LAG      SENT   SPOOLED   DROPPED  LAST
warehouse  clickhouse  https://warehouse.corp:8443    2m     1,284       0 B         0  ok
siem       plugin      /usr/local/bin/mw-ship         4h       900   12.4 MB         3  503
```

- **LAG** is measured in the *data's own clock* — the newest terminal row
  minus the cursor. It is comparable across sinks and stays meaningful when
  nothing has run for an hour. A sink on an idle mesh is not behind.
- **SPOOLED** is what the destination refused and the sink is holding.
- **DROPPED** is history that is **gone**. It is the number to alert on.
  Raise `--spool-mb`, or fix the destination.
- **LAST** is the status verbatim. `503` sends you to the right machine;
  "unhealthy" would send you to the wrong one.

`meshnode doctor` warns on any drop, and on a sink more than an hour behind.
Prometheus gets `meshware_sink_lag_seconds`, `meshware_sink_sent_total`,
`meshware_sink_spooled_bytes` and `meshware_sink_dropped_total`, all labelled
by sink name.

## After a restore

`meshnode restore` **holds every sink** on the restored node, and tells you
how many:

```console
$ meshnode restore backup.json --yes
restored 41,220 ops; 3 sink(s) held.

$ meshnode sinks
This node's data was restored from a backup, so nothing is exporting.
Review the sinks below, then `meshnode sinks resume`.
…
$ meshnode sinks resume
sinks resumed. Exporting continues from each sink's cursor.
```

This is deliberate. A restored mesh must not silently re-push years of
history into a live warehouse, and a *forked* one — a restore that mints a
fresh identity — must not push into a warehouse belonging to the mesh it
forked from, using that mesh's credentials.

## Retention is a different question

A sink answers *"who else has a copy"*. How long the mesh keeps its own
history is `meshnode retention`, and the two are independent on purpose: a
sink must never be the reason your history survives, because that would make
somebody else's warehouse load-bearing for your mesh.

```console
$ meshnode retention show
TABLE            MESH     FLOOR       HERE
runs              90d         —        90d   ← bounds the replicated log
facts              1y         —         1y
job-output        30d         —        30d
audit         forever        1y    forever
archive            5y         —         5y
content           30d         —        30d

policy                 the shipped defaults (nobody has signed a policy)
op-log now             412.7 MB
closed records         ~3.1 MB/day
permanent (audit, …)   ~180.4 KB/day
settles at             ~343.9 MB after a year
```

The `runs` window is the one for **job records**: it is what bounds the
replicated log. It kept its name from when it also held workflow runs, and a
retention policy already signed with it keeps working.

When somebody asks you to *block the job if the sink is down*, this is the
screen they actually want: a guarantee about how long records survive, not
about whether a warehouse acknowledged them.

### And the bad day

A window governs the steady state. Nothing governs a script looping at a
hundred records a second, which is why each *thing* — a job, a fact — gets a
rate of its own:

```console
$ meshnode state pressure
limit: 240 ops/minute per entity, refused at this node

ENTITY                 OPS/MIN    REFUSED  STATE
job:job-9f2c1a            1412       8204  paused until 12:02 (expires by itself)
job:job-4a7d0e             388        143  over the limit

`meshnode audit --action state.backpressure` has one entry per episode.
```

Three things about it worth knowing before you meet it:

- **It refuses on the machine that produced the record**, not on the ones
  that would have received it. The bug gets fixed rather than absorbed by
  your whole fleet.
- **A burst is free.** The limit is a sustained rate — four records a second
  for one thing — with four minutes' worth available instantly. A
  thousand-way fan-out is a thousand *things* with a few records each and
  never comes near it.
- **A pause expires by itself**, after five minutes. Nobody has to be paged
  to un-pause anything.

Membership, revocations, compaction checkpoints and the retention policy
itself are never refused: a control that can stop a machine joining, or stop
you setting the window that would fix the problem, is worse than the flood.

If it ever refuses something it should not, the number is yours:

```toml
[state]
op_rate_per_minute = 600   # or 0 to turn it off entirely
```

`meshnode doctor` says the same thing in one line, and Director shows it in
**Settings › Retention & export** under the window — but only when something is
actually being refused.

## The row format

One JSON object per line, one row per finished job. Field names match the
Parquet archive's exactly, so a mesh's exported rows and its Parquet never
need two explanations.

```json
{"command":"echo hello from the mesh","created_at":"2026-09-18T14:32:52.441820Z","duration_ms":27,"executor_node":"mw1cmsro5g5j…","exit_code":0,"finished_at":"2026-09-18T14:32:52.472701Z","job_id":"job-agqlj3z33fhi…","mesh_id":"mesh-aqhfdra3dyfz…","runtime":"bash","started_at":"2026-09-18T14:32:52.445671Z","state":"succeeded","submitter_node":"mw1cmsro5g5j…","submitter_user":"mwu1dj647wiv…","table":"jobs"}
```

`table` says which table a row belongs to, and `mesh_id` is on every row so
two meshes can land in one table and still be told apart. Instants are
RFC 3339 in UTC. Delivery is **at-least-once** — deduplicate on
`(mesh_id, job_id)` and a retry costs you nothing.

## Named destinations

There are four, and they are one code path — batch NDJSON, POST,
check the status, advance the cursor:

```console
$ meshnode sinks add warehouse --kind clickhouse \
    --url https://warehouse.corp:8443 --database meshware \
    --credential '{{secret:ch-token}}'
$ meshnode sinks test warehouse
ok    accepted by warehouse.corp
```

`--database` is the ClickHouse database the rows go into. Without it,
ClickHouse uses the connection's default database.

| Kind | Wire | Notes |
|---|---|---|
| `clickhouse` | `JSONEachRow` | Deduplicates identical inserts, so a retry is free. Alias: `ch` |
| `victorialogs` | JSON lines | A log store, and the one destination where **job output is exported by default** |
| `webhook` | NDJSON | The same POST at a URL you control |

`meshnode sinks ddl <name>` prints the schema and **never runs it**. Your
warehouse is yours; apply it when you are ready.

```console
$ meshnode sinks ddl warehouse
-- meshware clickhouse — DDL for sink `warehouse`
-- meshware never runs this. Review it, then apply it yourself.

CREATE TABLE IF NOT EXISTS meshware.jobs (
    mesh_id          String,
    job_id           String,
    command          String,
    …
    PRIMARY KEY (mesh_id, job_id)
)
ENGINE = ReplacingMergeTree
ORDER BY (mesh_id, job_id)
PARTITION BY toYYYYMM(created_at);
```

**Plain HTTP is refused to a public address.** A sink carries your whole
operational history and its credential in a header; over `http://` to the
internet it carries both in the clear. A destination on your own network is
fine.

## Exporting raw job output

Off by default everywhere except VictoriaLogs, which is a log store and is
what raw stdout is *for*.

```console
$ meshnode sinks add logs --kind victorialogs \
    --url https://logs.internal:9428 --payload-max-mb 32
```

Output leaves from **the machine that produced it**, straight to the log
store — never through the elected node, which would make a thousand-node
mesh export through one NIC. Over the cap, the record carries the reference
and the bytes stay put; `meshnode sinks` counts how often that happened, so
your cap is visible rather than mysterious.

## "Don't delete until it is exported"

A reasonable request, and it has an answer — with a ceiling:

```console
$ meshnode sinks add warehouse --kind clickhouse \
    --url https://warehouse.corp:8443 --hold-for-export-days 7
```

Closed records are held back from compaction for up to seven days past the
retention window while this sink is behind. **At the ceiling meshware keeps
deleting** and tells you the export gap is permanent — in `meshnode sinks`
and in `doctor`. The maximum is thirty days, and that is not negotiable: a
stalled sink must never pin the op-log open on every node in your fleet for
ever. If you need longer than that, what you actually need is a longer
retention window (`meshnode retention`), which is a decision about your mesh
rather than about somebody else's uptime.

## Reading it back

*"I need the stdout from a job that ran six months ago, and it was 5 GB."*

Storing data you cannot get back is not storing it. ClickHouse, VictoriaLogs
can be **read back into Director**; a plugin and a
webhook cannot, and the picker says which is which before you choose rather
than letting you find out in six months.

Two things make this work, and both are worth knowing about.

**A second credential.** The token that may `INSERT` should not have to
`SELECT`, so replay has its own:

```console
$ meshnode sinks add warehouse --kind clickhouse \
    --url https://warehouse.corp:8443 \
    --credential '{{secret:wh-write}}' \
    --read-credential '{{secret:wh-read}}'
```

Without one, replay is **refused and says so** — it does not quietly reuse
the write token. A sink in that state reads *"replay available — needs a
read credential"*, which points at the permission you have to grant rather
than at a limitation you cannot fix.

**The mesh fetches it, not your laptop.** The read is made by the same node
the write path elected, under the same `egress_allow` — so Director on a
machine with no route to the warehouse still opens the log, and a read is
governed exactly like the write that put the bytes there.

From Director: **Run › Activity › History**, find the job, click the destination in
the Output column. From a terminal:

```console
$ meshnode history replay job-4f2a…            # the last 500 lines
$ meshnode history replay job-4f2a… --from-end 500
$ meshnode history replay job-4f2a… --grep 'connection reset'
```

It opens at the **end**, because the reason anyone opens an old log is the
thing that went wrong. Nothing materialises the object — not on the fetching
node, not over IPC, not in Director — so a 5 GB log opens as fast as a small
one. `--grep` is handed to the destination's own index where it has one.

Opening a payload writes exactly one `history.replay` audit entry naming the
job, the sink and the host. Paging back through it writes none.

## Browsing, with or without a sink

Separate from all of the above, and it needs no sink at all: **Run ›
Activity › History** is one list across both local tiers — this node's index for recent
jobs, the Parquet archive for everything older. Rows out of the archive are
marked as such, because an archive row is a projection rather than the
record. If no node in reach holds an archive, the list says so plainly:
*"older rows exist nowhere this browse can see"* is a different fact from
*"nothing matched"*, and reading one as the other is how a person concludes
you lost their data.

**A sink is never consulted to draw that list.** Not for the rows, not for
the filters, not for the counts. A destination that is down costs you one
panel — the output of the one job you clicked — and never a page.

## Four destinations that were retired

**Retired 2026-09-20: `opensearch`, `postgres`, `splunk-hec` and `otlp`.**
A sink record that names one still decodes — the kind is a signed field and
deleting the variant would re-read every other sink's kind as the wrong
destination — and `meshnode sinks` says what to use instead rather than
reporting a transport failure nobody can act on.

Each of them was the same four lines this page opens with — batch, POST,
check the status, advance the cursor — wrapped in somebody else's schema,
auth shape and version policy, against a server meshware does not version
and cannot test. The recipe above is the replacement and it is a better one:

| Was | Use |
|---|---|
| `opensearch` | a `plugin` sink into Vector, Fluent Bit or Filebeat, which speak OpenSearch and Elasticsearch already and track their API changes for you |
| `postgres` | `clickhouse`, if you want a warehouse meshware can also read back; otherwise a `plugin` sink into your own `COPY` |
| `splunk-hec` | a `webhook` sink — it is the same POST, with your HEC URL and its token header — or a `plugin` sink into the forwarder your security team runs |
| `otlp` | an OpenTelemetry collector fed by a `plugin` sink. It was instrumentation rather than an archive: a trace is not a copy of your history, and it could never be read back |


## What is not here yet

**Iceberg**, which waits for a real organisation to ask for it — and would
sit on top of the bucket rather than beside it.

## And the copy you keep

Separate from all of this, and needing no destination at all: the **Parquet
archive** is meshware's own long-term tier, and it can replicate to a bucket
with WORM for the audit trail. See
[querying the archive](querying-the-archive.md).
