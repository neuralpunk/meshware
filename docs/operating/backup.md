# Backup and recovery

*Reference. Describes what is in the tree.*

**"No control plane" does not mean "no state."** A mesh knows its members, users, grants,
policies, services, jobs, secrets and audit history. All of it is durable, replicated and
authenticated — and it is worth knowing which parts you actually have to back up.

---

## 1. What state exists

| | Where | Replicated | Survives total restart |
|---|---|---|---|
| Identity (node + user keys) | `<data_dir>/identity/` | **No — unique per node** | Yes |
| Genesis (which mesh this is) | `<data_dir>/mesh/` | Yes | Yes |
| Replicated op-log | `<data_dir>/state/` | **Yes, to every node** | Yes |
| Content (bundles) | `<data_dir>/content/` | Lazily, by digest | Yes |
| Job spool and outputs | `<data_dir>/jobs/` | Summaries only | Yes |
| Provisioned volumes | `<data_dir>/volumes.json` | **No — a directory on one machine** | Yes |
| Ephemeral objects | `<data_dir>/objects/` | No — 15-minute transport | **No, by design** |
| Live resource reservations | memory | No | **No, and must not** |

**The op-log is on every node.** That is the thing worth internalising: a mesh of forty nodes
has forty copies of its own governance, and losing any one of them loses nothing but that
node's identity and its local job outputs.

## 2. Backing a node up

```console
$ meshnode backup                       # one restorable file: identity + genesis + full op-log
```

One file. Take it from **one** node and you have the mesh's state; take it from each node and
you additionally have each node's identity.

**The identity key is the part that cannot be reconstructed.** Everything else a node holds can
come back from its peers. A node whose identity is lost is a new node that has to be admitted
again — which is recoverable, just not free.

Treat a backup file as sensitive: it contains a node's private key and the whole op-log.

The admin key is carried exactly as it is on disk. A key protected with `meshnode identity
protect` stays protected in the backup and comes back needing the same passphrase; a
hardware-backed key comes back as the reference it is, and the private half never left the
token — so restoring either is safe, and neither is any use without the passphrase or the token.

## 3. Restoring

```console
$ meshnode restore <file> --yes         # into an empty data dir, daemon stopped
$ meshnode up
```

**A restored node catches up before it writes.** Its backup is the op-log of the day it was
taken; its peers hold everything it wrote since, under sequence numbers it would otherwise use
again — and a peer drops a second op under a number it already has, for good. So on its first
start a restored node writes nothing of its own, not even its new address, and answers `Busy`
to work, until it has synced with a peer and picked up everything of its own that peer holds.
That is usually one gossip round; `/healthz` says it is waiting until then. A node whose mesh
has no other member starts writing at once, since the backup is the only copy.

**Sinks are held after a restore** and must be resumed deliberately:

```console
$ meshnode sinks resume
```

That is on purpose. A restored node has an op-log that a sink may already have consumed;
re-exporting it silently would duplicate rows in somebody's warehouse. `meshnode doctor` says
so if you forget.

### An op-log that will not open

A node whose `state/oplog.redb` is damaged fails to start, and says so. It still has its
identity, and every peer has its history, so it can be rebuilt without a backup:

```console
$ meshnode state rebuild --from 10.0.0.5:7777 --yes   # daemon stopped; any member's address
$ meshnode up
```

The damaged file is moved aside, not deleted. The rebuilt node starts under the same catch-up
hold as a restored one.

## 4. Losing a whole mesh

If every node is gone, restore any one backup and start it. That node has the complete
replicated state, so the mesh exists again with one member; the others rejoin as they come back
or are re-admitted.

If every node is gone **and** you have no backup, the mesh is gone. There is no vendor-side
copy, which is the same property that means there is no vendor-side copy of your data.

## 5. What a backup does not include

- **Secret plaintext.** Secrets are sealed per node; a backup carries the sealed form. Restore a
  node and it can still open what it could open before, because it restored its own key.
