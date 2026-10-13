//! Navette's library: everything the `navette` binary does, behind ports that tests drive with
//! fakes. The binary (`src/main.rs`) only parses the command line and calls into it.

pub mod desk;
pub mod frame;
pub mod hypr;
pub mod identity;
pub mod link;
pub mod pair;
pub mod store;
pub mod transport;
