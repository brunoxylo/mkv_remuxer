/**
 * DidiMse — MSE-based playback with client-side EBML subtitle extraction.
 * Uses ManagedMediaSource (Safari 17+ / iOS) or MediaSource (Firefox/Chrome).
 *
 * IMPORTANT: Never assign a URL directly to video.src in this class.
 * All media data MUST flow through the MediaSource / SourceBuffer pipeline.
 * Direct src assignment causes the browser to manage its own media loading
 * and retry logic, which we cannot control (stale URLs, no backoff, wrong
 * seek parameters on reconnect).
 */

// Resolve the MSE constructor: prefer ManagedMediaSource (iOS Safari 17+), fall back to standard
const _MSE = window.ManagedMediaSource || window.MediaSource;

// Shared EBML VINT length helper (used by DocType patcher and EbmlSubtitleScanner)
function _ebmlVintLength(byte) {
    if (byte & 0x80) return 1;
    if (byte & 0x40) return 2;
    if (byte & 0x20) return 3;
    if (byte & 0x10) return 4;
    if (byte & 0x08) return 5;
    if (byte & 0x04) return 6;
    if (byte & 0x02) return 7;
    if (byte & 0x01) return 8;
    return 0;
}

class DidiMse extends DidiPlayer {
    constructor(videoElement, endpointPath) {
        super(videoElement, endpointPath);
        this._seekAbort = null;
        this._activeMediaSource = null;
        this._firstSeek = true;  // auto-play on first canplay (replaces HTML autoplay attr)

        // Session state
        this._sessionId = null;
        this._clientId = DidiMse._getOrCreateClientId();

        // Inline subtitle state
        this._inlineSubTrackId = -1;       // original track ID (for mappings request)
        this._inlineSubOutputTrack = -1;   // track number in the OUTPUT stream (for EBML scanner)
        this._inlineSubCues = [];
    }

    static _getOrCreateClientId() {
        let id = localStorage.getItem('didi_client_id');
        if (!id) {
            id = crypto.randomUUID();
            localStorage.setItem('didi_client_id', id);
        }
        return id;
    }

    destroy() {
        if (this._seekAbort) this._seekAbort.abort();
        if (this._sessionId) {
            // Fire-and-forget DELETE
            fetch(`/sessions/${this._sessionId}`, { method: 'DELETE' }).catch(() => {});
            this._sessionId = null;
        }
    }

    _onVideoTrackSet(currentAbsTime) {
        this.seek(currentAbsTime);
    }

    setInlineSubtitleTrack(trackId) {
        this._inlineSubTrackId = trackId;
        this._inlineSubOutputTrack = -1; // will be set in seek() based on mapping position
        this._inlineSubCues = [];
    }

