//! vmdesk server library: everything the `server` binary does, exposed so the end-to-end tests
//! in the `e2e` crate can drive the capture → convert → encode → WebRTC path without a display.
//!
//! Linux only (DRM/KMS capture, uinput injection).

pub mod capture;
pub mod config;
pub mod convert;
pub mod doctor;
pub mod encoder;
pub mod input;
pub mod metadata;
pub mod netinfo;
pub mod pipeline;
pub mod png_out;
pub mod rtcp_forward;
pub mod session;
pub mod setup;
pub mod signalling;
pub mod testpattern;