- **Ephemeral objects.** Fifteen-minute transport for `map`, not a storage tier.
- **Running work.** A restored node starts idle.
- **The content store** (`content/`). Bundles are addressed by digest and every one records
  which nodes hold it, so a restored node re-fetches what it needs from its peers on demand.
  Putting them in the backup would make a one-file restore document as large as the node's whole
  disk, to carry bytes the mesh can already supply. A mesh where *every* copy of a bundle is gone
  has lost it either way — which is why a bundle worth keeping is pushed from, or prefetched to,
  more than one node (`meshnode bundles prefetch`).

**`volumes.json` *is* included**, and it is the one thing in that directory no peer could give
back. A volume is a directory on one machine, so its record is deliberately node-local rather
than replicated; leaving it out meant a restored node came back with the directories still on
its disks and nothing claiming them. `meshnode restore` says how many came back.

## 6. Bounding what you have to keep

The op-log is on every node, so its size is (surviving history) × (nodes). Two mechanisms bound
it:

```console
$ meshnode state                         # what it costs here, and what compacting would recover
$ meshnode retention set runs=90d audit=forever
```

**Compaction** drops ops a later op supersedes. Each node compacts the ops it wrote, once an
hour, and says what it dropped in a signed checkpoint; every other node that has applied that
checkpoint drops the same ops from its own copy on its next pass. A node that joins later is
sent only what is left. An hour with nothing new to drop writes nothing. The file itself is
shrunk only when that gives back real space — three quarters of it, or a real share of what is
left on the volume — because shrinking holds the log while it runs; otherwise freed space is
reused by later writes.

**Retention** is an admin-signed replicated window for records nothing supersedes. Three windows
take ops out of the replicated log:

- `runs` (90 days by default): a finished job's last record, and a fan-out's or gang's plan and
  closing record once it has been closed for that long. A fan-out or gang another node adopted
  keeps its plan, because that node's records refer to it.
- `audit` (`forever` by default): under a finite window an audit row leaves the log, the history
  index and memory once it is older than the window. **Audit has an enforced floor of 365 days**
  (`AUDIT_FLOOR_DAYS`): no admin can sign a shorter audit window, because an audit log you can
  shorten to nothing is not an audit log. Under `forever`, audit rows are the one record that
  grows with the work the mesh does — roughly one row per run.
- `facts` (365 days by default): a fact document leaves the log and memory a window after the
  collector *observed* it. Ordinary supersession already keeps only each machine's newest
  document per collector, which bounds a subject that is still being collected and does nothing
  for one that is not — a host taken out of the inventory, a collector an admin removed, a
  machine that left. Those used to stay for the life of the mesh. A document that is still being
  republished is never at risk: the window that would have to elapse between two collections is
  a year by default.

Memory is bounded separately. A node keeps the newest 50,000 audit rows in memory and answers
`meshnode audit` for older ones from its history index (`<data_dir>/state/history.redb`), or
from the op-log when the index is switched off. Closed jobs, fan-outs and gangs leave memory
when their `runs` window passes.

`archive` is the cold tier and is pruned on demand rather than by the node, because the archive
directory is the operator's and may be on storage the node cannot see:

```console
$ meshware-archive prune --dry-run     # what a prune would delete
$ meshware-archive prune               # whole date partitions older than the `archive` window
```

It reads the window from the node — one mesh-wide policy, not a second copy in a second place —
and refuses rather than guessing if it cannot. `--days N` overrides it. Whole days only: rows in
the archive are immutable, a day is the unit everything is written and read in, and deleting
rows *within* a partition would mean rewriting a Parquet file somebody may be mid-query on.

## 7. Keeping history somewhere else

Optional, and genuinely optional — a mesh keeps its own history and answers its own questions.

```console
$ meshnode sinks add warehouse --kind clickhouse --url … 
$ meshnode sinks ls                      # lag, sent, spooled, dropped, last status
```

ClickHouse, VictoriaLogs, any webhook, or a plugin of your own. A failing sink
spends spool, never scheduler: it is lossy rather than blocking, by design, so an export
destination going down can never stall the mesh.

See [`sending-history-anywhere.md`](../sending-history-anywhere.md).