    async seek(seconds) {
        console.debug(`[DidiMse] seek() called with seconds=${seconds}`);
        const wasPlaying = !this.video.paused;

        let seekMode = 'snap_prev';
        let mappings = `${this.activeFileIndex}_${this.activeVideoTrackId}`;
        if (this.activeAudioFileIndex === undefined) this.activeAudioFileIndex = this.activeFileIndex;
        mappings += `,${this.activeAudioFileIndex}_${this.activeAudioTrackId}`;

        if (this._inlineSubTrackId > 0 && this.activeSubtitleFileIndex >= 0) {
            mappings += `,${this.activeSubtitleFileIndex}_${this._inlineSubTrackId}`;
            this._inlineSubOutputTrack = mappings.split(',').length;
        }

        // Abort previous session/seek
        if (this._seekAbort) this._seekAbort.abort();
        const controller = new AbortController();
        this._seekAbort = controller;

        // Destroy previous session
        if (this._sessionId) {
            fetch(`/sessions/${this._sessionId}`, { method: 'DELETE' }).catch(() => {});
            this._sessionId = null;
        }

        try {
            // 1. Create session
            const streamParams = new URLSearchParams({
                client_id: this._clientId,
                mappings: mappings,
                start: seconds.toString(),
                seek: seekMode,
            });
            const createRes = await fetch(`${this.apiBase}/start_stream_session?${streamParams}`, {
                signal: controller.signal,
            });
            if (!createRes.ok) {
                this._emitError(DidiErrorType.NETWORK, await createRes.text());
                return;
            }
            const session = await createRes.json();
            this._sessionId = session.session_id;
            const mimeType = session.mime_type;
            this.currentSeekOffset = session.start_sec || seconds;
            console.info(`[DidiMse] Session created: ${session.session_id}, mime: ${mimeType}`);

            // 2. Get init segment (step 0)
            const initRes = await fetch(`/sessions/${this._sessionId}/segment`, {
                signal: controller.signal,
            });
            if (!initRes.ok) {
                this._emitError(DidiErrorType.NETWORK, 'Failed to fetch init segment');
                return;
            }
            const initData = new Uint8Array(await initRes.arrayBuffer());

            this._inlineSubCues = [];
            this.video.querySelectorAll('track').forEach(t => {
                if (t === this._dynamicTrackEl) return;
                if (t.track && t.track.mode !== 'hidden') t.track.mode = 'hidden';
                t.remove();
            });
            if (this._dynamicTrack && this._dynamicTrack.cues) {
                Array.from(this._dynamicTrack.cues).forEach(c => this._dynamicTrack.removeCue(c));
            }
            if (this._subtitleBlobUrl) {
                URL.revokeObjectURL(this._subtitleBlobUrl);
                this._subtitleBlobUrl = null;
            }

            if (!_MSE || !_MSE.isTypeSupported(mimeType)) {
                this._emitError(DidiErrorType.DECODE, `MSE does not support "${mimeType}"`);
                return;
            }

            // 3. Set up MediaSource
            const ms = new _MSE();
            this._activeMediaSource = ms;
            const objectUrl = URL.createObjectURL(ms);

            const seekOffsetInStream = (seekMode === 'snap_prev' && !isNaN(session.start_sec))
                ? Math.max(0, seconds - session.start_sec) : 0;

            const onCanPlay = () => {
                this.video.removeEventListener('canplay', onCanPlay);
                if (seekOffsetInStream > 0.1) this.video.currentTime = seekOffsetInStream;
                if (wasPlaying || this._firstSeek) {
                    this._firstSeek = false;
                    this.video.play().catch(() => {});
                }
                if (this._inlineSubTrackId <= 0) this.reloadSubtitles();
            };
            this.video.addEventListener('canplay', onCanPlay);
            this.video.src = objectUrl;

            ms.addEventListener('sourceopen', async () => {
                URL.revokeObjectURL(objectUrl);
                let sb;
                try {
                    sb = ms.addSourceBuffer(mimeType);
                } catch (e) {
                    this._emitError(DidiErrorType.DECODE, 'addSourceBuffer failed: ' + e.message);
                    ms.endOfStream('decode');
                    return;
                }

                sb.addEventListener('error', (e) => {
                    console.error('[DidiMse] SourceBuffer error:', e);
                });

                const useInlineSubs = this._inlineSubTrackId > 0;
                if (useInlineSubs) {
                    if (!this._dynamicTrackEl || !this.video.contains(this._dynamicTrackEl)) {
                        this._dynamicTrackEl = document.createElement('track');
                        this._dynamicTrackEl.kind = 'subtitles';
                        this._dynamicTrackEl.label = 'Inline Subtitles';
                        this._dynamicTrackEl.srclang = 'und';
                        this._dynamicTrackEl.default = true;
                        this.video.appendChild(this._dynamicTrackEl);
                        this._dynamicTrack = this._dynamicTrackEl.track;
                    }
                    if (this._dynamicTrack) this._dynamicTrack.mode = 'showing';
                    if (this._dynamicTrack && this._dynamicTrack.cues) {
                        Array.from(this._dynamicTrack.cues).forEach(c => this._dynamicTrack.removeCue(c));
                    }
                }

                let scanner = useInlineSubs
                    ? new EbmlSubtitleScanner(this._inlineSubOutputTrack)
                    : null;

                // Helper: append data to SourceBuffer and wait
                const appendToSb = async (data) => {
                    if (sb.updating) await new Promise(r => sb.addEventListener('updateend', r, { once: true }));
                    if (controller.signal.aborted || ms.readyState !== 'open') return false;
                    sb.appendBuffer(data);
                    await new Promise(r => sb.addEventListener('updateend', r, { once: true }));
                    return true;
                };

                // 4. Append init segment
                if (!(await appendToSb(initData))) return;
                console.info('[DidiMse] Init segment appended');

                // 5. Session pump: POST /next → GET /segment → appendBuffer
                let step = 0;
                let backoff = 500;
                let processedCuesCount = 0;

                while (!controller.signal.aborted && ms.readyState === 'open') {
                    // Throttle: wait if buffer is >30s ahead
                    if (sb.buffered.length > 0) {
                        const ahead = sb.buffered.end(sb.buffered.length - 1) - this.video.currentTime;
                        while (ahead > 30 && !controller.signal.aborted && ms.readyState === 'open') {
                            await new Promise(r => setTimeout(r, 1000));
                            if (sb.buffered.length === 0) break;
                            const newAhead = sb.buffered.end(sb.buffered.length - 1) - this.video.currentTime;
                            if (newAhead <= 30) break;
                        }
                    }

                    // Advance to next segment
                    let nextRes;
                    try {
                        nextRes = await fetch(`/sessions/${this._sessionId}/next`, {
                            method: 'POST',
                            signal: controller.signal,
                        });
                    } catch (e) {
                        if (e.name === 'AbortError') break;
                        console.warn(`[DidiMse] /next failed, retrying in ${backoff}ms:`, e.message);
                        await new Promise(r => setTimeout(r, backoff));
                        backoff = Math.min(backoff * 2, 10000);
                        continue;
                    }

                    if (nextRes.status === 410) {
                        // Session finished — no more segments
                        console.info('[DidiMse] Session finished (410 Gone)');
                        break;
                    }
                    if (!nextRes.ok) {
                        console.warn(`[DidiMse] /next error ${nextRes.status}, retrying...`);
                        await new Promise(r => setTimeout(r, backoff));
                        backoff = Math.min(backoff * 2, 10000);
                        continue;
                    }
                    backoff = 500; // reset on success

                    const stepData = await nextRes.json();
                    step = stepData.step;

                    // Fetch the segment data
                    let segRes;
                    try {
                        segRes = await fetch(`/sessions/${this._sessionId}/segment`, {
                            signal: controller.signal,
                        });
                    } catch (e) {
                        if (e.name === 'AbortError') break;
                        console.warn(`[DidiMse] /segment failed, retrying...`);
                        await new Promise(r => setTimeout(r, 1000));
                        continue;
                    }
                    if (!segRes.ok) {
                        console.warn(`[DidiMse] /segment error ${segRes.status}`);
                        continue;
                    }

                    const segData = new Uint8Array(await segRes.arrayBuffer());

                    // Feed to subtitle scanner
                    if (scanner) {
                        scanner.feed(segData);
                        const cues = scanner.getCues();
                        for (let i = processedCuesCount; i < cues.length; i++) {
                            const cue = cues[i];
                            const delaySec = (this.subtitleDelayMs || 0) / 1000;
                            const startSec = (cue.startMs / 1000) + delaySec;
                            const endSec = startSec + (cue.durationMs / 1000);
                            if (endSec > 0) {
                                try {
                                    const vttCue = new VTTCue(Math.max(0, startSec), endSec, cue.text);
                                    if (this._dynamicTrack) this._dynamicTrack.addCue(vttCue);
                                } catch (e) { /* ignore invalid cue */ }
                            }
                        }
                        processedCuesCount = cues.length;
                    }

                    // Append to SourceBuffer
                    if (!(await appendToSb(segData))) break;

                    if (step <= 3) {
                        const bufInfo = sb.buffered.length > 0
                            ? sb.buffered.end(sb.buffered.length - 1).toFixed(1) + 's' : 'none';
                        console.debug(`[DidiMse] Step ${step}: ${segData.byteLength} bytes, buffered: ${bufInfo}`);
                    }
                }

                // End of stream
                if (ms.readyState === 'open') {
                    try { ms.endOfStream(); } catch (e) {}
                }
                console.info(`[DidiMse] Session pump finished at step ${step}`);
            }, { once: true });

        } catch (e) {
            if (e.name !== 'AbortError') {
                this._emitError(DidiErrorType.NETWORK, e.message || 'Unknown playback error');
            }
        }
    }

