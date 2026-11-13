// Where scripts come from.
//
// The engine has two content concepts: Git *sources* (polled, ingested,
// named) and *bundles* (content-addressed snapshots, named only by their
// BLAKE3 id). Neither is a thing a person adds on purpose — you add "the
// ops-scripts repo" or "that folder on my laptop". This module folds both
// into one list of origins the Library can show, so a bundle id is an
// implementation detail rather than something to type.
//
// A pushed folder has no name in replicated state, so the path we pushed is
// remembered locally (this machine's Director only — cosmetic, and the
// snapshot still shows by short id everywhere else).

import type { BundleView, SourceView } from "./api";

export type OriginKind = "git" | "folder" | "snapshot";

export type Origin = {
  /** Stable key for selection/deep links. */
  key: string;
  kind: OriginKind;
  /** What to call it in a sentence — never a content hash. */
  name: string;
  /**
   * The local directory (or single file) this was pushed from, when Director
   * knows it — folder origins only. It is what a definitions file gets
   * written back to: a bundle is content-addressed and immutable, so the
   * copy on disk is the one a person edits.
   */
  localPath: string | null;
  /** Second line: clone URL @ branch, folder path, or where it came from. */
  detail: string;
  /** Short content id, shown dim and small. Null when the name says enough. */
  code: string | null;
  /** Content to browse — null for a repo that has not been read yet. */
  bundleId: string | null;
  bundleShort: string | null;
  fileCount: number | null;
  /** Micros; null when never synced. */
  updatedAtMicros: number | null;
  git: SourceView | null;
  bundle: BundleView | null;
};

export const KIND_LABEL: Record<OriginKind, string> = {
  git: "Git repo",
  folder: "Folder",
  snapshot: "Snapshot",
};

/** The icon each kind wears everywhere it appears. */
export const KIND_ICON: Record<OriginKind, string> = {
  git: "git",
  folder: "folder",
  snapshot: "snapshot",
};

const FOLDERS_KEY = "meshware.folderLabels";

type FolderLabels = Record<string, string>;

function readLabels(): FolderLabels {
  try {
    const raw = localStorage.getItem(FOLDERS_KEY);
    const parsed: unknown = raw ? JSON.parse(raw) : {};
    return typeof parsed === "object" && parsed !== null ? (parsed as FolderLabels) : {};
  } catch {
    return {};
  }
}

/** Remember that this bundle came from this directory on this machine. */
export function rememberFolder(bundleId: string, path: string) {
  try {
    localStorage.setItem(FOLDERS_KEY, JSON.stringify({ ...readLabels(), [bundleId]: path }));
  } catch {
    // Private mode / quota: labels are a nicety, never a hard dependency.
  }
}

export function forgetFolder(bundleId: string) {
  try {
    const { [bundleId]: _drop, ...rest } = readLabels();
    localStorage.setItem(FOLDERS_KEY, JSON.stringify(rest));
  } catch {
    // as above
  }
}

export function basename(path: string): string {
  const parts = path.replace(/[/\\]+$/, "").split(/[/\\]/);
  return parts[parts.length - 1] || path;
}

/** Derive a source name from a clone URL: git@host:you/ops-scripts.git → ops-scripts */
export function repoNameFromUrl(url: string): string {
  const trimmed = url.trim().replace(/\.git$/i, "").replace(/\/+$/, "");
  if (!trimmed) return "";
  const tail = trimmed.split(/[/:]/).pop() ?? "";
  return tail.replace(/[^a-zA-Z0-9_.-]+/g, "-").toLowerCase();
}

/**
 * One list of everything a script can be picked from, newest activity first:
 * Git repos, then folders pushed from this machine, then any other snapshot
 * the mesh holds (pushed from the CLI, or by another node).
 */
export function originsOf(sources: SourceView[], bundles: BundleView[]): Origin[] {
  const labels = readLabels();
  const claimed = new Set(sources.map((s) => s.lastBundle).filter((b): b is string => !!b));

  const git: Origin[] = sources.map((s) => ({
    key: `git:${s.name}`,
    kind: "git",
    name: s.name,
    detail: `${s.url}${s.branch ? ` @ ${s.branch}` : ""}`,
    // A checkout lives inside the daemon's own directory, not somewhere a
    // person keeps files, so writing into it would be edits the next sync
    // silently reverts. Definitions for a repo belong in the repo.
    localPath: null,
    code: null,
    bundleId: s.lastBundle,
    bundleShort: s.lastBundleShort,
    fileCount: bundles.find((b) => b.bundleId === s.lastBundle)?.fileCount ?? null,
    updatedAtMicros: s.lastSyncedAtMicros,
    git: s,
    bundle: bundles.find((b) => b.bundleId === s.lastBundle) ?? null,
  }));

  const rest: Origin[] = bundles
    .filter((b) => !claimed.has(b.bundleId))
    .map((b) => {
      const path = labels[b.bundleId];
      return {
        key: `bundle:${b.bundleId}`,
        kind: path ? ("folder" as const) : ("snapshot" as const),
        // A BLAKE3 id is not a name. Unnamed content says what it is and
        // wears its short id as a quiet subtitle instead.
        name: path ? basename(path) : "Unnamed snapshot",
        detail: path ?? "added outside this Director",
        localPath: path ?? null,
        code: path ? null : b.shortId,
        bundleId: b.bundleId,
        bundleShort: b.shortId,
        fileCount: b.fileCount,
        updatedAtMicros: null,
        git: null,
        bundle: b,
      };
    });

  const order: Record<OriginKind, number> = { git: 0, folder: 1, snapshot: 2 };
  return [...git, ...rest].sort((a, b) => order[a.kind] - order[b.kind] || a.name.localeCompare(b.name));
}

/**
 * Split what a person deliberately added from what merely exists. Only the
 * first group belongs in a picker; the rest hides behind a disclosure so a
 * wall of content hashes never greets anyone.
 */
export function splitOrigins(all: Origin[]): { named: Origin[]; unnamed: Origin[] } {
  return {
    named: all.filter((o) => o.kind !== "snapshot"),
    unnamed: all.filter((o) => o.kind === "snapshot"),
  };
}
