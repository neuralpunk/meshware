# Environment variables

Every `MESHWARE_*` variable, grouped by who uses it:

- [**You set it**](#you-set-it): to configure `meshnode`, Director or the installer.
- [**A job sees it**](#a-job-sees-it): Meshware puts it into the environment of the work it runs.
- [**A plugin or hook sees it**](#a-plugin-or-hook-sees-it): Meshware passes it to a program you
  configured.
- [**Reserved**](#reserved-do-not-set): Meshware's own bookkeeping. Do not set these.

An empty value counts as unset everywhere.

> [!TIP]
> **Setting a variable for a packaged node.** The service does not read your shell. Add a drop-in
> with `sudo systemctl edit meshnode`:
>
> ```ini
> [Service]
> Environment=MESHWARE_LOG=info,mesh_core=debug
> ```
>
> Then run `sudo systemctl restart meshnode`. Most settings are better kept in
> [`meshnode.toml`](config.md), where `meshnode config check` can see them.

## You set it

### `meshnode`: the node and the CLI

A variable wins over the matching key in `meshnode.toml`. A command-line flag wins over both.

| Variable | What it does | Default |
|---|---|---|
| `MESHWARE_HOME` | The node's home directory: config, identity, state, jobs. The same as `--data-dir` (or `--home`), which wins over it. Setting it also stops the CLI from looking for a node anywhere else | `~/.meshware`, or `/var/lib/meshware` for the service account |
| `MESHWARE_DATA_DIR` | The older name for `MESHWARE_HOME`, still honoured. `MESHWARE_HOME` wins if both are set. The shipped systemd units set this one, to `/var/lib/meshware` | — |
| `MESHWARE_NODE_NAME` | This node's name. Overrides `node_name`; `up --node-name` wins over it | the host name |
| `MESHWARE_LISTEN` | The address and port for mesh traffic, as `IP:port`. Overrides `[listen]` | `0.0.0.0:7777` |
| `MESHWARE_MESH_NAME` | The name of the mesh this node creates, if it creates one. Overrides `[mesh] name`. Ignored when the node joins a mesh | `<node name>-mesh` |
| `MESHWARE_TAGS` | Comma-separated tags, replacing `tags`: `gpu,dc=east`. `--tag` wins over it | none |
| `MESHWARE_ADVERTISE` | Comma-separated `host:port` addresses other machines can reach this node at. Replaces `[wan] advertise` | none |
| `MESHWARE_RELAY` | Offer to relay traffic for machines that cannot reach each other: `on` or `off`. Overrides `[wan] relay`. Any other value is an error | `off` |
| `MESHWARE_LOG` | How much the node logs, as a filter: `info`, or `info,mesh_core=debug`. Overrides `[log] level`. `RUST_LOG`, if set, wins over it | `info` |
| `MESHWARE_HTTP` | The `/healthz` and `/metrics` endpoint. An address (`127.0.0.1:7776`) moves it and turns it on, even when the config turns it off. `off` turns it off | `127.0.0.1:7776` |
| `MESHWARE_API` | Turns the management API on at this address. Overrides `[api] listen`. It can only turn the API on: `off` leaves whatever the config says | off |
| `MESHWARE_RELEASE_BASE` | Where `meshnode upgrade` downloads releases from. `upgrade --release-base` and `[release] base` win over it. A plain `http://` URL works but warns | GitHub releases |
| `MESHWARE_MINISIGN_PUBKEY` | The key releases must be signed with. `--release-key` and `[release] pubkey` win over it. Setting it on a node makes that node check fleet upgrades against this key instead of the keys the mesh trusts, so use it only to test your own builds | the mesh's trusted keys |
| `MESHWARE_USER_IDENTITY` | The key file the CLI signs your requests with. The same as `--user-identity`, which wins over it | `<home>/identity/user.key`, then `~/.meshware/identity/user.key` |
| `MESHWARE_SOCKET` | The node's local socket, when it is somewhere unusual. It does not change which home's key the CLI uses | found from the home |
| `MESHWARE_IPC_TIMEOUT` | Seconds the CLI waits for the node before giving up. The same as `--ipc-timeout`. `0` waits forever | 30, or 1800 for slow requests |
| `MESHWARE_PASSPHRASE` | Opens a passphrase-protected user key without asking. Needed when there is no terminal to ask on. Removed from the environment of `--then` and `[notify]` hooks | ask |
| `MESHWARE_AGENT_SOCK` | The ssh-agent socket that holds a hardware-backed key. `identity hardware ls --socket` wins over it | `SSH_AUTH_SOCK` |
| `MESHWARE_ARCHIVE` | Where `meshware-archive` finds the archive. The same as its `--archive` | `<node home>/archive` |
| `MESHWARE_SANDBOX_SHIM` | The program re-run to sandbox a job. For packagers and tests | this `meshnode` |
| `MESHWARE_PTY_SHIM` | The program re-run to host a terminal session. For packagers and tests | this `meshnode` |
| `MESHWARE_CHECK_ARGS` | Set to `1` and `meshnode` checks its arguments, then exits without doing anything. The docs checker uses it | off |

`meshnode demo` ignores the variables above that would point its throwaway nodes at a real one.

### Director

Director drives the node in `<home>`, where `<home>` is found as above from `MESHWARE_HOME` or
`MESHWARE_DATA_DIR` — the same node the command line uses on that machine. That node reads the
variables in the section above as it always does; Director keeps only its own window settings in
`<home>/director`.

| Variable | What it does |
|---|---|
| `MESHWARE_HOME` · `MESHWARE_DATA_DIR` | Where Director keeps its node and your key |
| `MESHWARE_RELEASE_BASE` | Where **Updates** downloads releases from |
| `MESHWARE_MINISIGN_PUBKEY` | A key to check downloads against, after the keys the mesh trusts |
| `MESHWARE_PASSPHRASE` · `MESHWARE_AGENT_SOCK` | Unlock your key, as for the CLI |

Four more are read **when Director is built**, not when it runs. They set what **About** and the
help menu point at, so a fork or a downstream build can send people to its own pages:
`MESHWARE_DOCS_URL`, `MESHWARE_ISSUES_URL`, `MESHWARE_SOURCE_URL` and `MESHWARE_BUILD_COMMIT`.

### The installer (`install.sh`)

| Variable | What it does | Default |
|---|---|---|
| `MESHWARE_VERSION` | The release to install, such as `v0.9.0` | the latest |
| `MESHWARE_BIN_DIR` | Where to put `meshnode` | `/usr/local/bin` if writable, else `~/.local/bin` |
| `MESHWARE_REQUIRE_SIGNATURE` | Set to `1` to stop unless the download's provenance (or, for a fork that signs, its signature) was actually checked | off |
| `MESHWARE_NO_VERIFY` | Set to `1` to skip every check, including the one above. Do not | off |

## A job sees it

A job starts with an **empty** environment. It gets `PATH`, `HOME`, `LANG`, `LC_ALL`, `USER` and
`TERM` from the node, then the variables below, then its own `env` and secrets. Where a name
appears in both, the job's own value wins, except for the device variables, which always come last.

**Every job:**

| Variable | Holds |
|---|---|
| `MESHWARE_NODE_NAME` | The name of the machine running it |
| `MESHWARE_SOCKET` | That machine's node socket, so the job can use `meshnode` or the SDK |
| `MESHWARE_USER_IDENTITY` | A key made for this job alone. It works only on this machine, only as the person who started the job, and only while the job runs |
| `MESHWARE_BIN` | The `meshnode` binary that is running, so a job can call it without it being on `PATH` |
| `MESHWARE_SANDBOX` | How well the job is sandboxed on this machine: `full`, `partial` or `none`. Not set in a container, which has the engine's own isolation instead |
| `CUDA_VISIBLE_DEVICES` · `HIP_VISIBLE_DEVICES` · `ROCR_VISIBLE_DEVICES` | The GPUs the job holds. **Empty** for a job that holds none, so it sees none |

**Depending on what the job asked for:**

| Variable | When | Holds |
|---|---|---|
| `MESHWARE_PORT_<LABEL>` | It asked for a port | The port on this machine. `<LABEL>` is upper-cased, with anything that is not a letter or digit turned into `_`: `admin-api` is `MESHWARE_PORT_ADMIN_API` |
| `MESHWARE_IP_<LABEL>` · `MESHWARE_ADDR_<LABEL>` | The same | The address to listen on (`0.0.0.0` on a node that listens everywhere), and address and port together |
| `MESHWARE_VOLUME_<NAME>` | It holds a volume | The volume's directory. `<NAME>` is spelled as for ports |
| `MESHWARE_DEPLOYMENT` · `MESHWARE_DEPLOYMENT_GENERATION` · `MESHWARE_DEPLOYMENT_SLOT` | It is a copy of a [deployment](deployment-file.md#what-a-copy-sees) | The deployment's name, its generation, and the copy's slot |
| `MESHWARE_SCHEDULE` · `MESHWARE_SCHEDULE_OCCURRENCE` | A [schedule](schedule-file.md) started it | The schedule's name, and the time it was due, in microseconds since 1970 |
| `MESHWARE_DEVICE_COUNT` · `MESHWARE_DEVICE_INDICES` · `MESHWARE_DEVICE_UUIDS` | It holds GPUs (`--gpus`) | How many, and which |
| `MESHWARE_GANG_ID` · `MESHWARE_GANG_SIZE` · `MESHWARE_GANG_RANK` · `MESHWARE_GANG_NODES` · `MESHWARE_GANG_LEADER` | It is one member of a gang (`run --gang`) | The gang, how many members, this member's rank from 0, every member's `host:port` in rank order, and rank 0's address |
| `MESHWARE_TASK_CPUS` · `MESHWARE_TASK_MEM_MB` · `MESHWARE_TASK_DISK_MB` | It declared `--cpus`, `--mem` or `--disk` | What it declared. For reading only |
| `MESHWARE_FACT_COLLECTOR` · `MESHWARE_FACT_SUBJECT` | It is a fact collector | The collector's name, and the node it is about |

**Each item of `parallel` and `map`:**

| Variable | Holds |
|---|---|
| `MESHWARE_ITEM` | The item's value |
| `MESHWARE_OUT` | `map` only: a file to write the item's result to. If nothing is written, its output is used |
| `MESHWARE_INPUTS` | The `--reduce` step: a directory holding one file per result |
| `MESHWARE_ITEMS` · `MESHWARE_RESULTS` | A batch of items run together: a file of items, one JSON string per line, and the file to write one JSON result per line to |

**In a container** the job gets the same variables, passed in by name so no secret appears on the
engine's command line. The exceptions: `MESHWARE_SOCKET`, `MESHWARE_USER_IDENTITY` and
`MESHWARE_BIN` are left out, because they name files on the machine that the container cannot see,
and so are the `MESHWARE_TASK_*` values. `MESHWARE_VOLUME_<NAME>` holds the path *inside* the
container.

`meshnode run --fallback local` runs the command on your own machine when no node is reachable. It
is not a job: it keeps your shell's environment and adds `MESHWARE_FALLBACK=local`.

## A plugin or hook sees it

| Variable | Given to | Holds |
|---|---|---|
| `MESHWARE_JOB` · `MESHWARE_STATE` · `MESHWARE_EXIT` · `MESHWARE_NODE` | `jobs wait --then` | The job's id, how it ended, its exit code, and the machine that ran it |
| `MESHWARE_NOTIFY_TITLE` · `MESHWARE_NOTIFY_BODY` · `MESHWARE_NOTIFY_JOB` · `MESHWARE_NOTIFY_STATE` · `MESHWARE_NOTIFY_EXIT` · `MESHWARE_NOTIFY_NODE` · `MESHWARE_NOTIFY_COMMAND` · `MESHWARE_NOTIFY_SECONDS` | The `[notify] command`, for `--notify` | A ready-made title and message, then the job's short id, how it ended, its exit code (empty if unknown), the machine, the command, and how long it ran |
| `MESHWARE_JOB_ID` | A container plugin's `stop` | The job whose container to remove |
| `MESHWARE_NODE_ID` | A container plugin's `list` | The node whose containers to list |
| `MESHWARE_VOLUME_NAME` · `MESHWARE_VOLUME_CAPACITY_MB` · `MESHWARE_VOLUME_PARAMS` · `MESHWARE_VOLUME_PATH` · `MESHWARE_VOLUME_PLUGIN_CONFIG` | A volume plugin | See [Plugin contracts](plugins.md#volume-provisioner) |
| `MESHWARE_HOME` | A build started from Director's **Build farm** | Director's home |

Secrets providers and history sinks get no `MESHWARE_*` variables.

## Reserved: do not set

`meshnode run` refuses these names in `--env`. Most are how a request travels from your machine to the
node that runs it.

| Family | What it is |
|---|---|
| `MESHWARE_TASK_*` | What the job asked for: resources, ports, volumes, devices, placement, runtime, command, and the rest |
| `MESHWARE_GANG_*` · `MESHWARE_DEVICE_*` · `MESHWARE_FACT_*` · `MESHWARE_SCHEDULE` · `MESHWARE_SCHEDULE_OCCURRENCE` | Set by the node (above), so that a job cannot claim them |
| `MESHWARE_OUT_*` | What `run --json`, `--outputs` and `--expect` capture |
| `MESHWARE_CACHE_*` (`MESHWARE_CACHE_KEY`, `MESHWARE_CACHE_VERIFY`) | The result cache's key, and a re-run to check a cached result |
| `MESHWARE_ACT_*` | Who a job acts for. Removed before the job starts |
| `MESHWARE_INV_*` | Left from the retired host inventory. Refused from a job's environment, so a node from before the removal is never handed one a person wrote |
| `MESHWARE_ANSIBLE_*` (`MESHWARE_ANSIBLE_LIMIT`, `MESHWARE_ANSIBLE_TAGS`, `MESHWARE_ANSIBLE_CHECK`, `MESHWARE_ANSIBLE_FORKS`) | Playbook options, read by the node, not the playbook |
| `MESHWARE_WF_*` · `MESHWARE_STREAM` | Left from the retired workflow engine. Nothing sets them |

Some names are only kept out of the result cache's key, and nothing sets them for a job today:
`MESHWARE_JOB_ID`, `MESHWARE_JOB_DIR`, `MESHWARE_NODE`, `MESHWARE_NODE_ID`, `MESHWARE_RUN_ID`,
`MESHWARE_STEP`, `MESHWARE_ATTEMPT` and `MESHWARE_ITEM_INDEX`. Do not rely on them in a job.