    selectSubtitle(trackId, fileIndex, delayMs = 0) {
        this.activeSubtitleTrackId = trackId;
        this.activeSubtitleFileIndex = fileIndex;
        this.subtitleDelayMs = delayMs;

        if (trackId === -1) {
            this.setInlineSubtitleTrack(-1);
            this.reloadSubtitles();
            return;
        }

        const file = this.files[fileIndex];
        const subTrack = file ? file.subtitle_tracks.find(t => t.track_id === trackId) : null;
        const codec = subTrack ? subTrack.codec.toLowerCase() : '';
        const isWebVtt = codec.includes('webvtt') || codec.includes('s_text');

        if (isWebVtt) {
            this.setInlineSubtitleTrack(trackId);
            const absTime = this.video.currentTime + (this.currentSeekOffset || 0);
            this.seek(absTime);
        } else {
            this.setInlineSubtitleTrack(-1);
            this.reloadSubtitles();
        }
    }



}


// ═══════════════════════════════════════════════════════════════════════════
// EbmlSubtitleScanner — minimal streaming EBML parser
// ═══════════════════════════════════════════════════════════════════════════
//
// EBML element IDs (these keep their VINT marker bits, unlike sizes):
//   0x1A45DFA3  EBML header
//   0x18538067  Segment        (master, unknown size in streams)
//   0x1549A966  Info           (master, contains TimestampScale)
//   0x1654AE6B  Tracks         (master, skip)
//   0x1F43B675  Cluster        (master, contains blocks)
//   0xE7        Cluster Timestamp
//   0xA3        SimpleBlock    (binary)
//   0xA0        BlockGroup     (master)
//   0xA1        Block          (binary, inside BlockGroup)
//   0x9B        BlockDuration  (uint,   inside BlockGroup)
//   0x2AD7B1    TimestampScale (uint,   inside Info)

