/**
 * DidiMse — MSE-based playback with client-side EBML subtitle extraction.
 * Uses ManagedMediaSource (Safari 17+ / iOS) or MediaSource (Firefox/Chrome).
 *
 * IMPORTANT: Never assign a URL directly to video.src in this class.
 * All media data MUST flow through the MediaSource / SourceBuffer pipeline.
 * Direct src assignment causes the browser to manage its own media loading
 * and retry logic, which we cannot control (stale URLs, no backoff, wrong
 * seek parameters on reconnect).
 *
 * Logging policy: only log when something goes wrong (explicit errors) or
 * when playback stalls. Happy-path state dumps were removed — see
 * DIDIMSE_CLEANUP.md for what was deleted.
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

// ── Tunables ─────────────────────────────────────────────────────────────

/**
 * EARLY_APPEND — stream the FIRST media segment into the SourceBuffer in
 * chunks as its bytes arrive, instead of waiting for the whole segment.
 *
 * Rationale: the first cluster after a seek can be very large (~10s of
 * video). Buffering it fully before appending adds that whole download to
 * seek latency. Because segments are cluster-aligned and MSE's WebM parser
 * buffers partial clusters internally, appending large contiguous chunks
 * mid-cluster is safe and lets playback start much sooner.
 *
 * Set to false to fall back to whole-segment appends everywhere if this
 * ever causes problems.
 */
const EARLY_APPEND_ENABLED = true;

/** Minimum bytes accumulated before an early (partial) append is issued. */
const EARLY_APPEND_CHUNK_SIZE = 256 * 1024;

/** Buffer-ahead cap: pump pauses while we have more than this buffered. */
const BUFFER_AHEAD_LIMIT_SEC = 30;

/** Seek distance above which we snap to the NEAREST keyframe server-side
 *  and adopt whatever time the server returns (no client-side correction). */
const FAR_SEEK_THRESHOLD_SEC = 60;

class DidiMse extends DidiPlayer {
    constructor(videoElement, endpointPath, sessionBase = null) {
        super(videoElement, endpointPath, sessionBase);
        this._seekAbort = null;
        this._activeMediaSource = null;

        // Session state
        this._sessionId = null;
        this._clientId = DidiMse._getOrCreateClientId();
        this._keepaliveInterval = null;

        // Inline subtitle state
        this._inlineSubTrackId = -1;       // original track ID (for mappings request)
        this._inlineSubOutputTrack = -1;   // track number in the OUTPUT stream (for EBML scanner)

        // Stall logging: track playback position so we only warn on real stalls
        this._stallTimer = null;
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
        this._stopKeepalive();
        this._stopStallWatch();
        if (this._seekAbort) this._seekAbort.abort();
        if (this._sessionId) {
            // Fire-and-forget DELETE
            fetch(`${this.sessionBase}/session/${this._sessionId}`, { method: 'DELETE', cache: 'no-store' }).catch(() => { });
            this._sessionId = null;
        }
    }

    /**
     * Start polling GET /session/{id}/step every 60s to keep the server
     * session alive (prevents the 3-minute inactivity timeout).
     * Stops automatically if the server reports the session is gone.
     */
    _startKeepalive() {
        this._stopKeepalive();
        this._keepaliveInterval = setInterval(async () => {
            if (!this._sessionId) {
                this._stopKeepalive();
                return;
            }
            try {
                const res = await fetch(`${this.sessionBase}/session/${this._sessionId}/step`, { cache: 'no-store' });
                if (!res.ok) {
                    console.warn(`[DidiMse] Keepalive: session ${this._sessionId} gone (${res.status}), stopping poll`);
                    this._stopKeepalive();
                }
            } catch (e) {
                console.warn('[DidiMse] Keepalive fetch failed:', e.message);
            }
        }, 60_000);
    }

    _stopKeepalive() {
        if (this._keepaliveInterval) {
            clearInterval(this._keepaliveInterval);
            this._keepaliveInterval = null;
        }
    }

