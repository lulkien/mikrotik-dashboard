//! A native dashboard for MikroTik RouterOS: the RouterOS binary API client
//! (`ros`), the state it fills (`model`) and the collector thread that polls it
//! (`poll`). The Slint window lives in `main.rs`; nothing in this library knows
//! about Slint, so the client and the poll loop can be tested — and probed
//! against a real router — without a display.

pub mod model;
pub mod poll;
pub mod ros;
