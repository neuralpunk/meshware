#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| mesh_core::fuzz::agent_reply(data));