class EbmlSubtitleScanner {
    constructor(subtitleTrackId) {
        this._subTrackId = subtitleTrackId;
        this._buf = new Uint8Array(0);
        this._pos = 0;
        this._cues = [];
        this._totalBytesProcessed = 0;

        this._clusterTimestamp = 0;
        this._timestampScale = 1000000; // default: 1ms in ns

        // BlockGroup accumulator
        this._bgBlock = null;      // parsed block header { trackNum, relTimestamp, payload }
        this._bgDuration = null;   // in ticks
    }

    feed(chunk) {
        // Compact: keep only unprocessed bytes + new chunk
        const remaining = this._buf.length - this._pos;
        const combined = new Uint8Array(remaining + chunk.length);
        if (remaining > 0) {
            combined.set(this._buf.subarray(this._pos), 0);
        }
        combined.set(chunk, remaining);
        this._buf = combined;
        this._pos = 0;
        this._parse();
    }

    getCues() {
        return this._cues;
    }

    // ── Master element set (we descend into these) ─────────────────────
    static MASTER_IDS = new Set([
        0x18538067, // Segment
        0x1549A966, // Info
        0x1F43B675, // Cluster
        0xA0,       // BlockGroup
    ]);

    // ── Elements to skip entirely (read size, jump past data) ──────────
    static SKIP_IDS = new Set([
        0x1A45DFA3, // EBML header
        0x1654AE6B, // Tracks
        0x1C53BB6B, // Cues
        0x114D9B74, // SeekHead
        0x1043A770, // Chapters
        0x1254C367, // Tags
        0x1941A469, // Attachments
    ]);

