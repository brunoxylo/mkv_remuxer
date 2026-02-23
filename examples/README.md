# Stream Server Example

This example demonstrates how to use `StreamSink` to serve MKV/WebM video content over HTTP with on-the-fly remuxing and keyframe-aligned seeking.

## Features

- **Dynamic cutting**: Specify start and end positions in seconds
- **Seek mode selection**: Choose seek behaviour per request
- **Track selection**: Choose specific tracks from the source file
- **True streaming**: Uses `StreamSink` and `Remuxer` for chunk-by-chunk cluster streaming — no full-file buffering
- **Custom headers**: `X-Media-Start-Sec` / `X-Media-End-Sec` report the actual cut point after snapping

## Usage

### 1. Start the server

```bash
cargo run --example stream_server
```

The server will start on `http://localhost:3030`

### 2. Make requests

**Full video:**
```
http://localhost:3030/video
```

**Cut from 5s to 15s:**
```
http://localhost:3030/video?file=myvideo.webm&start=5&end=15
```

**Specific tracks:**
```
http://localhost:3030/video?file=myvideo.webm&start=10&tracks=1,2
```

**With seek mode:**
```
http://localhost:3030/video?file=myvideo.webm&start=10&end=20&seek=squeeze
```

## Query Parameters

| Parameter | Description | Default |
|-----------|-------------|---------|
| `file` | Basename of the file to serve (path traversal is rejected) | required |
| `start` | Start position in seconds | `0` |
| `end` | End position in seconds | full duration |
| `tracks` | Comma-separated track numbers to include (e.g. `1,2`) | all tracks |
| `seek` | Seek mode: `snap_prev`, `squeeze`, `dirty` | `snap` (nearest keyframe) |

## Response Headers

| Header | Description |
|--------|-------------|
| `Content-Type` | `video/webm` |
| `X-Media-Start-Sec` | Actual start after keyframe snapping (seconds) |
| `X-Media-End-Sec` | Actual end after keyframe snapping (seconds), if known |

## How It Works

1. **Request handling**: Warp web server receives request with query parameters
2. **Parameter parsing**: Extracts file name, start/end times, track list, and seek mode
3. **Source setup**: Creates `FileSource` from the requested file
4. **Cut + seek init**: Initialises `Remuxer` with `CutInterval` and chosen `SeekType`
5. **Streaming**: `Remuxer::process()` is called in a `spawn_blocking` loop, writing clusters one at a time into a `tokio::sync::mpsc` channel via `StreamSink`
6. **Response**: Warp wraps the channel receiver as a `Body::wrap_stream`

## Architecture

```
HTTP Request
    ↓
Warp Handler  (parse params, build CutInterval + SeekType + TrackMappings)
    ↓
FileSource → InputSource<Uninitialized>
    ↓
Remuxer::new()  →  InputSource<Initialized>  +  actual CutInterval
    ↓
tokio::task::spawn_blocking loop
    │   Remuxer::process() → writes cluster to StreamSink
    │                                    ↓
    │                             mpsc::Sender<Bytes>
    ↓
mpsc::Receiver  →  Body::wrap_stream  →  HTTP Response (video/webm)
```

## Requirements

- Input files must be placed where the server can read them (path is configurable in `stream_server.rs`)
- Dependencies: `warp`, `tokio`, `bytes`, `env_logger`