    /**
     * Stall watch: warn when the video element is stuck (not advancing while
     * it should be playing). This replaces the old always-on diagnostic
     * watchdog — we only speak up when playback actually stalls.
     */
    _startStallWatch() {
        this._stopStallWatch();
        let lastTime = this.video.currentTime;
        let stalledSince = null;
        this._stallTimer = setInterval(() => {
            const v = this.video;
            if (v.paused || v.ended || v.readyState >= 4) {
                stalledSince = null;
                lastTime = v.currentTime;
                return;
            }
            if (Math.abs(v.currentTime - lastTime) < 0.05) {
                if (stalledSince === null) {
                    stalledSince = Date.now();
                } else if (Date.now() - stalledSince > 3000) {
                    const buf = v.buffered.length > 0
                        ? `${v.buffered.start(0).toFixed(1)}-${v.buffered.end(v.buffered.length - 1).toFixed(1)}s`
                        : 'empty';
                    console.warn('[DidiMse] Playback stalled —',
                        'currentTime:', v.currentTime.toFixed(2),
                        'readyState:', v.readyState,
                        'networkState:', v.networkState,
                        'buffered:', buf,
                        'error:', v.error ? `code=${v.error.code} "${v.error.message}"` : 'null');
                    stalledSince = Date.now(); // re-warn every 3s while stalled
                }
            } else {
                stalledSince = null;
            }
            lastTime = v.currentTime;
        }, 1000);
    }

    _stopStallWatch() {
        if (this._stallTimer) {
            clearInterval(this._stallTimer);
            this._stallTimer = null;
        }
    }

    _onVideoTrackSet(currentAbsTime) {
        this.seek(currentAbsTime);
    }

    setInlineSubtitleTrack(trackId) {
        this._inlineSubTrackId = trackId;
        this._inlineSubOutputTrack = -1; // will be set in seek() based on mapping position
    }

