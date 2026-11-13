# Bundled meshnode binaries

`scripts/build-director-release.sh` copies the bare release binaries it finds
(`meshnode-v<version>-<arch>-unknown-linux-musl`, from `dist/` or
`$MESHNODE_BUNDLE_DIR`) here as `meshnode-<arch>-unknown-linux-musl`, with the
version in `VERSION`. Tauri ships whatever is in this directory inside the
installer (`bundle.resources` in `tauri.conf.json`).

Director's **Add machines** looks for a binary in this order: a release
published in the mesh, a binary here, a `meshnode` installed beside
Director, and only then the release channel. A Director built with nothing
here still works — it simply has one fewer place to look.

The binaries themselves are build output and git-ignored; this file is here
so the directory (and the resource glob) always exists.
