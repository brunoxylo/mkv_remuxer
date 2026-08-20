# Examples

## stream_server

Simple HTTP server that streams remuxed MKV/WebM via `StreamSink`. No full-file buffering.

```bash
cargo run --example stream_server
# → http://localhost:3030
```

**Requests:**

```
/video?file=myvideo.webm&start=5&end=15&seek=squeeze&tracks=1,2
```

| Param | Description | Default |
|-------|-------------|---------|
| `file` | Basename of file (traversal rejected) | required |
| `start` | Start seconds | `0` |
| `end` | End seconds | full duration |
| `tracks` | Track numbers, comma-separated | all |
| `seek` | `snap`, `snap_prev`, `squeeze`, `dirty` | `snap` |

Response headers: `X-Media-Start-Sec`, `X-Media-End-Sec` report actual cut points after snapping.

---

## advanced_server

Full-featured server: static frontend, file listing, direct (Range) serving, and **session-based chunked streaming** for retry-safe segment delivery.

```bash
cargo run --example advanced_server
# → http://localhost:3031/  (open in browser)
```

**Session API:**

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/my_video` | List media files |
| `GET` | `/my_video/direct/:index` | Direct file serve (Range support) |
| `GET` | `/my_video/start_stream_session?mappings=0_1,1_2&start=10&end=20&seek=squeeze` | Create session |
| `GET` | `/sessions/{id}/segment` | Current segment (idempotent, retry-safe) |
| `POST` | `/sessions/{id}/next` | Advance to next segment |
| `GET` | `/sessions/{id}/step` | Current step index |
| `DELETE` | `/sessions/{id}` | Destroy session |

Mappings format: `source_track` pairs, comma-separated (e.g. `0_1,1_2`).

**Flow:**

```
HTTP → FileSource → InputSource<Uninit>
    → Remuxer::new() → InputSource<Init> + actual CutInterval
    → spawn_blocking loop: Remuxer::process() → StreamSink → mpsc channel
    → Body::wrap_stream → HTTP response (video/webm)
```
