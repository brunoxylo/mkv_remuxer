# Stream Server Example

This example demonstrates how to use `StreamSink` to serve MKV/WebM video content over HTTP with on-the-fly remuxing.

## Features

- **Dynamic cutting**: Specify start and end positions in seconds
- **Audio track selection**: Choose specific audio tracks to include
- **On-the-fly remuxing**: Video is remuxed in-memory based on request parameters
- **No reinventing**: Uses the existing `remux()` function with `MeltingPot` for proper, tested remuxing logic

## Usage

### 1. Start the server

```bash
cargo run --example stream_server
```

The server will start on `http://localhost:3030`

### 2. Make requests

Open the test page in your browser:
```bash
# Open examples/stream_test.html in your browser
xdg-open examples/stream_test.html  # Linux
open examples/stream_test.html      # macOS
start examples/stream_test.html     # Windows
```

Or make direct requests:

**Full video:**
```
http://localhost:3030/video
```

**Cut from 5s to 15s:**
```
http://localhost:3030/video?start=5&end=15
```

**From 10s with audio track 2:**
```
http://localhost:3030/video?start=10&audio_track=2
```

**First 30s with audio track 1:**
```
http://localhost:3030/video?start=0&end=30&audio_track=1
```

## Query Parameters

- `start` - Start position in seconds (default: 0)
- `end` - End position in seconds (default: full duration)
- `audio_track` - Audio track number to include (default: all audio tracks)

## How It Works

1. **Request handling**: Warp web server receives HTTP request with query parameters
2. **Parameter parsing**: Extracts start/end times and audio track selection
3. **Source setup**: Creates `FileSource` from input file (`test_vp9.webm`)
4. **Cut configuration**: Sets up `CutInterval` for cutting if start/end specified
5. **Track mapping**: Configures track mappings if specific audio track requested
6. **StreamSink**: Uses `StreamSink` with a shared buffer (`Arc<Mutex<Vec<u8>>>`)
7. **Remuxing**: Calls existing `remux()` function which uses `MeltingPot` and `SourcesMappings`
8. **Response**: Returns the buffered video data as HTTP response with `video/webm` content type

## Architecture

```
HTTP Request
    ↓
Warp Handler
    ↓
Parse Parameters → CutInterval + TrackMapping
    ↓
FileSource (input) → InputSource
    ↓
SharedBuffer (Arc<Mutex<Vec<u8>>>) → StreamSink → OutputSink
    ↓                                               ↓
    ↓                                          remux() function
    ↓                                               ↓
    ↓                                    SourcesMappings + MeltingPot
    ↓                                               ↓
    ←───────────────────────────────────────────────┘
    ↓
HTTP Response (video/webm)
```

## Requirements

- Input file: `test_vp9.webm` (or modify the path in `stream_server.rs`)
- Dependencies: warp, tokio, bytes, env_logger

## Notes

- Uses `SeekType::SnapNearestKeyframe` for cutting to ensure proper keyframe alignment
- The entire remuxed video is buffered in memory before sending
- For production use, consider streaming chunks instead of buffering everything
- Uses the battle-tested `remux()` function - no custom block filtering logic
