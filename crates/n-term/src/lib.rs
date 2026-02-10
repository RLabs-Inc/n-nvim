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

pub mod cell;
pub mod color;
