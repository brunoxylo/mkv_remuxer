# rust mkv_remuxer

A high-performance Rust-based MKV/WebM remuxer with advanced seeking and cutting capabilities.  
Still highly buggy and in experimental state.

## Features

- **Multiple Seek Modes** for precise video cutting:
  - **Squeeze**: Compress pre-roll frames into a 10ms window for seamless playback
  - **Freeze**: Display a freeze frame before the cut point
  - **SnapNearestKeyframe**: Jump to the closest keyframe (fastest, may drift slightly)
  - **DirtyCut**: Hard cut at the specified time (may cause artifacts)

- **Lossless Remuxing**: No re-encoding, preserves original quality
- **Multi-track Support**: Handles video, audio, and subtitle tracks
- **Flexible Track Mapping**: Select specific tracks from multiple sources

## Installation

```bash
cargo build --release
```

## Usage

### Basic Cut

Extract 10 seconds of video starting at 5 seconds:

```bash
playground-element -i input.webm -s 5s -t 10s output.webm
```

### Seek Modes

**Squeeze mode** (recommended for clean cuts):
```bash
playground-element -i input.webm -s 10s -t 20s --seek-mode squeeze output.webm
```

**Freeze mode** (shows freeze frame before cut):
```bash
playground-element -i input.webm -s 10s -t 20s --seek-mode freeze output.webm
```

**Snap mode** (fast, jumps to nearest keyframe):
```bash
playground-element -i input.webm -s 10s -t 20s --seek-mode snap output.webm
```

**Dirty cut** (hard cut, may have artifacts):
```bash
playground-element -i input.webm -s 10s -t 20s --seek-mode dirty output.webm
```

### Verbose Output

Add `-v` for detailed processing information:

```bash
playground-element -v -i input.webm -s 10s -t 20s --seek-mode squeeze output.webm
```

## How It Works

### Seek Modes Explained

- **Squeeze**: Finds the keyframe before the start time, keeps all frames but compresses pre-roll frames into a 10ms invisible window. Results in seamless playback starting exactly at the requested time.

- **Freeze**: Finds the first keyframe after the start time and displays it as a freeze frame until the exact start time is reached.

- **SnapNearestKeyframe**: Jumps to the closest keyframe (before or after) the requested start time. Fastest but may not be frame-accurate.

- **DirtyCut**: Cuts exactly at the requested time, discarding frames outside the range. May result in playback issues if cut doesn't align with keyframes.

## Requirements

- Rust 1.70 or higher
- Input files must be valid MKV/WebM containers

## License

This project is provided as-is for educational and research purposes.
