//! M4: the daemon. Every mutation to any root — watch, reconcile, update,
//! merge — happens in exactly one process, reached by every client (the CLI,
//! future plugins) over a local, permission-restricted transport. See
//! `protocol` for the wire format, `transport` for the socket/pipe layer,
//! `actor` for the per-root single-writer task, `server` for the accept
//! loop, and `client` for the autostart-and-connect logic the CLI uses.

pub mod actor;
pub mod client;
pub mod config;
pub mod paths;
pub mod protocol;
pub mod run;
pub mod server;
pub mod transport;
