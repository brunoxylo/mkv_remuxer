# Firefox `NS_ERROR_DOM_MEDIA_DEMUXER_ERR` — Root Cause Analysis

## Problem

When playing a remuxed WebM stream via the advanced server example in Firefox, a playback error occurs near the end of the video:

```
Media resource could not be decoded, error: Error Code: NS_ERROR_DOM_MEDIA_DEMUXER_ERR (0x806e000c)
```

The same original source file (`test_av1.webm`) plays flawlessly in Firefox.

---

## Investigation Summary

### What was compared

| Property | Original (`test_av1.webm`) | Remuxed (`stream.webm`) |
|---|---|---|
| Segment size | **Known** (2,583,995 bytes) | **Unknown** (`0x01FFFFFFFFFFFFFF`) |
| SeekHead | Present | **Missing** |
| Cues | Present (after last cluster) | **Missing** |
| Duration metadata | 30.42s | 30.42s |
| Last frame timestamp | 30.393s | 30.393s |
| `start_time` | 0.000s | **0.007s** |
| DocType | `webm` (4 bytes) | `webm\x00` (5 bytes, **null-terminated**) |
| BlockGroups | 7 (6 subtitle + 1 audio) | 1 (audio only) |
| Cluster count | 11 | 6 |

### Key structural differences in the last BlockGroup

The last audio frame in both files uses a `BlockGroup` (element `0xA0`) instead of a `SimpleBlock`. This is standard for the final Opus audio packet.

**Original file's last BlockGroup:**
```
BlockGroup (0xA0), size=328:
  Block (0xA1), size=318: track=2, rel_tc=3332
  0x75A2, size=4, data=00 e2 56 b5
```

**Remuxed file's last BlockGroup:**
```
BlockGroup (0xA0), size=330:
  Block (0xA1), size=318: track=2, rel_tc=3336
  ReferencePriority (0xFA), size=1, data=00        ← EXTRA ELEMENT
  0x75A2, size=3, data=e2 56 b5                    ← CORRUPTED DATA (missing leading 0x00)
```

---

## Identified Issues (Ranked by Likelihood)

### 1. 🔴 `ReferencePriority` (0xFA) written in WebM BlockGroup

The `mkv_element` library's [`BlockGroup`](src/master.rs) struct has `ReferencePriority` as a **required** field (with default value 0). When serializing, it always writes this element — even when the value is 0 (the default).

- **`ReferencePriority` (0xFA) is a Matroska-only element** — it is NOT part of the WebM specification
- Firefox's WebM demuxer is strict and may reject unknown elements inside `BlockGroup`
- The original file (created by ffmpeg) does NOT write `ReferencePriority` when the value is 0

**Fix:** The `mkv_element` library should skip writing `ReferencePriority` when its value is 0 (the default), especially for WebM output. Alternatively, `mkv_remuxer` could strip this element before writing.

### 2. 🔴 `0x75A2` (BlockMore) data corruption

The original file has `0x75A2` with 4 bytes of data: `00 e2 56 b5`. The remuxed file has `0x75A2` with only 3 bytes: `e2 56 b5`. The leading `0x00` byte has been "stolen" by the `ReferencePriority` element.

This suggests the `mkv_element` library is **misinterpreting the original BlockGroup's binary data** during deserialization:
- It reads the `Block` element correctly
- It then encounters `0x75A2` which is `BlockMore` — but `BlockMore` is NOT a direct child of `BlockGroup` in the Matroska spec (it should be inside `BlockAdditions` / `0x75A1`)
- The library may be misreading the remaining bytes, interpreting the first byte (`0x00`) as `ReferencePriority` data and the rest as `0x75A2`

**Fix:** This is likely a bug in the `mkv_element` library's `BlockGroup` deserialization. It needs to handle non-standard elements (like `0x75A2` appearing directly in `BlockGroup`) gracefully, either by preserving them as raw bytes or by properly wrapping them in `BlockAdditions`.

### 3. 🟡 Unknown Segment size for non-live content

The remuxed file uses unknown Segment size (`0x01FFFFFFFFFFFFFF`), which is standard for **live streaming** but unusual for **downloaded files**. Firefox may handle end-of-stream differently when the Segment size is unknown — it might keep trying to read more data after the last cluster and fail.

The [`StreamSink`](src/sink/stream_sink.rs:56) always writes unknown size:
```rust
self.writer.write_all(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])?;
```

**Fix:** For the streaming use case this is correct. However, if the downloaded file is being tested locally, this could contribute to the issue. Not likely the primary cause since the original file plays fine with known size.

### 4. 🟡 Missing Cues and SeekHead

The remuxed file has no `Cues` or `SeekHead` elements. While these are optional in WebM, their absence means Firefox cannot seek efficiently and must parse the entire file sequentially. At the end of the file, without Cues, Firefox may have trouble determining when the stream truly ends.

**Fix:** The `StreamSink` intentionally omits these for streaming. For file-based output, `FileSink` should be used instead (which does write Cues).

### 5. 🟡 Null-terminated strings in EBML

The remuxed file has null-terminated strings (`webm\x00`, `mkv_remuxer\x00`, `eng\x00`, `V_AV1\x00`, `A_OPUS\x00`). The EBML/Matroska spec says string element sizes define the string length — null terminators should not be present (or should be ignored). Some strict parsers may reject these.

**Fix:** The `mkv_element` library should not write null terminators in string elements.

### 6. 🟢 Non-zero start time (0.007s)

The remuxed file has `start_time=0.007` while the original has `start_time=0.000`. This is because the first cluster's timecode is 7 (ticks), and the first block in the remuxed file has `rel_tc=7` (absolute 14ms) instead of `rel_tc=0` (absolute 7ms) as in the original. This is a minor timestamp shift from the remuxing process but unlikely to cause the end-of-file error.

---

## Recommended Fix Strategy

### Short-term (in `mkv_remuxer`)

1. **Strip `ReferencePriority` from BlockGroups when writing WebM** — Before writing a cluster, iterate over BlockGroups and remove/zero-out the `ReferencePriority` element if the output format is WebM.

2. **Alternatively, convert BlockGroups to SimpleBlocks** — If the BlockGroup only contains a Block and no essential metadata (like `BlockDuration` or `DiscardPadding`), convert it to a `SimpleBlock` before writing. This avoids the `ReferencePriority` issue entirely.

### Medium-term (in `mkv_element` library)

3. **Fix `BlockGroup` serialization** — `ReferencePriority` should not be written when its value is 0 (the default). The `HAS_DEFAULT_VALUE: bool = true` flag exists but the serialization logic may not be using it to skip default values.

4. **Fix `BlockGroup` deserialization** — Handle non-standard elements like `0x75A2` appearing directly in `BlockGroup` (as ffmpeg writes them) without corrupting the data.

5. **Fix null-terminated strings** — Ensure string elements are written without null terminators.

### Validation

After applying fixes, verify with:
```bash
# Check structure
python cluster_inspector.py output.webm

# Check for demuxer errors
ffprobe -v error output.webm

# Test in Firefox (the ultimate validator)
firefox output.webm
```
