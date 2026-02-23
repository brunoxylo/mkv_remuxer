# rust mkv_remuxer

A Rust-based MKV/WebM remuxer with advanced seeking, cutting, and multi-source track merging.  
Still highly buggy and in experimental state.

## Features

- **Multiple Seek Modes** for precise video cutting
- **Lossless Remuxing**: No re-encoding, preserves original quality
- **Multi-source Support**: Merge tracks from multiple input files into one output
- **WebVTT Subtitle Merging**: Add `.vtt`/`.webvtt` subtitle files as tracks
- **Flexible Track Mapping**: Select specific tracks from any input source
- **Cue-accelerated Seeking**: Uses MKV Cues index for fast cluster location when available
- **Library API**: Use as a library with the `Remuxer` struct or the `remux()` convenience function

## Installation

```bash
cargo build --release
```

## Usage

### Basic Cut

Extract from 5s to 15s:

```bash
mkv_remuxer -i input.webm -s 5s --to 15s output.webm
```

Or specify a duration instead of an end point:

```bash
mkv_remuxer -i input.webm -s 5s -t 10s output.webm
```

### Seek Modes

Controlled with `--seek-mode`. Default is `snap`.

**Snap nearest keyframe** — fast, output starts at the keyframe closest to the requested time:
```bash
mkv_remuxer -i input.webm -s 10s --to 20s --seek-mode snap output.webm
```

**Snap previous keyframe** — like `snap` but always chooses the keyframe *before* the target:
```bash
mkv_remuxer -i input.webm -s 10s --to 20s --seek-mode snap_prev output.webm
```

**Squeeze** — keeps all frames but compresses pre-roll into invisible frames at t=0 so playback starts exactly at the requested time:
```bash
mkv_remuxer -i input.webm -s 10s --to 20s --seek-mode squeeze output.webm
```

**Dirty cut** — hard cut at the exact timestamp; may cause decoding issues near the cut:
```bash
mkv_remuxer -i input.webm -s 10s --to 20s --seek-mode dirty output.webm
```

### Multiple Inputs and Track Mapping

Merge video from one file with audio from another:

```bash
mkv_remuxer -i video.webm -i audio.webm -m 0:0 -m 1:1 output.webm
```

Mapping format is `source_index:track_index` (both 0-based). If no mappings are given, all tracks from all sources are included.

### Add WebVTT Subtitles

Merge an MKV with a WebVTT subtitle file:

```bash
mkv_remuxer -i video.webm -i subtitles.vtt output.mkv
```

### Verbose Output

```bash
mkv_remuxer -v -i input.webm -s 10s --to 20s output.webm
```

## CLI Reference

| Flag | Description |
|------|-------------|
| `-i <file>` | Input file (repeatable). Supports `.mkv`, `.webm`, `.vtt`, `.webvtt` |
| `-s <time>` / `--ss` | Start position (e.g. `5s`, `1m30s`, `1:30`, `90`) |
| `-t <time>` / `--duration` | Duration from start. Mutually exclusive with `--to` |
| `--to <time>` | End position. Mutually exclusive with `-t` |
| `--seek-mode <mode>` | `snap` (default), `snap_prev`, `squeeze`, `dirty` |
| `-m <src:track>` | Track mapping (repeatable, 0-based indices) |
| `-o <file>` | Output file (positional, required). Extension determines format |
| `-v` / `--verbose` | Enable debug logging |

### Time Formats

All time arguments accept: `5s`, `1m30s`, `2h15m`, `1:30` (MM:SS), `1:30:00` (HH:MM:SS), or a plain number of seconds (`90`).

## Seek Modes Explained

- **`snap`** (`SnapNearestKeyframe`): Seeks to whichever keyframe is closest (before or after) to the requested time. Fast — relies on the Cues index when available.

- **`snap_prev`** (`SnapPreviousKeyframe`): Always seeks to the keyframe *before* the requested time. Useful when you need the output to begin no later than the requested position.

- **`squeeze`** (`Squeeze`): Includes the pre-roll frames from the previous keyframe up to the cut point, marking them invisible and compressing them to t=0. The player sees a seamless start at exactly the requested time.

- **`dirty`** (`DirtyCut`): Cuts strictly at the requested timestamps, discarding frames outside the range regardless of keyframe boundaries. Fast but may produce decoding artifacts near the cut point.

## Library API

The crate exposes a `Remuxer` struct for streaming, cluster-by-cluster processing, plus a `remux()` convenience function for one-shot use:

```rust
use mkv_remuxer::{remux, CutInterval, TrackMapping};
use mkv_remuxer::source::{FileSource, InputSource, SeekType};
use mkv_remuxer::sink::{FileSink, OutputSink};

let source = InputSource::from(FileSource::new("input.webm")?);
let sink = OutputSink::from(FileSink::new("output.webm")?);
let cut = CutInterval::new().with_start(10_000_000_000).with_end(20_000_000_000);

let stats = remux(vec![source], sink, Some(cut), Some(SeekType::SnapNearestKeyframe), None)?;
println!("Processed {} blocks in {:.2}s", stats.blocks_processed, stats.duration_ns as f64 / 1e9);
```

For streaming use cases (e.g. HTTP), use `StreamSink` with `Remuxer::new()` + `Remuxer::process()` — see `examples/stream_server.rs`.

## Architecture

```
InputSource<Uninitialized>  →  initialize_with_cut()  →  InputSource<Initialized>
                                                                    ↓
                                                            SourcesMappings
                                                                    ↓
                                                              MeltingPot
                                                           (cluster ordering,
                                                            timestamp merging)
                                                                    ↓
OutputSink<Uninitialized>   →    initialize()          →   OutputSink<Initialized>
                                                                    ↓
                                                          FileSink / StreamSink / VttSink
```

## Requirements

- Rust 1.80 or higher (uses `if let` chains)
- Input files must be valid MKV/WebM containers

## License

This project is provided as-is for educational and research purposes.
