//! Agent detection and status tracking module
//!
//! This module provides types and utilities for detecting which coding agent
//! (Claude Code, Codex, Aider) is running in a PTY session and tracking
//! agent status metrics.

pub mod adapter;
pub(crate) mod config;
pub mod detector;
pub(crate) mod events;
pub(crate) mod kimi_usage_consent;
pub(crate) mod notification;
pub(crate) mod reply;
pub(crate) mod review;
pub mod sanitize_title;
pub mod types;

// Re-export commonly used types for external modules and frontend
#[allow(unused_imports)]
pub use types::{AgentDetectedEvent, AgentDisconnectedEvent, AgentType};

pub use adapter::base::TranscriptState;
pub use adapter::AgentWatcherState;
pub use sanitize_title::sanitize_title;
