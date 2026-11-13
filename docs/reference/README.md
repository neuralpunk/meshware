# Reference

The exact, complete answer to "what can I set, and what does it mean". Where a page can be
generated from the code, it is; where it is written by hand, a check in CI holds it to the code,
so a new key, metric or variable that nobody documented fails the build.

| Page | What it covers | How it stays current |
|---|---|---|
| [Configuration](config.md) | Every key in `meshnode.toml`, its type, and whether a reload applies it | Generated from `config.rs` |
| [Environment variables](environment.md) | Every `MESHWARE_*` variable: the ones you set, the ones a job sees, the ones a plugin sees | Checked against every name the source reads or sets |
| [Metrics](metrics.md) | Every Prometheus series a node serves on `/metrics`, and `/healthz` | Generated from a node's own output; checked against the source |
| [Deployment file](deployment-file.md) | Every key a deployment file takes, with defaults and the refusals | Checked against `meshnode deployment schema` |
| [Schedule file](schedule-file.md) | Every key a schedule file takes | Checked against the schedule parser |
| [Script definitions](script-definitions.md) | The `.definitions` file and the `#meshware:` header a script describes itself with | Checked against the script parser |
| [Plugin contracts](plugins.md) | Secrets providers, container runners, volume provisioners and history sinks | — |
| [By design](by-design.md) | What deployments, schedules and the node surfaces under them do **not** do, with the reason on each | Written by hand |

To regenerate after changing the code:

```console
$ python3 scripts/gen-reference.py config
$ curl -s localhost:7776/metrics > /tmp/m && python3 scripts/gen-reference.py metrics --scrape /tmp/m
$ python3 scripts/gen-reference.py --check          # what CI runs
```
