//! Meshware Director entry point.

fn main() {
    // Everything this process creates is private by default (S-L1): its
    // settings, its rolling log and a `known_hosts`, on somebody's desktop.
    //
    // That is the whole of the preamble now. Director used to embed a node
    // and so had to *be* several other programs as well — the PTY shim a
    // terminal re-execs, the sandbox around a job, the benchmark runner, the
    // build farm's `cc` wrapper — because each of those re-execs the binary
    // hosting the node. The node is `meshnode` now, in a process of its
    // own, and it is those programs; Director is a window.
    mesh_core::layout::tighten_umask();

    meshware_director_lib::run()
}
