//! Library half of the `meshnode` binary: the parts worth exercising
//! in-process by integration tests. `main.rs` is the CLI around them.
//!
//! - [`upgrade`] (MASTERPLAN M7): the self-upgrade flow and its tiny HTTPS
//!   client.
//! - [`farm`]: the `meshnode cc` compiler wrapper. Its one interesting
//!   claim — *a real compile, on a real node, comes back as a real object
//!   file* — needs an engine on the other end, which is what
//!   [`farm::Submitter`] lets a test provide.

// The build farm wrapper lives in the engine now — Director has to be able
// to be `CC` too, and it does not depend on this crate. Re-exported so
// `meshware_node::farm::…` keeps meaning what it always did.
pub use mesh_core::farm;
pub mod upgrade;
