//! # n-editor — Editor core for n-nvim
//!
//! This crate contains the fundamental building blocks of the editor:
//!
//! - **[`position`]** — `Position` (line, col) and `Range` types, 0-indexed
//! - **[`buffer`]** — `Buffer` wrapping a rope with editing, file I/O, and metadata
//! - **[`mode`]** — Vim-style modal editing (`Normal`, `Insert`, `Visual`, etc.)
//! - **[`cursor`]** — Cursor with movement, sticky column, and selection
//! - **[`word`]** — Word/WORD boundary detection for `w`/`b`/`e`/`W`/`B`/`E` motions
//! - **[`view`]** — View layer that bridges buffers to n-term's framebuffer
//!
//! Future modules will add split tree layout, floating windows, and commands.

pub mod buffer;
pub mod cursor;
pub mod mode;
pub mod position;
pub mod view;
pub mod word;
