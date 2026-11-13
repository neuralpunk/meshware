#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| mesh_core::fuzz::ipc_envelope(data));