    _parse() {
        while (this._pos < this._buf.length) {
            const saved = this._pos;
            const elem = this._readElementHeader();
            if (!elem) {
                this._pos = saved;
                break;
            }

            const { id, dataSize } = elem;


            // ── Master elements: descend (don't skip their children) ──
            if (EbmlSubtitleScanner.MASTER_IDS.has(id)) {
                if (id === 0x1F43B675) {
                    // Flush any pending BlockGroup from previous cluster
                    this._flushBlockGroup();
                    this._clusterTimestamp = 0;
                }
                if (id === 0xA0) {
                    // Flush previous BlockGroup, start new
                    this._flushBlockGroup();
                    this._bgBlock = null;
                    this._bgDuration = null;

                }
                // For master elements: just continue parsing their children inline
                // (don't advance _pos past them — their children follow immediately)
                continue;
            }

            // ── Skip elements: jump past their data ──
            if (EbmlSubtitleScanner.SKIP_IDS.has(id)) {
                if (dataSize < 0) {
                    this._pos = saved;
                    break;
                }
                this._pos += dataSize;
                continue;
            }

            // ── Leaf elements: need full data available ──
            if (dataSize < 0) {
                // Unknown-size non-master — can't handle
                continue;
            }
            if (this._pos + dataSize > this._buf.length) {
                // Not enough data yet — rewind
                this._pos = saved;
                break;
            }

            const data = this._buf.subarray(this._pos, this._pos + dataSize);
            this._pos += dataSize;

            switch (id) {
                case 0x2AD7B1: // TimestampScale
                    this._timestampScale = this._readUint(data);
                    break;

                case 0xE7: // Cluster Timestamp
                    this._clusterTimestamp = this._readUint(data);
                    break;

                case 0xA3: { // SimpleBlock
                    const parsed = this._parseBlockHeader(data);
                    if (parsed && parsed.trackNum === this._subTrackId) {
                        this._emitSubtitleCue(parsed, null);
                    }
                    break;
                }

                case 0xA1: // Block (inside BlockGroup)
                    this._bgBlock = this._parseBlockHeader(data);
                    break;

                case 0x9B: // BlockDuration
                    this._bgDuration = this._readUint(data);
                    // Check if we can now flush the BlockGroup
                    if (this._bgBlock && this._bgBlock.trackNum === this._subTrackId) {
                        this._flushBlockGroup();
                    }
                    break;

                default:
                    // Unknown leaf element — skip silently
                    break;
            }
        }
    }

    /**
     * Flush accumulated BlockGroup data into a subtitle cue (if it matches our track).
     */
    _flushBlockGroup() {
        if (!this._bgBlock) return;
        if (this._bgBlock.trackNum === this._subTrackId) {
            this._emitSubtitleCue(this._bgBlock, this._bgDuration);
        }
        this._bgBlock = null;
        this._bgDuration = null;
    }

    /**
     * Create a subtitle cue from a parsed block.
     */
    _emitSubtitleCue(parsed, durationTicks) {
        const absTimeTicks = this._clusterTimestamp + parsed.relTimestamp;
        const absTimeMs = (absTimeTicks * this._timestampScale) / 1000000;

        // WebVTT-in-MKV frame format (S_TEXT/WEBVTT):
        //   Line 1: Cue Identifier (may be empty)
        //   Line 2: Cue Settings  (may be empty — e.g. "position:10% align:left")
        //   Line 3+: Cue Payload  (the actual subtitle text)
        //
        // Lines are separated by 0x0A. We need to extract just the payload.
        const rawText = new TextDecoder().decode(parsed.payload);


        // Split on newline — first two "lines" are cue ID and settings
        const lines = rawText.split('\n');
        let cueText;
        if (lines.length >= 3) {
            // Standard WebVTT-in-MKV: skip cue ID (line 0) and settings (line 1)
            cueText = lines.slice(2).join('\n');
        } else if (lines.length === 2) {
            // Maybe no cue ID, just settings + text? Or ID + text?
            cueText = lines[1];
        } else {
            cueText = rawText;
        }

        if (!cueText.trim()) {
            return;
        }

        let durationMs;
        if (durationTicks !== null && durationTicks !== undefined) {
            durationMs = (durationTicks * this._timestampScale) / 1000000;
        } else {
            durationMs = 5000;
        }

        this._cues.push({
            startMs: absTimeMs,
            durationMs: durationMs,
            text: cueText.trim(),
        });
    }