    async seek(seconds) {
        // ── Choose server cut mode based on seek distance ──────────────
        // Far jump (>60s): snap to NEAREST keyframe server-side and adopt
        //   whatever start_sec the server returns — no client-side correction,
        //   the timeline simply snaps to the keyframe.
        // Near jump (≤60s): snap to the PREVIOUS keyframe server-side, then
        //   seek forward inside the MSE pipeline to the exact requested time.
        const distance = Math.abs(seconds - this.getAbsoluteTime());
        const seekMode = distance > FAR_SEEK_THRESHOLD_SEC ? 'snap' : 'snap_prev';

        let mappings = `${this.activeFileIndex}_${this.activeVideoTrackId}`;
        if (this.activeAudioFileIndex === undefined) this.activeAudioFileIndex = this.activeFileIndex;
        mappings += `,${this.activeAudioFileIndex}_${this.activeAudioTrackId}`;

        if (this._inlineSubTrackId > 0 && this.activeSubtitleFileIndex >= 0) {
            mappings += `,${this.activeSubtitleFileIndex}_${this._inlineSubTrackId}`;
            this._inlineSubOutputTrack = mappings.split(',').length;
        }

        // Abort previous session/seek and stop keepalive
        this._stopKeepalive();
        if (this._seekAbort) this._seekAbort.abort();
        const controller = new AbortController();
        this._seekAbort = controller;

        // Destroy previous session
        if (this._sessionId) {
            fetch(`${this.sessionBase}/session/${this._sessionId}`, { method: 'DELETE', cache: 'no-store' }).catch(() => { });
            this._sessionId = null;
        }
        try {
            // 1. Create session (GET apiBase/start_stream_session?mappings=...&start=...&seek=...)
            const qs = new URLSearchParams({
                client_id: this._clientId,
                mappings: mappings,
                start: String(seconds),
                seek: seekMode,
            });
            const createRes = await fetch(`${this.apiBase}/start_stream_session?${qs}`, {
                signal: controller.signal,
                cache: 'no-store',
            });
            if (!createRes.ok) {
                this._emitError(DidiErrorType.NETWORK, await createRes.text());
                return;
            }
            const session = await createRes.json();
            this._sessionId = session.session_id;
            const mimeType = session.mime_type;
            this.currentSeekOffset = session.start_sec || seconds;
            this._startKeepalive();

            // 2. Get init segment (step 0)
            const initRes = await fetch(`${this.sessionBase}/session/${this._sessionId}/segment`, {
                signal: controller.signal,
                cache: 'no-store',
            });
            if (!initRes.ok) {
                this._emitError(DidiErrorType.NETWORK, `Failed to fetch init segment (HTTP ${initRes.status})`);
                return;
            }
            const initData = new Uint8Array(await initRes.arrayBuffer());
            // WebM must start with 0x1A 0x45 0xDF 0xA3 (EBML header)
            if (initData.length < 4 || initData[0] !== 0x1A || initData[1] !== 0x45 || initData[2] !== 0xDF || initData[3] !== 0xA3) {
                console.error('[DidiMse] Init segment does NOT start with WebM/EBML magic bytes');
                this._emitError(DidiErrorType.DECODE, 'Server returned a malformed init segment (no EBML header)');
                return;
            }

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

            // In snap_prev mode the server started at the PREVIOUS keyframe, so we
            // must seek forward inside the pipeline to the exact requested time.
            // In snap mode we adopt the server's start_sec as-is (timeline snaps).
            const seekOffsetInStream = (seekMode === 'snap_prev' && !isNaN(session.start_sec))
                ? Math.max(0, seconds - session.start_sec) : 0;

            const onCanPlay = () => {
                this.video.removeEventListener('canplay', onCanPlay);
                if (seekOffsetInStream > 0.1) {
                    this.video.currentTime = seekOffsetInStream;
                }
                this.video.play().catch((err) => {
                    console.error('[DidiMse] play() rejected:', err.name, err.message,
                        'readyState:', this.video.readyState,
                        'error:', this.video.error ? `code=${this.video.error.code}` : 'null');
                });
                if (this._inlineSubTrackId <= 0) this.reloadSubtitles();
            };
            const onVideoError = () => {
                const e = this.video.error;
                console.error('[DidiMse] video element error:', e ? `code=${e.code} message="${e.message}"` : 'null');
            };
            this.video.addEventListener('error', onVideoError, { once: true });
            this.video.addEventListener('canplay', onCanPlay);
            this.video.src = objectUrl;

            this._startStallWatch();

            ms.addEventListener('sourceopen', async () => {
                URL.revokeObjectURL(objectUrl);
                let sb;
                try {
                    sb = ms.addSourceBuffer(mimeType);
                } catch (e) {
                    console.error('[DidiMse] addSourceBuffer failed:', e.name, e.message);
                    this._emitError(DidiErrorType.DECODE, 'addSourceBuffer failed: ' + e.message);
                    ms.endOfStream('decode');
                    return;
                }

                sb.addEventListener('error', () => {
                    console.error('[DidiMse] SourceBuffer error —',
                        'ms.readyState:', ms.readyState,
                        'video.error:', this.video.error ? `code=${this.video.error.code}` : 'null');
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

                const scanner = useInlineSubs
                    ? new EbmlSubtitleScanner(this._inlineSubOutputTrack)
                    : null;
                let processedCuesCount = 0;
                const flushScannerCues = () => {
                    if (!scanner) return;
                    const cues = scanner.getCues();
                    const delaySec = (this.subtitleDelayMs || 0) / 1000;
                    for (let i = processedCuesCount; i < cues.length; i++) {
                        const cue = cues[i];
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
                };

                // Helper: append data to SourceBuffer and wait for completion.
                // On QuotaExceededError, evict buffer behind the playhead and retry once.
                const appendToSb = async (data) => {
                    try {
                        if (sb.updating) await new Promise(r => sb.addEventListener('updateend', r, { once: true }));
                        if (controller.signal.aborted || ms.readyState !== 'open') return false;
                        try {
                            sb.appendBuffer(data);
                        } catch (e) {
                            if (e.name === 'QuotaExceededError') {
                                const ct = this.video.currentTime;
                                if (ct > 1 && sb.buffered.length > 0) {
                                    const evictEnd = Math.max(0, ct - 1);
                                    if (evictEnd > sb.buffered.start(0)) {
                                        sb.remove(sb.buffered.start(0), evictEnd);
                                        await new Promise(r => sb.addEventListener('updateend', r, { once: true }));
                                    }
                                }
                                if (controller.signal.aborted || ms.readyState !== 'open') return false;
                                sb.appendBuffer(data);
                            } else {
                                throw e;
                            }
                        }
                        await new Promise(r => sb.addEventListener('updateend', r, { once: true }));
                        return true;
                    } catch (e) {
                        // SourceBuffer invalidated by a concurrent seek — not an error
                        return false;
                    }
                };

                // 4. Append init segment
                if (!(await appendToSb(initData))) {
                    console.error('[DidiMse] Init segment append failed —',
                        'ms.readyState:', ms.readyState,
                        'video.error:', this.video.error ? `code=${this.video.error.code} "${this.video.error.message}"` : 'null');
                    this._emitError(DidiErrorType.DECODE,
                        `Init segment rejected for "${mimeType}". The browser cannot decode this stream.`);
                    return;
                }

                // Give the browser a tick to process the init segment and surface any SB errors
                await new Promise(r => setTimeout(r, 0));
                if (ms.readyState !== 'open') {
                    console.error('[DidiMse] MediaSource closed after init segment —',
                        'ms.readyState:', ms.readyState,
                        'video.error:', this.video.error ? `code=${this.video.error.code} "${this.video.error.message}"` : 'null');
                    this._emitError(DidiErrorType.DECODE,
                        `Init segment rejected for "${mimeType}". The browser cannot decode this stream.`);
                    return;
                }

                /**
                 * Fetch the current segment and append it to the SourceBuffer.
                 *
                 * - streamEarly=true (first segment, if EARLY_APPEND_ENABLED):
                 *   appends in EARLY_APPEND_CHUNK_SIZE pieces as bytes arrive so
                 *   playback can start before the (potentially huge) first
                 *   cluster has fully downloaded.
                 * - On mid-stream failure: the /segment endpoint is idempotent,
                 *   so we simply re-fetch it and discard the bytes we already
                 *   appended, resuming the append from where we stopped.
                 *   Scanner data is fed in lockstep with appends, so the EBML
                 *   subtitle scanner never sees a gap or duplicate.
                 *
                 * Returns true when the segment was fully appended.
                 */
                const fetchAndAppendSegment = async (streamEarly) => {
                    let appended = 0; // bytes of THIS segment already in the SourceBuffer
                    let backoff = 500;
                    for (;;) {
                        if (controller.signal.aborted || ms.readyState !== 'open') return false;
                        let segRes;
                        try {
                            segRes = await fetch(`${this.sessionBase}/session/${this._sessionId}/segment`, {
                                signal: controller.signal,
                                cache: 'no-store',
                            });
                        } catch (e) {
                            if (e.name === 'AbortError') return false;
                            console.warn(`[DidiMse] /segment fetch failed (appended ${appended} bytes so far), retrying in ${backoff}ms:`, e.message);
                            await new Promise(r => setTimeout(r, backoff));
                            backoff = Math.min(backoff * 2, 10000);
                            continue;
                        }
                        if (!segRes.ok) {
                            console.warn(`[DidiMse] /segment error ${segRes.status}, retrying in ${backoff}ms`);
                            await new Promise(r => setTimeout(r, backoff));
                            backoff = Math.min(backoff * 2, 10000);
                            continue;
                        }
                        backoff = 500;

                        // Read the body; skip `appended` bytes already in the SB
                        // (non-zero only when resuming after an interrupted stream).
                        let skip = appended;
                        let pending = new Uint8Array(0); // early-append accumulator
                        const chunks = [];               // whole-segment accumulator
                        let streamFailed = false;

                        try {
                            const reader = segRes.body.getReader();
                            for (;;) {
                                const { done, value } = await reader.read();
                                if (done) break;
                                let chunk = value;
                                if (skip > 0) {
                                    if (chunk.length <= skip) { skip -= chunk.length; continue; }
                                    chunk = chunk.subarray(skip);
                                    skip = 0;
                                }
                                if (streamEarly) {
                                    // Accumulate and append in large contiguous pieces.
                                    // MSE's WebM parser buffers partial clusters internally,
                                    // so mid-cluster appends are safe (segments are cluster-aligned).
                                    const merged = new Uint8Array(pending.length + chunk.length);
                                    merged.set(pending, 0);
                                    merged.set(chunk, pending.length);
                                    pending = merged;
                                    if (pending.length >= EARLY_APPEND_CHUNK_SIZE) {
                                        if (!(await appendToSb(pending))) return false;
                                        if (scanner) { scanner.feed(pending); flushScannerCues(); }
                                        appended += pending.length;
                                        pending = new Uint8Array(0);
                                    }
                                } else {
                                    chunks.push(chunk);
                                }
                            }
                        } catch (e) {
                            if (e.name === 'AbortError') return false;
                            // Stream broke mid-segment — re-request and resume from `appended`
                            console.warn(`[DidiMse] /segment stream interrupted after ${appended} bytes, resuming:`, e.message);
                            streamFailed = true;
                        }
                        if (streamFailed) {
                            await new Promise(r => setTimeout(r, backoff));
                            backoff = Math.min(backoff * 2, 10000);
                            continue;
                        }

                        // Flush remainder
                        if (streamEarly) {
                            if (pending.length > 0) {
                                if (!(await appendToSb(pending))) return false;
                                if (scanner) { scanner.feed(pending); flushScannerCues(); }
                                appended += pending.length;
                            }
                        } else {
                            let total = 0;
                            for (const c of chunks) total += c.length;
                            const segData = new Uint8Array(total);
                            let off = 0;
                            for (const c of chunks) { segData.set(c, off); off += c.length; }
                            if (scanner) { scanner.feed(segData); flushScannerCues(); }
                            if (!(await appendToSb(segData))) return false;
                        }
                        return true;
                    }
                };

                // 5. Session pump: POST /next → GET /segment → append
                let backoff = 500;
                let firstSegment = true;

                while (!controller.signal.aborted && ms.readyState === 'open') {
                    // Throttle: wait while buffer is far ahead of the playhead
                    try {
                        while (sb.buffered.length > 0
                            && sb.buffered.end(sb.buffered.length - 1) - this.video.currentTime > BUFFER_AHEAD_LIMIT_SEC
                            && !controller.signal.aborted && ms.readyState === 'open') {
                            await new Promise(r => setTimeout(r, 500));
                        }
                    } catch (e) {
                        // SourceBuffer invalidated by a concurrent seek
                        break;
                    }

                    // Advance to next segment
                    let nextRes;
                    try {
                        nextRes = await fetch(`${this.sessionBase}/session/${this._sessionId}/next`, {
                            method: 'POST',
                            signal: controller.signal,
                            cache: 'no-store',
                        });
                    } catch (e) {
                        if (e.name === 'AbortError') break;
                        console.warn(`[DidiMse] /next failed, retrying in ${backoff}ms:`, e.message);
                        await new Promise(r => setTimeout(r, backoff));
                        backoff = Math.min(backoff * 2, 10000);
                        continue;
                    }

                    if (nextRes.status === 410) break; // session finished — no more segments
                    if (!nextRes.ok) {
                        console.warn(`[DidiMse] /next error ${nextRes.status}, retrying in ${backoff}ms`);
                        await new Promise(r => setTimeout(r, backoff));
                        backoff = Math.min(backoff * 2, 10000);
                        continue;
                    }
                    backoff = 500; // reset on success

                    await nextRes.json(); // { step } — sequential, nothing to do with it client-side

                    if (!(await fetchAndAppendSegment(firstSegment && EARLY_APPEND_ENABLED))) break;
                    firstSegment = false;
                }

                if (ms.readyState === 'open') {
                    try { ms.endOfStream(); } catch (e) { }
                }
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
