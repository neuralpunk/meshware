# Removed features

*Reference. Describes what is no longer in the tree.*

The Focus strategy (2026-08) narrowed meshware to the mesh and the run. The features it retired
sat behind `MESHWARE_LEGACY=1` for a while, and have since been removed outright: none of them
exists in any mode, and there is no switch left that brings one back. This page says what each
one was, when it went, and what to use instead. Older documents, and older nodes, may still
mention them.

---

## 1. What was removed

| What | Removed | What replaced it |
|---|---|---|
| **Workflows** — multi-step DAGs with retries, compensation and a canvas | 2026-09-12 | Nothing inside meshware. Chain steps in a script, or drive meshware from your CI, Ansible or systemd |
| **Cron-style scheduled workflow runs** | 2026-09-12 | Schedules: a service that runs itself, on a clock (`meshnode schedule`) |
| **The drafting agent** — a model that wrote workflows for you | with workflows | Nothing |
| **The mesh proxy** — `http://<name>.mesh` on port 80 through every node | 2026-09-14 | A load balancer in front of the addresses `meshnode registry` lists ([network.md §7](network.md#7-reaching-a-service)) |
| **Change control** — policies, approvals, freeze windows, blast-radius ceilings, `--break-glass`, and touch-to-approve with a registered security key: the `policy`, `approvals` and `identity token` commands | 2026-09-18 | Grants scoped to machines and services, `--dry-run` to see what a run would touch, and the audit log for who did it. A security key can still hold your identity: `meshnode identity hardware` ([limits.md §6b](limits.md#6b-hardware-bound-identity--a-key-that-cannot-be-copied)) |
| **The host inventory** — switches, routers and appliances the mesh reached but did not run on, rendered as an Ansible inventory: the `hosts` command, `--hosts` on `run`, `parallel` and `map`, `--inventory` on `service run`, host fact collectors, and `[inventory] probe` | 2026-09-18 | Keep the inventory in Ansible, and run the playbook as a script: an Ansible service still runs against the inventory in its own bundle. `[inventory]` in `meshnode.toml` is ignored, with a warning |
| **Quotas and the usage report** — shares of the pool, ceilings on it, borrowing above a ceiling, and resource use per person, per node and per window: the `quotas` and `usage` commands, and Director's **Fairness** screen | 2026-09-18 | Fair share still orders a busy queue, from the same replicated counters, with every person weighed equally (`meshnode queue`); work started `--preemptible` can still be reclaimed for someone further behind. For accounting, export the usage table to a sink ([sending history anywhere](../sending-history-anywhere.md)) |
| **The legacy switch** — `MESHWARE_LEGACY=1`, which brought the retired commands and Director screens back | 2026-09-18 | Nothing: there is nothing left behind it, and setting it does nothing |

## 2. What a mesh that used them keeps

A mesh that used any of these keeps their records in its replicated log; nodes decode them and
ignore them. Nothing has to be cleaned up.

During a rolling upgrade, nodes that are not upgraded yet keep doing what they always did with
those records. A policy keeps gating work on an older node, and a quota keeps its ceiling and
its share there, until that node is upgraded too. An upgraded node applies neither.
