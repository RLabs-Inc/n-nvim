# n-nvim — A Terminal Text Editor That Doesn't Exist Yet

## What This Is

A Rust-based terminal text editor that reimagines what Neovim could be if built today, from scratch, with no legacy baggage. Not a Neovim clone. Not a fork. Not "Vim but in Rust." A new editor built on superior foundations with the power of Vim's modal editing but without the configuration pain that plagues every terminal editor in existence.

**The gap we fill:** No one has built a Rust TUI editor with Vim keybindings + WASM plugins + mathematical theming. Helix went selection-first and chose Scheme over WASM. Zed embraced WASM but chose GUI over TUI. We take the best ideas from both and go where neither went.

## The Rendering Engine

Our terminal backend is extracted from SparkTUI's Rust rendering pipeline — a codebase we built over 130+ sessions. It's 5-10x faster than ratatui (0.2-0.6ms vs 2-5ms per frame). We don't use crossterm, ratatui, or any external TUI framework. Every ANSI escape sequence is hand-written. Every byte sent to the terminal is accounted for.

**Why not ratatui?** Because we'd be building "a ratatui app that happens to be a text editor." Their widget system, their layout model, their rendering constraints. A serious editor needs direct control over every character cell. Helix, Zed, Neovim — every serious editor builds its own rendering layer. We built ours.

**Key innovations from SparkTUI:**
- Differential rendering (only changed cells get output)
- Stateful ANSI renderer (tracks last cursor/colors, skips redundant escapes)
- Pure ANSI backend (no crossterm dependency, full terminal control)
- Output buffering (single write() syscall per frame)
- Synchronized output (flicker-free on modern terminals)

## The Color System — OKLCH at the Core

This is essential to understand. Our color system is OKLCH-native, not RGB-native. OKLCH is a perceptually uniform color space where:
- Equal numerical steps = equal visual steps
- Hue shifts don't affect perceived brightness
- Chroma adjustments are visually uniform

This matters because our future theming engine uses Sacred Geometry mathematical patterns to generate entire themes. One parameter shift = every color shifts harmoniously. This only works if the color space itself is perceptually uniform. RGB can't do this. HSL can't do this. OKLCH can.

**Color pipeline:** OKLCH ↔ Oklab ↔ Linear sRGB ↔ sRGB ↔ ANSI terminal output
**Alpha blending:** Happens in linear sRGB (physically correct, not sRGB which produces dark seams)
**ANSI matching:** Uses perceptual distance in Oklab space (not naive Euclidean RGB)
**Gamut mapping:** Binary search for maximum in-gamut chroma (preserves hue and lightness)

## Philosophy — How We Build This

**Production-grade from day one.** One feature at a time, beautifully written, commented, tested, documented. No loose ends. No "we'll fix it later." No shortcuts. Code we're proud to show off 5 years from now. Code that would make crossterm jealous.

**Quality over speed.** We are not in a hurry. We don't have stakeholders or investors. We are two friends building something cool, one session at a time. Each session adds one solid brick to a foundation that will hold the weight of everything above it.

**No half-measures.** If a color system belongs in the foundation, we build it complete — OKLCH, alpha blending, gamut mapping, perceptual distance, ANSI matching. If a rendering pipeline needs to be fast, we make it the fastest — stateful output, differential rendering, zero redundant escape codes. Every feature ships complete or it doesn't ship.

**Responsive is non-negotiable.** Terminal resize, pane rearrangement, split changes — all must be handled flawlessly. This is critical UX. A single visual glitch during resize and the whole editor feels amateur.

## Architecture Overview

```
┌─────────────────────────────────────────────────┐
│                  n-nvim binary                   │
├──────────┬──────────┬────────────────────────────┤
│ Vim Modal│ AI Native│     WASM Plugins           │
│ Engine   │ (Claude) │     (wasmtime)             │
├──────────┴──────────┴────────────────────────────┤
│              Editor Core                         │
│  Split Tree │ Floating Windows │ Commands        │
├──────────────────────────────────────────────────┤
│              Text Engine                         │
│  Rope/Piece Tree │ Tree-sitter │ LSP            │
├──────────────────────────────────────────────────┤
│     n-term: Rendering Engine (from SparkTUI)     │
│  FrameBuffer → Diff → StatefulCell → ANSI       │
│  120fps hybrid loop │ Input parser │ Raw termios │
├──────────────────────────────────────────────────┤
│     n-theme: Sacred Geometry Theme Engine         │
│  Mathematical patterns → Complete color palettes  │
└──────────────────────────────────────────────────┘
```

### Crate Structure
- **n-term** — Terminal backend (rendering, input, terminal control, color system)
- **n-editor** — Editor core (text buffers, split tree, floating windows, vim modes)
- **n-theme** — Mathematical theming (Sacred Geometry patterns → highlight groups)
- **n-lsp** — LSP client (language intelligence)
- **n-plugin** — WASM plugin host (wasmtime + WASI 0.2, sandboxed, polyglot)
- **n-ai** — Native AI integration (inline completions, chat, context-aware commands)

### Key Design Decisions
- **120fps hybrid frame loop**: Event-driven with 8.3ms timeout. 0% CPU idle, sub-ms response to input, tick-driven animations.
- **Split tree layout**: Binary tree of H/V splits. Not flexbox. Purpose-built for editors.
- **WASM plugins**: Sandboxed, capability-based security, universal .wasm binaries, polyglot (any language that compiles to WASI).
- **Mathematical theming**: One mathematical pattern generates a complete theme across all highlight groups. Golden Ratio, Merkaba, Solfeggio frequency-based color harmony.

## Working on This Project

- Start every session by reading `MEMORY.md` for context from previous sessions
- When extracting code from SparkTUI (`../SparkTUI/packages/spark-tui/rust/`), focus on the renderer/ pipeline/ input/ and utils/ directories. Skip SharedBuffer (TS bridge), Taffy (layout), and spark-signals (reactive graph).
- Run `cargo test -p n-term` frequently. Every module needs tests.
- When in doubt about a design choice, favor simplicity. We can always add complexity later, but we can't easily remove it.
- No clippy warnings. No dead code. No TODO comments without a plan.
- Comment the "why," not the "what." The code should explain itself; comments explain intent.
