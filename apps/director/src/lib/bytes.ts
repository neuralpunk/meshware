// Sizes, said one way everywhere (UX-19).
//
// There were seven formatters. Capacity printed the mesh's disk as
// "8886.72 GB" two centimetres under a spec line that said "8.7 TB" about
// the same number, because one helper stopped at GB with two decimals and
// the other did what the engine does. These are the only two now:
//
//   · `fmtSizeMb` — a machine's memory or disk, given in MB, exactly as
//     `mesh_proto::ipc::size_mb` writes it into the spec line, so the line
//     and every card under it agree to the digit.
//   · `fmtBytes` — anything else counted in bytes (a log, a backup, a
//     download), on the same rule: one decimal below ten, whole above.

const UNITS = ["B", "KB", "MB", "GB", "TB", "PB"];

export function fmtBytes(n: number): string {
  if (!Number.isFinite(n) || n < 0) return "—";
  let v = n;
  let i = 0;
  while (v >= 1024 && i < UNITS.length - 1) {
    v /= 1024;
    i += 1;
  }
  return i === 0 ? `${Math.round(n)} B` : `${v < 10 ? v.toFixed(1) : Math.round(v)} ${UNITS[i]}`;
}

/** `312 GB`, `4.1 TB`, `0.5 GB` — `size_mb` in `mesh-proto`, rule for rule. */
export function fmtSizeMb(mb: number): string {
  if (!Number.isFinite(mb) || mb < 0) return "—";
  const gb = mb / 1024;
  if (gb >= 1024) return `${(gb / 1024).toFixed(1)} TB`;
  if (gb >= 10) return `${gb.toFixed(0)} GB`;
  return `${gb.toFixed(1)} GB`;
}
