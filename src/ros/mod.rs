//! The RouterOS binary API: a persistent TCP connection to port 8728 (8729 for
//! api-ssl), sentences framed by length-prefixed words, replies starting with
//! `!re` / `!done` / `!trap` / `!fatal`, and a `.tag=N` word so that replies can be
//! told apart when one socket serves several readers.
//!
//! `protocol` is the framing, `client` the connection and the command vocabulary,
//! `sim` a RouterOS-shaped server used for development.

pub mod client;
pub mod protocol;
pub mod sim;

pub use client::{Reply, RosClient, RosError};
