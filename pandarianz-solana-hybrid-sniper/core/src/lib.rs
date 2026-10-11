//! pandarianz-solana-hybrid-sniper core — PAPER ONLY.
//!
//! This crate contains no private-key handling, no transaction signing and no transaction
//! broadcasting. See `scripts/check_no_live.sh` and `tests/no_live_execution.rs`.

pub mod api;
pub mod config;
pub mod decode;
pub mod engine;
pub mod fixtures;
pub mod metrics;
pub mod paper;
pub mod quote;
pub mod rpc;
pub mod safety;
pub mod live;
pub mod feed;
pub mod forward;
pub mod execcheck;
pub mod doctor;
pub mod verify;
pub mod state;
pub mod store;
pub mod strategy;
pub mod stream;
pub mod time;
pub mod types;
