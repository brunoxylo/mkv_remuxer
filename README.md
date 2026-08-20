# mkv_remuxer

Rust-based MKV/WebM remuxer with cutting, multi-source track merging, and WebVTT support. Experimental.

## Features

- **Multiple seek modes** for precise cutting
- **Lossless remuxing** — no re-encoding
- **Multi-source support** — merge tracks from multiple inputs
- **WebVTT subtitles** — read and write `.vtt`/`.webvtt` tracks
- **Track mapping** — select specific tracks from any input
- **Session streaming** — chunked, retry-safe HTTP segment delivery (see `examples/advanced_server.rs`)
- **Library API** — `remux()` convenience fn + `Remuxer` struct for streaming

## Installation

```bash
cargo build --release
```

## Usage

```bash
# Basic cut (5s to 15s)
mkv_remuxer -i input.webm -s 5s --to 15s output.webm

# Cut with duration instead
mkv_remuxer -i input.webm -s 5s -t 10s output.webm

# Multi-source track mapping: video from file 0, audio from file 1
mkv_remuxer -i video.webm -i audio.webm -m 0:0 -m 1:1 output.webm

# Add WebVTT subtitles as a track
mkv_remuxer -i video.webm -i subtitles.vtt output.mkv

# Verbose logging
mkv_remuxer -v -i input.webm -s 10s --to 20s output.webm
```

## CLI Reference

| Flag | Description |
|------|-------------|
| `-i <file>` | Input file (repeatable). `.mkv`, `.webm`, `.vtt`, `.webvtt` |
| `-s` / `--ss <time>` | Start position |
| `-t` / `--duration <time>` | Duration (mutually exclusive with `--to`) |
| `--to <time>` | End position (mutually exclusive with `-t`) |
| `--seek-mode <mode>` | `snap` (default), `snap_prev`, `snap_next`, `squeeze`, `dirty` |
| `-m` / `--map <src:track>` | Track mapping (repeatable, 0-based). Default: all tracks |
| `-v` / `--verbose` | Debug logging |

Time formats: `5s`, `1m30s`, `2h15m`, `1:30`, `1:30:00`, or plain seconds.

## Seek Modes

- **`snap`** — nearest keyframe (before or after), uses Cues index when available
- **`snap_prev`** — always the keyframe *before* the target
- **`snap_next`** — always the keyframe *after* the target
- **`squeeze`** — keeps pre-roll frames as invisible at t=0; starts exactly at requested time
- **`dirty`** — hard cut at exact timestamp; may cause decode artifacts near the cut

## Library API

One-shot:

```rust
use mkv_remuxer::{remux, CutInterval, RemuxerCutMode};
use mkv_remuxer::source::{FileSource, InputSource};
use mkv_remuxer::sink::{FileSink, OutputSink};

let source = InputSource::from(FileSource::new("input.webm")?);
let sink = OutputSink::from(FileSink::new("output.webm")?);
let cut = CutInterval::new().with_start(10_000_000_000).with_end(20_000_000_000);

let stats = remux(vec![source], sink, Some(cut), Some(RemuxerCutMode::SnapNearestKeyframe), None, true)?;
```

Streaming: use `Remuxer::new()` + `Remuxer::process()` with `StreamSink` — see `examples/stream_server.rs`. Chunked sessions: `ChunkedRemuxer` + `SessionStreamer` — see `examples/advanced_server.rs`.

## Architecture

```
InputSource<Uninitialized>  →  initialize_with_cut()  →  InputSource<Initialized>
                                                              ↓
                                                        SourcesMappings → MeltingPot
                                                              ↓
OutputSink<Uninitialized> → initialize() → FileSink / StreamSink / ChunkedStreamSink / VttSink
```

## Examples

| Example | Description |
|---------|-------------|
| `stream_server` | Simple HTTP streaming server (port 3030) |
| `advanced_server` | Full frontend + session-based chunked streaming (port 3031) |

## Requirements

- Rust 1.80+
- Valid MKV/WebM input files

## License

Provided as-is for educational and research purposes.
