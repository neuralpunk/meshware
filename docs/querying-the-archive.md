# Querying the archive — **Optional**

meshware keeps its own history and answers its own questions. This page is
for the case where you want a **second copy, offsite, that outlives the
mesh** — and, once it is in a bucket, six warehouses read it without
meshware writing a line of code for any of them.

Nothing here is required. A mesh with no archive and no bucket works
exactly as designed; `meshnode doctor` will tell you if you have configured
a retention window that implies an archive nobody is filling, and otherwise
stays quiet.

## The one-paragraph version

The archive is **Parquet on disk**, partitioned by day, one directory per
table. `meshware-archive ingest` copies settled history into it;
`--bucket s3://…` pushes the same files to object storage. Because the part
filenames are deterministic, pushing twice uploads nothing the second time.
Anything that reads Parquet then reads your history — DuckDB, Athena,
Trino, Snowflake, BigQuery, Redshift, Databricks, ClickHouse's `s3()` — with
no meshware installed and nothing running.

## Turning it on

The ingest is a separate binary and a systemd timer, both shipped and
**both off by default**:

```console
$ systemctl enable --now meshware-archive.timer
$ systemctl list-timers meshware-archive.timer
```

Hourly, with a random delay so a fleet does not all page the same index at
`:00`. It is idempotent and cheap when there is nothing new, so the cadence
is really choosing how much history a machine can lose between the last
ingest and a disk failure.

By hand, the same thing:

```console
$ meshware-archive ingest
archived 9021 jobs, 412 audit entries
```

## Sending it to a bucket

```console
$ meshware-archive ingest --bucket s3://acme-archive/prod
pushed 41 object(s), 388 already there, 12.4 MB
```

MinIO, Ceph, R2 and anything else S3-compatible:

```console
$ meshware-archive ingest --bucket s3://acme-archive/prod \
    --endpoint https://minio.internal:9000 --region us-east-1
```

Credentials come from the **environment** by default — `AWS_ACCESS_KEY_ID`
and friends, an instance profile, IRSA, your `AWS_PROFILE`. If you would
rather keep them in the mesh's own vault, one secret holds both halves:

```console
$ meshnode secrets set bucket-key --value 'AKIAEXAMPLE:wJalr…EXAMPLEKEY'
$ meshware-archive ingest --bucket s3://acme-archive/prod \
    --credential '{{secret:bucket-key}}'
```

Either way, **never from the command line**: a key in argv is readable from
`/proc/<pid>/cmdline` by any user on the box. The secret is unsealed with
the *node* key, so the ingest has to run as the same user as the daemon —
which is what the shipped unit's `User=meshnode` is for.

To push a directory you already have, on a machine with no node at all:

```console
$ meshware-archive --archive ./copied-archive push --bucket s3://acme-archive/prod
```

## WORM for the audit trail

> **Compliance mode, never governance mode.** Governance mode can be
> bypassed by anyone holding `s3:BypassGovernanceRetention` — usually the
> account that created the bucket. It is a speed bump with a
> compliance-sounding name, and meshware will not write it.

```console
$ meshware-archive ingest --bucket s3://acme-audit/prod --worm
pushed 3 object(s), 0 already there, 0.4 MB
3 audit object(s) written under Object Lock, compliance mode, for 365 days
```

Only the `audit` table is locked. A run record locked for a year is an
operator who cannot fix a mistake; an audit row locked for a year is the
point. The retention defaults to this mesh's own `audit` window — read from the
node when one is reachable — and is never shorter than a year. Override it
with `--worm-days`.

**Object Lock has to be enabled when the bucket is created.** It cannot be
turned on afterwards, so this needs a bucket made for it — and if the bucket
does not have it, the push fails with what S3 said rather than quietly
writing a deletable object.

## Proving nobody edited it

This is what the audit table is for, and it is why the export carries more
than the row. Every archived audit row ships with:

- `node` — the node that recorded it, which **is** its Ed25519 public key;
- `signature` — that node's signature;
- `signing_bytes` — the exact bytes the signature covers.

So the file carries its own verifier. Check the lot:

```console
$ meshware-archive verify
4182 audit row(s) · 4182 verified · 0 failed
```

That works on a copy pulled out of the bucket, on a machine that has never
been part of the mesh, with no daemon running. It is also forty lines in any
language — decode the node id, `ed25519_verify(node, signature,
signing_bytes)` — which is the reason `signing_bytes` is carried rather than
reconstructed. An audit trail you can only check by owning a compatible
meshware build is an audit trail with a dependency on us, and that is the
dependency a compliance archive exists to remove.

`request_sig` is also exported. It is the **user's** signature over the
request that caused the action, it cannot be verified from the row alone
(the signed payload is not carried), and it is there because it ties a
recorded action to a client envelope. Two signatures, one verifiable.

## Reading it, without meshware

The archive is `<table>/<date=YYYY-MM-DD>/part-*.parquet`. Every reader
below needs nothing installed but itself.

**DuckDB**, locally or straight off the bucket:

```sql
-- the commands that fail most, from a packaged node's archive
SELECT command, count(*) AS failures, avg(duration_ms) AS avg_ms
FROM read_parquet('/var/lib/meshware/archive/jobs/**/*.parquet')
WHERE state IN ('failed', 'timed-out')
GROUP BY command ORDER BY failures DESC;

-- who did what, from the audit trail
SELECT at, username, action, target, outcome
FROM read_parquet('/var/lib/meshware/archive/audit/**/*.parquet')
ORDER BY at DESC LIMIT 100;

-- or over S3, with credentials from the environment
INSTALL httpfs; LOAD httpfs;
SELECT count(*) FROM read_parquet('s3://acme-archive/prod/jobs/**/*.parquet');
```

A node you run as yourself keeps its archive in `~/.meshware/archive` instead.
`meshware-archive query` with no SQL lists more example queries, and
`meshware-archive query '<SQL>'` runs one with no DuckDB installed.

**Athena** (Glue crawler, or a table per directory):

```sql
CREATE EXTERNAL TABLE meshware_jobs (…)
STORED AS PARQUET
LOCATION 's3://acme-archive/prod/jobs/';
```

**Trino**: point a Hive catalog at the prefix and query it as a table.

**ClickHouse**, with no ingest at all:

```sql
SELECT * FROM s3('https://acme-archive.s3.amazonaws.com/prod/jobs/**/*.parquet', 'Parquet')
WHERE state = 'failed';
```

**Snowflake, BigQuery, Redshift Spectrum, Databricks**: external tables over
the same prefix. The format is Parquet and the layout is Hive-style
partitioning, which is what all of them expect.

## One archive, one mesh

An archive directory holds one mesh's history and refuses a second. The
manifest carries a single cursor pair, so two meshes in one directory would
not merge — they would interleave, each advancing the other's watermark past
rows it never wrote. Every table also leads with `mesh_id`, so unioning two
*separate* archives in your warehouse is a `UNION ALL` and stays honest.

## What this is not

**Not a backup of the mesh.** `meshnode backup` is that: identity, genesis
and the op-log, from which a node rebuilds. The archive is a *projection* —
the copy you keep and query, not the copy you restore from.

**Not a live view.** The ingest copies history that has *settled*, five
minutes behind the present by default, and the audit trail with no lag at
all. If you want the current state of a running mesh, ask the mesh.
