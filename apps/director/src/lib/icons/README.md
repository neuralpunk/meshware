# Director's icons

Every icon here but one is a [Lucide](https://lucide.dev) glyph (lucide-static 1.47.0),
copied as-is except for the `<svg>` element itself, which is normalised to one
form: `viewBox="0 0 24 24"`, no width, height or class, stroke in
`currentColor`. `LICENSE-lucide.txt` is Lucide's licence (ISC, with the MIT
notice for the icons it inherited from Feather), and it travels with the files.

`components/Icon.svelte` registers every file in this folder by its name, so
adding an icon is adding a file. It must be drawn to Lucide's rules — a
24-unit grid, 2px padding, 2-unit stroke, round caps and joins, outlines only —
or it will not look like the rest. Take it from Lucide where one fits.

`brand-icon.svg` and `brand-icon-small.svg` are the Meshware mark, not icons,
and are not registered.

| Director name | Lucide icon |
| --- | --- |
| `access` | [`users-round`](https://lucide.dev/icons/users-round) |
| `alert` | [`triangle-alert`](https://lucide.dev/icons/triangle-alert) |
| `bell` | [`bell`](https://lucide.dev/icons/bell) |
| `box` | [`package`](https://lucide.dev/icons/package) |
| `braces` | [`braces`](https://lucide.dev/icons/braces) |
| `cancel` | [`circle-x`](https://lucide.dev/icons/circle-x) |
| `cat-data` | [`database`](https://lucide.dev/icons/database) |
| `cat-encoding` | [`binary`](https://lucide.dev/icons/binary) |
| `cat-flow` | [`workflow`](https://lucide.dev/icons/workflow) |
| `cat-hash` | [`fingerprint`](https://lucide.dev/icons/fingerprint) |
| `cat-json` | [`braces`](https://lucide.dev/icons/braces) |
| `cat-list` | [`list`](https://lucide.dev/icons/list) |
| `cat-logic` | [`split`](https://lucide.dev/icons/split) |
| `cat-number` | [`calculator`](https://lucide.dev/icons/calculator) |
| `cat-path` | [`folder-tree`](https://lucide.dev/icons/folder-tree) |
| `cat-random` | [`dices`](https://lucide.dev/icons/dices) |
| `cat-text` | [`type`](https://lucide.dev/icons/type) |
| `cat-time` | [`clock`](https://lucide.dev/icons/clock) |
| `check` | [`check`](https://lucide.dev/icons/check) |
| `chevron-down` | [`chevron-down`](https://lucide.dev/icons/chevron-down) |
| `chevron-left` | [`chevron-left`](https://lucide.dev/icons/chevron-left) |
| `chevron-right` | [`chevron-right`](https://lucide.dev/icons/chevron-right) |
| `close` | [`x`](https://lucide.dev/icons/x) |
| `command` | [`square-chevron-right`](https://lucide.dev/icons/square-chevron-right) |
| `compute` | [`cpu`](https://lucide.dev/icons/cpu) |
| `copy` | [`copy`](https://lucide.dev/icons/copy) |
| `deployments` | [`rocket`](https://lucide.dev/icons/rocket) |
| `disk` | [`hard-drive`](https://lucide.dev/icons/hard-drive) |
| `download` | [`download`](https://lucide.dev/icons/download) |
| `drag` | [`grip-vertical`](https://lucide.dev/icons/grip-vertical) |
| `expand` | [`maximize-2`](https://lucide.dev/icons/maximize-2) |
| `eye` | [`eye`](https://lucide.dev/icons/eye) |
| `file` | [`file`](https://lucide.dev/icons/file) |
| `fit` | [`scan`](https://lucide.dev/icons/scan) |
| `folder` | [`folder`](https://lucide.dev/icons/folder) |
| `fullscreen` | [`maximize`](https://lucide.dev/icons/maximize) |
| `gang` | [`network`](https://lucide.dev/icons/network) |
| `git` | [`git-branch`](https://lucide.dev/icons/git-branch) |
| `gpu` | [`gpu`](https://lucide.dev/icons/gpu) |
| `grid` | [`layout-dashboard`](https://lucide.dev/icons/layout-dashboard) |
| `help` | [`circle-help`](https://lucide.dev/icons/circle-help) |
| `history` | [`history`](https://lucide.dev/icons/history) |
| `inbox` | [`inbox`](https://lucide.dev/icons/inbox) |
| `info` | [`info`](https://lucide.dev/icons/info) |
| `jobs` | [`list-checks`](https://lucide.dev/icons/list-checks) |
| `kind-ansible` | [`book-open-check`](https://lucide.dev/icons/book-open-check) |
| `kind-bash` | [`file-terminal`](https://lucide.dev/icons/file-terminal) |
| `kind-python` | [`file-code-2`](https://lucide.dev/icons/file-code-2) |
| `library` | [`library-big`](https://lucide.dev/icons/library-big) |
| `link` | [`link`](https://lucide.dev/icons/link) |
| `memory` | [`memory-stick`](https://lucide.dev/icons/memory-stick) |
| `menu` | [`panel-left`](https://lucide.dev/icons/panel-left) |
| `mesh` | Director's own: three machines, each linked to the other two, drawn to Lucide's rules. Lucide's nearest, `network`, is a hierarchy, which a mesh is not; `waypoints` read as a squiggle at 14px. |
| `more` | [`ellipsis-vertical`](https://lucide.dev/icons/ellipsis-vertical) |
| `more-h` | [`ellipsis`](https://lucide.dev/icons/ellipsis) |
| `nodes` | [`server`](https://lucide.dev/icons/server) |
| `pair` | [`link-2`](https://lucide.dev/icons/link-2) |
| `pause` | [`pause`](https://lucide.dev/icons/pause) |
| `play` | [`play`](https://lucide.dev/icons/play) |
| `plus` | [`plus`](https://lucide.dev/icons/plus) |
| `port` | [`plug`](https://lucide.dev/icons/plug) |
| `pulse` | [`activity`](https://lucide.dev/icons/activity) |
| `redo` | [`redo-2`](https://lucide.dev/icons/redo-2) |
| `refresh` | [`refresh-cw`](https://lucide.dev/icons/refresh-cw) |
| `registry` | [`radio`](https://lucide.dev/icons/radio) |
| `revoke` | [`shield-x`](https://lucide.dev/icons/shield-x) |
| `run` | [`play`](https://lucide.dev/icons/play) |
| `save` | [`save`](https://lucide.dev/icons/save) |
| `schedules` | [`calendar-clock`](https://lucide.dev/icons/calendar-clock) |
| `script` | [`file-code`](https://lucide.dev/icons/file-code) |
| `search` | [`search`](https://lucide.dev/icons/search) |
| `secrets` | [`lock-keyhole`](https://lucide.dev/icons/lock-keyhole) |
| `service` | [`box`](https://lucide.dev/icons/box) |
| `settings` | [`sliders-horizontal`](https://lucide.dev/icons/sliders-horizontal) |
| `shield` | [`shield-check`](https://lucide.dev/icons/shield-check) |
| `shrink` | [`minimize-2`](https://lucide.dev/icons/minimize-2) |
| `snapshot` | [`layers`](https://lucide.dev/icons/layers) |
| `sort-asc` | [`arrow-up`](https://lucide.dev/icons/arrow-up) |
| `sparkle` | [`sparkles`](https://lucide.dev/icons/sparkles) |
| `terminal` | [`square-terminal`](https://lucide.dev/icons/square-terminal) |
| `trash` | [`trash-2`](https://lucide.dev/icons/trash-2) |
| `trigger` | [`zap`](https://lucide.dev/icons/zap) |
| `undo` | [`undo-2`](https://lucide.dev/icons/undo-2) |
| `view-cards` | [`layout-grid`](https://lucide.dev/icons/layout-grid) |
| `view-rows` | [`list`](https://lucide.dev/icons/list) |
| `zoom-in` | [`zoom-in`](https://lucide.dev/icons/zoom-in) |
| `zoom-out` | [`zoom-out`](https://lucide.dev/icons/zoom-out) |
