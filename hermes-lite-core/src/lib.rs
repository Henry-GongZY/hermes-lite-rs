#![cfg_attr(not(test), no_std)]
//! hermes-lite-core: the Rust twin of the C `hermes_core`.
//!
//! A `#![no_std]` + alloc agent runtime that talks to any
//! OpenAI-compatible `/v1/chat/completions` endpoint and executes locally
//! registered tools. Skills are markdown files (SKILL.md frontmatter,
//! agentskills.io compatible); conversation history is JSONL with budget
//! trimming and in-place compaction.
//!
//! Ports implement the [`Platform`] trait — the same primitives the C
//! version uses (buffered POST, optional streaming POST, four filesystem
//! ops, clock, delay, log) — and get the whole agent loop for free.

extern crate alloc;

mod agent;
mod history;
mod platform;
mod skills;
mod sse;

pub use agent::{Agent, Config, Tool};
pub use history::json_quote;
pub use platform::{HlError, HlResult, Level, Platform};