    /**
     * Parse block header: VINT track number, 2-byte signed timestamp, 1 byte flags.
     * Returns { trackNum, relTimestamp, payload } or null.
     */
    _parseBlockHeader(data) {
        if (data.length < 4) return null;

        // Read VINT-coded track number (value only, marker bits stripped)
        const vintLen = this._vintLength(data[0]);
        if (vintLen === 0 || vintLen > data.length) return null;

        let trackNum = data[0] & ((1 << (8 - vintLen)) - 1); // strip VINT marker
        for (let i = 1; i < vintLen; i++) {
            trackNum = (trackNum << 8) | data[i];
        }

        const tsOffset = vintLen;
        if (tsOffset + 3 > data.length) return null;

        // 2-byte signed relative timestamp
        const relTimestamp = (data[tsOffset] << 8 | data[tsOffset + 1]) << 16 >> 16;

        // 1 byte flags
        const headerEnd = tsOffset + 3;
        const payload = data.subarray(headerEnd);

        return { trackNum, relTimestamp, payload };
    }

    // ── Low-level EBML reading ─────────────────────────────────────────

    /**
     * Read element header at this._pos.
     * Returns { id, dataSize } or null if not enough data.
     *
     * IMPORTANT: EBML Element IDs keep their VINT marker bits!
     * (e.g., Cluster ID = 0x1F43B675 — the 0x1F includes the marker)
     * But Element Sizes strip the marker bit to get the actual size value.
     */
    _readElementHeader() {
        if (this._pos >= this._buf.length) return null;

        // ── Read Element ID (keep marker bits) ──
        const idLen = this._vintLength(this._buf[this._pos]);
        if (idLen === 0 || this._pos + idLen > this._buf.length) return null;

        let id = 0;
        for (let i = 0; i < idLen; i++) {
            id = (id * 256) + this._buf[this._pos + i];
        }
        this._pos += idLen;

        // ── Read Element Size (strip marker bit) ──
        if (this._pos >= this._buf.length) return null;
        const sizeLen = this._vintLength(this._buf[this._pos]);
        if (sizeLen === 0 || this._pos + sizeLen > this._buf.length) return null;

        let dataSize = this._buf[this._pos] & ((1 << (8 - sizeLen)) - 1);
        for (let i = 1; i < sizeLen; i++) {
            dataSize = (dataSize * 256) + this._buf[this._pos + i];
        }
        this._pos += sizeLen;

        // Check for "unknown size" — all data bits set to 1
        // For sizeLen=1: mask=0x7F, unknown=127
        // For sizeLen=2: mask=0x3FFF, unknown=16383
        // etc.
        let isUnknown = true;
        const maskByte0 = (1 << (8 - sizeLen)) - 1;
        if ((this._buf[this._pos - sizeLen] & maskByte0) !== maskByte0) {
            isUnknown = false;
        } else {
            for (let i = 1; i < sizeLen; i++) {
                if (this._buf[this._pos - sizeLen + i] !== 0xFF) {
                    isUnknown = false;
                    break;
                }
            }
        }

        if (isUnknown) {
            dataSize = -1;
        }

        return { id, dataSize };
    }

    _vintLength(byte) {
        if (byte & 0x80) return 1;
        if (byte & 0x40) return 2;
        if (byte & 0x20) return 3;
        if (byte & 0x10) return 4;
        if (byte & 0x08) return 5;
        if (byte & 0x04) return 6;
        if (byte & 0x02) return 7;
        if (byte & 0x01) return 8;
        return 0;
    }

    _readUint(data) {
        let val = 0;
        for (let i = 0; i < data.length; i++) {
            val = (val * 256) + data[i];
        }
        return val;
    }
}


window.DidiMse = DidiMse;
