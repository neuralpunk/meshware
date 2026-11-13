# Fuzzing

Coverage-guided fuzz targets for the code that reads bytes somebody else
chose (SEC-22). Each target is one line; its body is in
`crates/mesh-core/src/fuzz.rs`, where `crates/mesh-core/tests/fuzz_smoke.rs`
also runs every target over seeds and a few thousand mutations on the
stable toolchain, as part of the ordinary test suite.

| Target | What it reads |
| --- | --- |
| `wire_frame` | a peer's framed request or response (`read_frame`) |
| `ipc_envelope` | a signed request, as the socket and the HTTP API receive it |
| `op_envelope` | a replicated op before anything is trusted |
| `op_apply` | an op body, signed by a node the engine accepts, and merged |
| `manifest` | a bundle or object manifest a peer serves |
| `http_response` | a response head, a chunked body, a redirect target |
| `jwt` | an ID token from an identity provider |
| `attestation` | a hardware attestation certificate chain |
| `agent_reply` | an SSH agent's answers, and an SSHSIG signature |
| `dns_query` | a query to the registry's resolver, and the answer built for it |

`op_apply` matters most. A merge runs under the state lock on every node
an op reaches; the engine contains a panicking merge so a node survives
it, and this target fails on one so the bug gets found.

## Running

Needs a nightly toolchain and `cargo-fuzz`:

```console
$ rustup toolchain install nightly
$ cargo install cargo-fuzz
$ cd fuzz
$ cargo +nightly fuzz run op_apply -- -max_total_time=600 -rss_limit_mb=4096
```

One target at a time, with a time and memory limit: each run is a process
that grows its corpus for as long as it is left going. `-jobs=N` runs N in
parallel and costs N times the memory.

A crash lands in `artifacts/<target>/`. Reproduce it with
`cargo +nightly fuzz run <target> artifacts/<target>/<file>`, and add the
input to the seeds in `tests/fuzz_smoke.rs` once it is fixed, so the stable
suite keeps it fixed.
