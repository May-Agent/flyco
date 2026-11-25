//! Interactive full-screen `flyco` TUI (`ratatui` + `crossterm`).
//!
//! Provides a dedicated terminal interface for cloud sessions across all four
//! harnesses (Claude Code, Codex, Devin, Antigravity), live session switching
//! (`/sessions`), inactive session resuming (`/resume`), model/harness
//! switching (`/model`, `/provider`), and unified same-harness native local
//! session resume (`/resume-codex`, `/resume-claude`, `/resume-agy`,
//! `/resume-devin`).

pub mod app;
pub mod commands;
pub mod input;
pub mod picker;
pub mod render;
pub mod transcript;

pub use app::{LaunchOptions, StartPicker, run};
