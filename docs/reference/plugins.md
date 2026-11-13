# Plugin contracts

Meshware can hand four kinds of work to a program you provide: fetching a secret from a vault
it does not run, running containers through an engine it does not drive, provisioning storage,
and delivering exported history. Each is the same shape — **any executable, the operation as its
first argument, JSON in, JSON out, a non-zero exit is a failure** — and each is configured on the
node that runs it.

| Plugin | Configured in | Called when |
|---|---|---|
| [Secrets provider](#secrets-provider) | `[secrets.providers.<name>]` | A job references `{{secret:<name>:…}}` |
| [Container runner](#container-runner) | `[runtime] container_plugin` | A container job starts, stops, or the node restarts |
| [Volume provisioner](#volume-provisioner) | `[runtime] volume_plugin` | Somebody runs `meshnode volumes create` or `rm` |
| [History sink](#history-sink) | a sink of `--kind plugin` | A batch of history is ready to export |

A plugin runs as the account the node runs as, with that account's rights, and meshware
believes what it says. Install only plugins you trust as much as the node itself.

---

## Secrets provider

For secrets that live in a vault meshware does not run — HashiCorp Vault, a cloud secret
manager, `pass`. A job refers to one as `{{secret:<provider>:<path>#<key>}}` in an environment
value; the node running the job asks the plugin at the moment the job starts.

```toml
# meshnode.toml, on each node that runs jobs needing it
[secrets.providers.vault]
plugin = "/usr/local/libexec/meshware-vault"
config = { addr = "https://vault.internal:8200", mount = "kv" }
```

```console
$ meshnode run 'deploy.sh' --env 'TOKEN={{secret:vault:apps/web#token}}'
```

**The contract.** The node runs `PLUGIN get`, writes one JSON object to its standard input, and
reads one back from its standard output:

```json
{ "path": "apps/web", "key": "token", "config": { "addr": "https://vault.internal:8200", "mount": "kv" } }
```

```json
{ "value": "s3cr3t-t0k3n" }
```

- `config` is the table from `meshnode.toml`, passed through untouched; meshware never reads it.
- Exit **0** with a `value` is success. Any other exit is a failure, and the last lines of the
  plugin's standard error are put in the job's error, so a vault that refused says why.
- The plugin has **30 seconds**.
- A string `value` is used as is; any other JSON value is used as its JSON text.

The value is used exactly like a sealed mesh secret: it is put only into the job's environment
(or, with `{{secret_file:…}}`, a mode-0400 file whose path is), never into its command line, and
it is redacted from the job's output. `meshnode secrets providers` lists the providers a node
has configured.

## Container runner

**Most meshes never need this.** Install podman or docker on a machine and meshware drives it
itself ([Runtimes](../operating/runtimes.md)). A plugin is for a runtime meshware does not drive —
gVisor, Firecracker, a hardened wrapper of your own — and it wins over the built-in engines when
it is set:

```toml
[runtime]
container_plugin = "/usr/local/libexec/my-runner"
```

The plugin **does not run the container**. It answers *"what command would run this image?"*,
and meshware spawns that command itself — which is what keeps a container job's logs,
cancellation, secrets redaction and accounting the same as every other job's.

| Operation | Required | Input | Output |
|---|---|---|---|
| `prepare` | yes | a request, JSON on standard input | a command, JSON on standard output |
| `stop` | yes | `MESHWARE_JOB_ID` in the environment | nothing. Remove that job's container; already gone is success |
| `list` | no | `MESHWARE_NODE_ID` in the environment | the job id of every container this node started, one per line |
| `info` | no | nothing | what the plugin is, and which controls it enforces |

`prepare` has 5 minutes (an image pull can be slow) and `stop` 10 seconds.

**The `prepare` request:**

| Field | What it is |
|---|---|
| `version` | The contract version, `1`. Refuse a version you do not know |
| `image` | The image reference: `docker.io/library/nginx:alpine` |
| `command` | Replace the image's entry point with this, or `null` for the image's own |
| `args` | The arguments |
| `workdir` | The working directory inside the container, or empty for the image's |
| `bundle_root`, `bundle_mount` | The job's code on the host, if it has any, and where to mount it — read-only |
| `job_dir` | The job's own directory on the host |
| `cpus`, `mem_mb` | Limits to apply; `0` means none declared |
| `ports` | `[{ "label", "host", "to" }]` — host ports already allocated for this job, and the container port each one publishes to |
| `volumes` | `[{ "name", "source", "target", "read_only" }]` — volumes already claimed for this job, to mount |
| `cgroup_parent` | The cgroup the node would have used, if it delegates one |
| `job_id` | The job's id, to name the container by |
| `node` | This node's id. **Put it on the container as a label** (`meshware.node`), so `list` can tell this node's containers from another node's on the same machine |

**The `prepare` reply:**

```json
{
  "program": "podman",
  "args": ["run", "--rm", "--name", "meshware-job-7u4z", "--label", "meshware.node=mw1…", "…"],
  "env": [["XDG_RUNTIME_DIR", "/run/user/993"], ["HOME", "/var/lib/meshware"]],
  "workdir": "",
  "forward_env_at": 22,
  "controls": { "memory": true, "cpu": true, "readonly_rootfs": true,
                "drop_capabilities": true, "no_new_privileges": true, "rootless": true }
}
```

- `env` is added to the job's environment, as `[name, value]` pairs. Meshware starts every job
  with an empty environment, so a runtime that needs `HOME` or `XDG_RUNTIME_DIR` must pass them
  here.
- `forward_env_at` is where in `args` meshware should put the job's own environment — its `env`,
  its secrets, its `MESHWARE_*` variables — as `--env NAME` pairs, the podman and docker spelling.
  Point it at the image. **Leave it out and the container gets none of them**: the command you
  return runs with the job's environment, but `podman run` passes nothing into a container unless
  a flag says so. The pairs carry names only, so the engine reads each value from its own
  environment and a secret never appears in an argument list. Left out of the list: the node's own
  directives, `HOME`, `PATH` and anything else you named in `env`, and `MESHWARE_SOCKET`,
  `MESHWARE_USER_IDENTITY` and `MESHWARE_BIN`, which name files on the host.
- Set `MESHWARE_PORT_<LABEL>` for each port and `MESHWARE_VOLUME_<NAME>` for each volume yourself,
  the second to the path *inside* the container. `<LABEL>` and `<NAME>` are upper-cased with
  anything that is not a letter or digit turned into `_`: `admin-api` is `ADMIN_API`.
- `workdir` is where to run the program on the host; empty means the job's directory.
- `controls` is **what the plugin claims it enforces**, and `meshnode doctor` reports exactly
  that. A plugin that claims nothing is reported as enforcing nothing. Keep it true to the flags
  the plugin really passes.

**`list` matters.** A restarting node calls it to find containers it left behind — a container
can outlive the node that started it — and stops them. Filter on the `meshware.node` label, or
one node will remove another's work. A plugin without `list` reaps nothing after a crash, and
`doctor` says so.

`examples/container-plugin/meshware-podman` in the source tree is a complete, commented
implementation of this contract.

## Volume provisioner

Lets a node create volumes on demand instead of only lending the `[[volume]]` directories in its
config:

```toml
[runtime]
volume_plugin = "/usr/local/libexec/meshware-mkdir"
volume_plugin_config = "/etc/meshware/volumes.conf"   # handed to the plugin; never read by meshware
```

```console
$ meshnode volumes create data --capacity-mb 512 --param pool=fast
$ meshnode volumes rm data
```

| Operation | Required | Output |
|---|---|---|
| `create` | yes | `{"path": "/srv/volumes/data", "capacity_mb": 512, "detail": "…"}` — the directory it made |
| `delete` | yes | nothing. Must succeed when the volume is already gone |
| `fingerprint` | no | anything worth showing an operator |

**Everything arrives in the environment, never in the arguments:**

| Variable | What it holds |
|---|---|
| `MESHWARE_VOLUME_NAME` | The volume's name — already checked to be 1–48 letters, digits, `-`, `_` |
| `MESHWARE_VOLUME_CAPACITY_MB` | The size asked for |
| `MESHWARE_VOLUME_PARAMS` | The `--param` pairs, as a JSON object |
| `MESHWARE_VOLUME_PATH` | Where meshware would like it, when it has an opinion |
| `MESHWARE_VOLUME_PLUGIN_CONFIG` | `volume_plugin_config`, verbatim |

Every value is validated before the plugin sees it, so a plugin that forgets to quote
`$MESHWARE_VOLUME_NAME` is a broken plugin, never a shell injection. The plugin has 2 minutes.

A created volume becomes an ordinary `[[volume]]`: the same offer, claim and lease. Meshware
**does not check** that the plugin enforces the capacity it reports; that is the plugin's promise
to keep. `examples/volume-plugin/meshware-mkdir` is a small reference implementation.

## History sink

A sink of kind `plugin` hands each batch of exported history to a program of yours — for a
destination meshware has no built-in writer for, or to write files on your own terms:

```console
$ meshnode sinks add shipper --kind plugin --plugin /usr/local/bin/mw-ship
```

The node elected to export runs `PLUGIN write` with a batch of **NDJSON on standard input** — one
row per line, the same fields as every other destination — and reads an acknowledgement from
standard output:

```json
{ "accepted": 640 }
```

- Exit **0** means the batch landed, and the export moves past it. `{"accepted": -1}`, or no
  output at all, means all of it; a smaller number means only that many rows, and the rest are
  sent again.
- A **non-zero** exit means *retry*: the batch is kept and sent again later, and the error names
  your plugin.
- The plugin has 10 minutes per batch.

```sh
#!/bin/sh
# /usr/local/bin/mw-ship — a complete sink plugin
set -eu
[ "$1" = "write" ] || { echo "usage: mw-ship write" >&2; exit 2; }
curl --fail --silent --show-error \
     --header 'Content-Type: application/x-ndjson' \
     --data-binary @- \
     "https://warehouse.corp/api/v1/ingest"
echo '{"accepted": -1}'
```

Delivery is at least once: deduplicate on the row's id. See
[Sending history anywhere](../sending-history-anywhere.md) for everything else about sinks.
