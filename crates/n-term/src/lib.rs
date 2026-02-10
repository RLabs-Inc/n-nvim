// SPDX-License-Identifier: MIT
//
// n-term — Terminal rendering engine for n-nvim.
//
// A fast, OKLCH-native terminal backend extracted from SparkTUI's
// rendering pipeline. Designed to be the foundation layer for a
// modern terminal text editor, with differential rendering that
// only touches changed cells, stateful ANSI output that skips
// redundant escape codes, and a color system built for mathematical
// theme generation.
//
// This crate intentionally avoids external TUI frameworks (ratatui,
// crossterm) in favor of direct terminal control via ANSI escape
// sequences and raw termios. Every byte sent to the terminal is
// accounted for. Every frame is diffed. Every escape code is earned.

#[allow(clippy::missing_errors_doc)] // ANSI functions all just forward io::Write errors.
pub mod ansi;
pub mod buffer;
pub mod cell;
pub mod color;
pub mod diff;
pub mod event_loop;
pub mod input;
pub mod output;
pub mod reader;
pub mod terminal;
