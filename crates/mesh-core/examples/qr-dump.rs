//! Dump QR symbols as `0`/`1` grids, for the independent decoder in
//! `scripts/verify-qr.py` to read back.
//!
//! Not a product surface: `cargo run -p mesh-core --example qr-dump --
//! <payload>...` exists so the encoder can be checked by something that
//! shares no code with it. See the module docs in `src/qr.rs`.

fn main() {
    for payload in std::env::args().skip(1) {
        let symbol = mesh_core::qr::encode(payload.as_bytes()).expect("encodes");
        println!("PAYLOAD {payload}");
        for row in symbol.rows() {
            let line: String = row.iter().map(|d| if *d { '1' } else { '0' }).collect();
            println!("{line}");
        }
        println!("END");
    }
}
