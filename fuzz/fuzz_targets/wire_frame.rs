#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| mesh_core::fuzz::wire_frame(data));
