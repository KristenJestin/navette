//! Navette's library: everything the `navette` binary does, behind ports that tests drive with
//! fakes. The binary (`src/main.rs`) only parses the command line and calls into it.

pub mod identity;
pub mod store;
pub mod transport;
