//! vmdesk client library: WebRTC session, H.264 decoding and the winit presentation layer.
//!
//! The `client` binary is a thin wrapper around [`app::run`], [`net::run`] and
//! [`decoder::run`]; the `e2e` crate uses [`decoder`] and the [`net`] peer-connection helpers
//! directly to test the media path against the server crate.

pub mod app;
pub mod assembler;
pub mod decoder;
pub mod keymap;
pub mod net;
