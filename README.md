# iced_digraph

Small desktop app: an **interactive digraph heatmap** for raw binary data, built with [Iced](https://github.com/iced-rs/iced). Histogram construction and rasterization come from the sibling [`digraph`](../digraph) library.

## Prerequisites

- **Sibling `digraph` checkout** at `../digraph` (this crate uses a path dependency; see `Cargo.toml`).
- **Default sample** on startup: `../digraph/tests/data/sample.bin` relative to this crate. If that file is missing, the viewer starts with an empty buffer until you use **Open…**.
- **Rust toolchain**: `Cargo.toml` declares `rust-version = "1.74"`, but **Iced 0.14** is built and tested against a **much newer** compiler in practice. Use a current stable Rust (`rustup update stable`) if `cargo build` fails on an older toolchain.

## Run

```bash
cargo run --release
```

Works from a normal shell on Windows, Linux, or macOS.

## What you see

- **Left column**: heatmap palette, pair mode (overlapping vs non-overlapping), normalization scale, **Open…** (native file dialog), and a **progress bar** while a file is loading.
- **Right side**: a vertical **byte rail** (overview strip and draggable selection) beside the **heatmap** for the currently selected byte range.

Drag the rail handles or the shaded band to change which part of the file is fed into the digraph. See [`src/minimap.rs`](src/minimap.rs) for the canvas rail implementation.

## Behavior and architecture

- **Large files** are read in **chunks** on a background async path (Tokio `fs` + Iced `Subscription` / `stream::channel`). Bytes live in an `Arc<RwLock<Vec<u8>>>` shared with the UI.
- While a load is active, the rail shows a **placeholder** strip until the buffer length matches the file size from metadata.
- The **heatmap is not recomputed on every chunk**; recomputation runs after the load finishes (or on errors that clear the load), so the window stays responsive on very large inputs.

