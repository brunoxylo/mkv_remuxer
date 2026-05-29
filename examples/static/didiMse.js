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

        // Inline subtitle state
        this._inlineSubTrackId = -1;       // original track ID (for mappings request)
        this._inlineSubOutputTrack = -1;   // track number in the OUTPUT stream (for EBML scanner)
        this._inlineSubCues = [];
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
        console.trace(`[DidiMse] seek() called with seconds=${seconds}`);
        const wasPlaying = !this.video.paused;

        let currentAbsTime = this.video.currentTime + (this.currentSeekOffset || 0);
        let diff = Math.abs(seconds - currentAbsTime);
        let seekMode = (diff > 60) ? 'snap' : 'snap_prev';

        let mappings = `${this.activeFileIndex}_${this.activeVideoTrackId}`;
        if (this.activeAudioFileIndex === undefined) this.activeAudioFileIndex = this.activeFileIndex;
        mappings += `,${this.activeAudioFileIndex}_${this.activeAudioTrackId}`;

        // If inline subtitle extraction is active, include the subtitle track in mappings
        // The remuxer renumbers tracks based on position: video=1, audio=2, subtitle=3
        if (this._inlineSubTrackId > 0 && this.activeSubtitleFileIndex >= 0) {
            mappings += `,${this.activeSubtitleFileIndex}_${this._inlineSubTrackId}`;
            // Count commas + 1 to get the 1-based output track number for the subtitle
            this._inlineSubOutputTrack = mappings.split(',').length; // = 3
        }

        const url = `${this.apiBase}/remux?mappings=${mappings}&seek=${seekMode}&start=${seconds}`;

        if (this._seekAbort) this._seekAbort.abort();
        const controller = new AbortController();
        this._seekAbort = controller;

        try {
            const res = await fetch(url, {
                signal: controller.signal,
                headers: { 'Accept-Encoding': 'identity' },
            });
            if (!res.ok) {
                const errText = await res.text();
                this._emitError(DidiErrorType.NETWORK, errText);
                return;
            }

            const headerStart = parseFloat(res.headers.get('x-media-start-sec'));
            this.currentSeekOffset = !isNaN(headerStart) ? headerStart : seconds;
            let mimeType = res.headers.get('content-type') || 'video/webm';
            console.log(`[DidiMse] Server Content-Type: "${mimeType}" | MSE supported: ${_MSE ? _MSE.isTypeSupported(mimeType) : 'no MSE'}`);

            this._inlineSubCues = [];
            // Remove only static blob-URL tracks; preserve the dynamic track element.
            this.video.querySelectorAll('track').forEach(t => {
                if (t === this._dynamicTrackEl) return; // keep it
                if (t.track && t.track.mode !== 'hidden') t.track.mode = 'hidden';
                t.remove();
            });
            // Clear cues from the dynamic track so stale subtitles don't linger
            if (this._dynamicTrack && this._dynamicTrack.cues) {
                Array.from(this._dynamicTrack.cues).forEach(c => this._dynamicTrack.removeCue(c));
            }
            if (this._subtitleBlobUrl) {
                URL.revokeObjectURL(this._subtitleBlobUrl);
                this._subtitleBlobUrl = null;
            }

            if (_MSE && _MSE.isTypeSupported(mimeType)) {
                console.log('[DidiMse] ✓ MSE path entered, creating MediaSource…');
                const ms = new _MSE();
                this._activeMediaSource = ms;
                const objectUrl = URL.createObjectURL(ms);

                // With snap_prev the stream starts at the previous keyframe,
                // so we need to seek the video forward to the actual requested time.
                const seekOffsetInStream = (seekMode === 'snap_prev' && !isNaN(headerStart))
                    ? Math.max(0, seconds - headerStart)
                    : 0;

                const onCanPlay = () => {
                    console.log('[DidiMse] ✓ canplay fired');
                    this.video.removeEventListener('canplay', onCanPlay);
                    if (seekOffsetInStream > 0.1) {
                        this.video.currentTime = seekOffsetInStream;
                    }
                    if (wasPlaying) this.video.play().catch(() => { });
                    if (this._inlineSubTrackId <= 0) {
                        this.reloadSubtitles();
                    }
                };
                this.video.addEventListener('canplay', onCanPlay);

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
                this.video.src = objectUrl;
                console.log('[DidiMse] video.src set, waiting for sourceopen…');

                ms.addEventListener('sourceopen', async () => {
                    console.log('[DidiMse] ✓ sourceopen fired, readyState:', ms.readyState);
                    URL.revokeObjectURL(objectUrl);

                    let sb;
                    try {
                        sb = ms.addSourceBuffer(mimeType);
                        console.log('[DidiMse] ✓ addSourceBuffer succeeded for:', mimeType);
                    } catch (e) {
                        this._emitError(DidiErrorType.DECODE, 'addSourceBuffer failed: ' + e.message);
                        ms.endOfStream('decode');
                        return;
                    }

                    // Listen for silent MSE errors (Chrome fires these without throwing)
                    sb.addEventListener('error', (e) => {
                        console.error('[DidiMse] SourceBuffer error event:', e);
                        if (this.video.error) {
                            console.error(`[DidiMse] video.error: code=${this.video.error.code} message="${this.video.error.message}"`);
                        }
                    });
                    ms.addEventListener('sourceclose', () => {
                        console.warn('[DidiMse] MediaSource closed! readyState:', ms.readyState);
                    });
                    ms.addEventListener('sourceended', () => {
                        console.warn('[DidiMse] MediaSource ended! readyState:', ms.readyState);
                    });

                    const useInlineSubs = this._inlineSubTrackId > 0;

                    // Set up the dynamic subtitle track element once if needed
                    if (useInlineSubs) {
                        if (!this._dynamicTrackEl || !this.video.contains(this._dynamicTrackEl)) {
                            this._dynamicTrackEl = document.createElement('track');
                            this._dynamicTrackEl.kind = 'subtitles';
                            this._dynamicTrackEl.label = 'Inline Subtitles';
                            this._dynamicTrackEl.srclang = 'und';
                            this._dynamicTrackEl.default = true;
                            this.video.appendChild(this._dynamicTrackEl);
                            this._dynamicTrack = this._dynamicTrackEl.track;
                            console.log('[DidiMse Subs] Created <track> element. track:', this._dynamicTrack);
                        }
                        if (this._dynamicTrack) this._dynamicTrack.mode = 'showing';
                        // Clear stale cues from previous seek
                        if (this._dynamicTrack && this._dynamicTrack.cues) {
                            Array.from(this._dynamicTrack.cues).forEach(c => this._dynamicTrack.removeCue(c));
                        }
                    }

                    // ── MSE pump with chunk accumulation ──
                    // Chrome's ChunkDemuxer is stricter than Firefox's: it needs complete
                    // parseable EBML elements per appendBuffer call, and the first append
                    // MUST contain the full initialization segment (EBML header + Segment
                    // + Info + Tracks).  We accumulate small network chunks into a pending
                    // buffer and only call appendBuffer once we have enough data.
                    //
                    // Chrome MSE also REQUIRES the Segment element to use unknown/
                    // indeterminate size.  File-oriented remuxers write a known size;
                    // Firefox accepts both, Chrome does not.
                    //
                    // scanner: optional EbmlSubtitleScanner — fed inline so it's throttled by the pump

                    const MIN_INITIAL_APPEND = 32 * 1024;  // 32KB — ensures full init segment
                    const MIN_APPEND = 64 * 1024;  // 64KB — subsequent appends
                    const MAX_APPEND = 1024 * 1024; // 1MB  — split very large appends

                    /**
                     * Patch the EBML init segment for Chrome MSE compatibility:
                     *  1. If DocType is "matroska", rewrite to "webm".
                     *  2. If the Segment element has a known size, rewrite to unknown.
                     *  3. Void out Matroska-only elements inside Info that Chrome's
                     *     strict WebM parser rejects (SegmentUID, DateUTC, etc.).
                     *     The WebM spec only allows: TimestampScale, Duration,
                     *     MuxingApp, WritingApp inside Info.
                     */
                    const patchForMse = (buf) => {
                        const out = new Uint8Array(buf);  // copy so we can mutate

                        // Helper: read EBML element ID at `pos`, return {id, len} or null
                        const readElementId = (data, pos) => {
                            if (pos >= data.length) return null;
                            const b = data[pos];
                            let idLen;
                            if (b & 0x80) idLen = 1;
                            else if (b & 0x40) idLen = 2;
                            else if (b & 0x20) idLen = 3;
                            else if (b & 0x10) idLen = 4;
                            else return null;
                            if (pos + idLen > data.length) return null;
                            let id = 0;
                            for (let j = 0; j < idLen; j++) id = (id << 8) | data[pos + j];
                            return { id, len: idLen };
                        };

                        // Helper: read EBML VINT size at `pos`, return {size, len} or null
                        const readVintSize = (data, pos) => {
                            if (pos >= data.length) return null;
                            const sLen = _ebmlVintLength(data[pos]);
                            if (sLen === 0 || pos + sLen > data.length) return null;
                            const mask = (1 << (8 - sLen)) - 1;
                            let size = data[pos] & mask;
                            for (let j = 1; j < sLen; j++) size = (size * 256) + data[pos + j];
                            // Check for "unknown" size (all data bits 1)
                            let isUnknown = (data[pos] & mask) === mask;
                            for (let j = 1; j < sLen && isUnknown; j++) if (data[pos + j] !== 0xFF) isUnknown = false;
                            return { size: isUnknown ? -1 : size, len: sLen };
                        };

                        // Helper: replace element at `pos` with a Void element of same total length
                        const voidElement = (data, pos, totalLen) => {
                            // Void element: ID=0xEC (1 byte), then VINT size, then padding
                            const payloadLen = totalLen - 1; // subtract 1 for the Void ID byte
                            // We need a VINT that encodes payloadLen - vintLen (data bytes).
                            // For simplicity, try 1-byte VINT first (handles up to 126 data bytes = 128 total)
                            let vintLen;
                            if (payloadLen - 1 <= 0x7E) vintLen = 1;
                            else if (payloadLen - 2 <= 0x3FFE) vintLen = 2;
                            else if (payloadLen - 3 <= 0x1FFFFE) vintLen = 3;
                            else vintLen = 4;
                            const dataLen = payloadLen - vintLen;
                            data[pos] = 0xEC;  // Void ID
                            // Write VINT size
                            if (vintLen === 1) {
                                data[pos + 1] = 0x80 | dataLen;
                            } else if (vintLen === 2) {
                                data[pos + 1] = 0x40 | ((dataLen >> 8) & 0x3F);
                                data[pos + 2] = dataLen & 0xFF;
                            } else if (vintLen === 3) {
                                data[pos + 1] = 0x20 | ((dataLen >> 16) & 0x1F);
                                data[pos + 2] = (dataLen >> 8) & 0xFF;
                                data[pos + 3] = dataLen & 0xFF;
                            }
                            // Fill data with zeros (padding)
                            for (let j = 1 + vintLen; j < totalLen; j++) data[pos + j] = 0x00;
                        };

                        // ── 1. Patch DocType ─────────────────────────────────────
                        for (let i = 0; i < Math.min(out.length - 12, 64); i++) {
                            if (out[i] === 0x42 && out[i + 1] === 0x82) {
                                const sizeStart = i + 2;
                                const sizeLen = _ebmlVintLength(out[sizeStart]);
                                if (sizeLen === 0) continue;
                                let docTypeSize = out[sizeStart] & ((1 << (8 - sizeLen)) - 1);
                                for (let j = 1; j < sizeLen; j++) docTypeSize = (docTypeSize << 8) | out[sizeStart + j];
                                const strStart = sizeStart + sizeLen;
                                const strEnd = strStart + docTypeSize;
                                if (strEnd > out.length) continue;
                                const docType = new TextDecoder().decode(out.subarray(strStart, strEnd));
                                console.log(`[DidiMse] EBML DocType: "${docType}" (${docTypeSize} bytes)`);
                                if (docType.replace(/\0+$/, '') === 'matroska') {
                                    console.warn('[DidiMse] Patching DocType "matroska" → "webm"');
                                    const webm = new TextEncoder().encode('webm');
                                    for (let k = 0; k < docTypeSize; k++) {
                                        out[strStart + k] = k < webm.length ? webm[k] : 0;
                                    }
                                }
                                break;
                            }
                        }

                        // ── 2. Patch Segment size to unknown ─────────────────────
                        const segId = [0x18, 0x53, 0x80, 0x67];
                        let segmentContentStart = -1;
                        for (let i = 0; i < Math.min(out.length - 12, 128); i++) {
                            if (out[i] === segId[0] && out[i + 1] === segId[1] &&
                                out[i + 2] === segId[2] && out[i + 3] === segId[3]) {
                                const sizeStart = i + 4;
                                const sv = readVintSize(out, sizeStart);
                                if (!sv) break;
                                segmentContentStart = sizeStart + sv.len;

                                if (sv.size === -1) {
                                    console.log(`[DidiMse] Segment size: unknown (${sv.len}-byte VINT) — OK`);
                                } else {
                                    console.warn(`[DidiMse] Segment has known size ${sv.size} — patching to unknown`);
                                    const mask = (1 << (8 - sv.len)) - 1;
                                    out[sizeStart] = (1 << (8 - sv.len)) | mask;
                                    for (let j = 1; j < sv.len; j++) out[sizeStart + j] = 0xFF;
                                }
                                break;
                            }
                        }

                        // ── 3. Void out Matroska-only elements inside Info ───────
                        // WebM-allowed Info sub-elements:
                        const WEBM_INFO_ALLOWED = new Set([
                            0x2AD7B1,  // TimestampScale
                            0x4489,    // Duration
                            0x4D80,    // MuxingApp
                            0x5741,    // WritingApp
                        ]);
                        // Info element ID = 0x1549A966
                        if (segmentContentStart > 0) {
                            const infoId = readElementId(out, segmentContentStart);
                            if (infoId && infoId.id === 0x1549A966) {
                                const infoSizeV = readVintSize(out, segmentContentStart + infoId.len);
                                if (infoSizeV && infoSizeV.size > 0) {
                                    const infoContentStart = segmentContentStart + infoId.len + infoSizeV.len;
                                    const infoContentEnd = infoContentStart + infoSizeV.size;
                                    let pos = infoContentStart;
                                    let voided = [];
                                    while (pos < infoContentEnd && pos < out.length) {
                                        const elId = readElementId(out, pos);
                                        if (!elId) break;
                                        const elSize = readVintSize(out, pos + elId.len);
                                        if (!elSize || elSize.size < 0) break;
                                        const totalLen = elId.len + elSize.len + elSize.size;

                                        if (!WEBM_INFO_ALLOWED.has(elId.id)) {
                                            voided.push(`0x${elId.id.toString(16).toUpperCase()}`);
                                            voidElement(out, pos, totalLen);
                                        }
                                        pos += totalLen;
                                    }
                                    if (voided.length > 0) {
                                        console.warn(`[DidiMse] Voided ${voided.length} Matroska-only Info element(s): ${voided.join(', ')} — not in WebM spec`);
                                    } else {
                                        console.log('[DidiMse] Info element: all sub-elements are WebM-compatible — OK');
                                    }
                                }
                            }
                        }

                        // ── 4. Void non-WebM elements inside Tracks ────────────
                        // TEMPORARILY DISABLED — testing whether Chrome actually needs this
                        // or if the server-side monotonicity fix was sufficient.
                        if (false) {
                            // Whitelist of element IDs allowed in WebM for each context.
                            // Anything not in the set gets voided.
                            const WEBM_TRACKENTRY = new Set([
                                0xD7,      // TrackNumber
                                0x73C5,    // TrackUID
                                0x83,      // TrackType
                                0xB9,      // FlagEnabled
                                0x88,      // FlagDefault
                                0x55AA,    // FlagForced
                                0x9C,      // FlagLacing
                                0x6DE7,    // MinCache
                                0x6DF8,    // MaxCache
                                0x23E383,  // DefaultDuration
                                0x55EE,    // MaxBlockAdditionID
                                0x41E4,    // BlockAdditionMapping
                                0x536E,    // Name
                                0x22B59C,  // Language
                                0x86,      // CodecID
                                0x63A2,    // CodecPrivate
                                0x258688,  // CodecName
                                0x56AA,    // CodecDelay
                                0x56BB,    // SeekPreRoll
                                0xE0,      // Video (container)
                                0xE1,      // Audio (container)
                                0x6D80,    // ContentEncodings (container)
                                0xEC,      // Void (pass-through)
                            ]);
                            const WEBM_VIDEO = new Set([
                                0x9A,      // FlagInterlaced
                                // 0x9D is FieldOrder — NOT in WebM spec!
                                0x53B8,    // StereoMode
                                0x53C0,    // AlphaMode
                                0xB0,      // PixelWidth
                                0xBA,      // PixelHeight
                                0x54AA,    // PixelCropBottom
                                0x54BB,    // PixelCropTop
                                0x54CC,    // PixelCropLeft
                                0x54DD,    // PixelCropRight
                                0x54B0,    // DisplayWidth
                                0x54BA,    // DisplayHeight
                                0x54B2,    // DisplayUnit
                                0x2EB524,  // AspectRatioType
                                0x55B0,    // Colour (container)
                                0xEC,      // Void
                            ]);
                            const WEBM_AUDIO = new Set([
                                0xB5,      // SamplingFrequency
                                0x78B5,    // OutputSamplingFrequency
                                0x9F,      // Channels
                                0x6264,    // BitDepth
                                0xEC,      // Void
                            ]);
                            const WEBM_COLOUR = new Set([
                                0x55B1,    // MatrixCoefficients
                                0x55B2,    // BitsPerChannel
                                0x55B3,    // ChromaSubsamplingHorz
                                0x55B4,    // ChromaSubsamplingVert
                                0x55B5,    // CbSubsamplingHorz
                                0x55B6,    // CbSubsamplingVert
                                0x55B7,    // ChromaSitingHorz
                                0x55B8,    // ChromaSitingVert
                                0x55B9,    // Range
                                0x55BA,    // TransferCharacteristics
                                0x55BB,    // Primaries
                                0x55BC,    // MaxCLL
                                0x55BD,    // MaxFALL
                                0x55D0,    // MasteringMetadata (container)
                                0xEC,      // Void
                            ]);
                            const WEBM_MASTERING = new Set([
                                0x55D1, 0x55D2, 0x55D3, 0x55D4,  // PrimaryR/G chromaticity
                                0x55D5, 0x55D6,                 // PrimaryB chromaticity
                                0x55D7, 0x55D8,                 // WhitePoint chromaticity
                                0x55D9, 0x55DA,                 // LuminanceMax/Min
                                0xEC,
                            ]);
                            // Containers that need recursive scanning
                            const CONTAINER_WHITELISTS = {
                                0xE0: WEBM_VIDEO,       // Video
                                0xE1: WEBM_AUDIO,       // Audio
                                0x55B0: WEBM_COLOUR,    // Colour
                                0x55D0: WEBM_MASTERING, // MasteringMetadata
                            };

                            const voidNonWebm = (buf, start, end, whitelist, depth) => {
                                let p = start;
                                const voided = [];
                                while (p < end && p < buf.length) {
                                    const eId = readElementId(buf, p);
                                    if (!eId) break;
                                    const eSz = readVintSize(buf, p + eId.len);
                                    if (!eSz || eSz.size < 0) break;
                                    const total = eId.len + eSz.len + eSz.size;
                                    if (p + total > end) break;

                                    if (!whitelist.has(eId.id)) {
                                        voided.push(`0x${eId.id.toString(16).toUpperCase()}`);
                                        voidElement(buf, p, total);
                                    } else if (CONTAINER_WHITELISTS[eId.id]) {
                                        // Recurse into known containers
                                        const childStart = p + eId.len + eSz.len;
                                        const childVoided = voidNonWebm(buf, childStart, childStart + eSz.size,
                                            CONTAINER_WHITELISTS[eId.id], depth + 1);
                                        voided.push(...childVoided);
                                    }
                                    p += total;
                                }
                                return voided;
                            };

                            if (segmentContentStart > 0) {
                                // Find Tracks element (right after Info)
                                // First skip past Info
                                let searchPos = segmentContentStart;
                                const limit = Math.min(out.length, segmentContentStart + 2048);
                                while (searchPos < limit) {
                                    const sId = readElementId(out, searchPos);
                                    if (!sId) break;
                                    const sSz = readVintSize(out, searchPos + sId.len);
                                    if (!sSz) break;
                                    if (sId.id === 0x1654AE6B) {
                                        // Found Tracks
                                        const tracksStart = searchPos + sId.len + sSz.len;
                                        const tracksEnd = tracksStart + sSz.size;
                                        // Walk each TrackEntry
                                        let tp = tracksStart;
                                        const allVoided = [];
                                        while (tp < tracksEnd && tp < out.length) {
                                            const teId = readElementId(out, tp);
                                            if (!teId) break;
                                            const teSz = readVintSize(out, tp + teId.len);
                                            if (!teSz || teSz.size < 0) break;
                                            const teTotal = teId.len + teSz.len + teSz.size;
                                            if (teId.id === 0xAE) {
                                                // TrackEntry — scan its children
                                                const teContentStart = tp + teId.len + teSz.len;
                                                const teContentEnd = teContentStart + teSz.size;
                                                let cp = teContentStart;
                                                while (cp < teContentEnd && cp < out.length) {
                                                    const cId = readElementId(out, cp);
                                                    if (!cId) break;
                                                    const cSz = readVintSize(out, cp + cId.len);
                                                    if (!cSz || cSz.size < 0) break;
                                                    const cTotal = cId.len + cSz.len + cSz.size;
                                                    if (cp + cTotal > teContentEnd) break;

                                                    if (!WEBM_TRACKENTRY.has(cId.id)) {
                                                        allVoided.push(`0x${cId.id.toString(16).toUpperCase()}`);
                                                        voidElement(out, cp, cTotal);
                                                    } else if (CONTAINER_WHITELISTS[cId.id]) {
                                                        const childStart = cp + cId.len + cSz.len;
                                                        const v = voidNonWebm(out, childStart, childStart + cSz.size,
                                                            CONTAINER_WHITELISTS[cId.id], 1);
                                                        allVoided.push(...v);
                                                    }
                                                    cp += cTotal;
                                                }
                                            }
                                            tp += teTotal;
                                        }
                                        if (allVoided.length > 0) {
                                            console.warn(`[DidiMse] Voided ${allVoided.length} non-WebM Tracks element(s): ${allVoided.join(', ')}`);
                                        } else {
                                            console.log('[DidiMse] Tracks: all sub-elements are WebM-compatible — OK');
                                        }
                                        break;
                                    }
                                    if (sSz.size === -1) break;
                                    if (sId.id === 0x1F43B675) break; // hit Cluster, stop
                                    searchPos += sId.len + sSz.len + sSz.size;
                                }
                            }
                        } // end if(false) — Stage 4 disabled

                        return out;
                    };


                    const pumpStream = async (reader, scanner, startProcessedCues = 0) => {
                        let processedCuesCount = startProcessedCues;
                        let chunkCount = 0;
                        let appendCount = 0;
                        let pendingBuf = null;       // accumulated bytes not yet appended
                        let pendingLen = 0;

                        /** Merge `chunk` into pendingBuf. */
                        const accumulate = (chunk) => {
                            if (!pendingBuf) {
                                pendingBuf = new Uint8Array(chunk);
                                pendingLen = chunk.byteLength;
                            } else {
                                const combined = new Uint8Array(pendingLen + chunk.byteLength);
                                combined.set(pendingBuf.subarray(0, pendingLen), 0);
                                combined.set(chunk, pendingLen);
                                pendingBuf = combined;
                                pendingLen = combined.byteLength;
                            }
                        };

                        /** Append the accumulated pending buffer to the SourceBuffer. */
                        const flushPending = async () => {
                            if (!pendingBuf || pendingLen === 0) return;
                            let data = pendingBuf.subarray(0, pendingLen);

                            // On the very first append, patch for Chrome MSE compatibility
                            if (appendCount === 0) {
                                // Diagnostic: log first 512 bytes as hex (enough to cover EBML+Segment+Info+Tracks header)
                                const hexLen = Math.min(512, data.length);
                                const hex = Array.from(data.subarray(0, hexLen))
                                    .map(b => b.toString(16).padStart(2, '0')).join(' ');
                                console.log(`[DidiMse] first append hex (${data.length} bytes, showing ${hexLen}): ${hex}`);

                                // Structured EBML element listing for debugging
                                try {
                                    const readId = (d, p) => {
                                        if (p >= d.length) return null;
                                        const b = d[p];
                                        let l = (b & 0x80) ? 1 : (b & 0x40) ? 2 : (b & 0x20) ? 3 : (b & 0x10) ? 4 : 0;
                                        if (!l || p + l > d.length) return null;
                                        let id = 0;
                                        for (let j = 0; j < l; j++) id = (id << 8) | d[p + j];
                                        return { id, len: l };
                                    };
                                    const readSz = (d, p) => {
                                        if (p >= d.length) return null;
                                        const sl = _ebmlVintLength(d[p]);
                                        if (!sl || p + sl > d.length) return null;
                                        const m = (1 << (8 - sl)) - 1;
                                        let sz = d[p] & m;
                                        for (let j = 1; j < sl; j++) sz = sz * 256 + d[p + j];
                                        let unk = (d[p] & m) === m;
                                        for (let j = 1; j < sl && unk; j++) if (d[p + j] !== 0xFF) unk = false;
                                        return { size: unk ? -1 : sz, len: sl };
                                    };
                                    const NAMES = {
                                        0x1A45DFA3: 'EBML', 0x18538067: 'Segment', 0x1549A966: 'Info',
                                        0x1654AE6B: 'Tracks', 0x1F43B675: 'Cluster', 0x114D9B74: 'SeekHead',
                                        0x1C53BB6B: 'Cues', 0x1254C367: 'Tags', 0x1941A469: 'Attachments',
                                        0xAE: 'TrackEntry', 0xD7: 'TrackNumber', 0x73C5: 'TrackUID',
                                        0x83: 'TrackType', 0x86: 'CodecID', 0xE0: 'Video', 0xE1: 'Audio',
                                        0x63A2: 'CodecPrivate', 0x22B59C: 'Language', 0x536E: 'Name',
                                        0xB0: 'PixelWidth', 0xBA: 'PixelHeight', 0xB5: 'SamplingFrequency',
                                        0x9F: 'Channels', 0x6264: 'BitDepth', 0x56AA: 'CodecDelay',
                                        0x56BB: 'SeekPreRoll', 0x2AD7B1: 'TimestampScale', 0x4489: 'Duration',
                                        0x4D80: 'MuxingApp', 0x5741: 'WritingApp', 0x73A4: 'SegmentUID',
                                        0x4461: 'DateUTC', 0xEC: 'Void', 0xBF: 'CRC-32',
                                        0x55EE: 'MaxBlockAdditionID', 0x55B0: 'Colour', 0x55B1: 'MatrixCoefficients',
                                        0x55B2: 'BitsPerChannel', 0x55B5: 'ChromaSitingHorz',
                                        0x55B6: 'ChromaSitingVert', 0x55B7: 'Range',
                                        0x55B8: 'TransferCharacteristics', 0x55B9: 'Primaries',
                                        0x54B0: 'DisplayWidth', 0x54BA: 'DisplayHeight', 0x54B2: 'DisplayUnit',
                                        0x55AA: 'FlagForced', 0x88: 'FlagDefault', 0xB9: 'FlagEnabled',
                                        0x9C: 'FlagLacing', 0x23E383: 'DefaultDuration',
                                    };
                                    // Walk top-level elements inside Segment
                                    let pos = 0;
                                    // Skip EBML header
                                    const ebmlId = readId(data, pos);
                                    if (ebmlId) {
                                        const ebmlSz = readSz(data, pos + ebmlId.len);
                                        if (ebmlSz && ebmlSz.size > 0) {
                                            console.log(`[DidiMse EBML] EBML header: ${ebmlId.len + ebmlSz.len + ebmlSz.size} bytes`);
                                            pos = ebmlId.len + ebmlSz.len + ebmlSz.size;
                                        }
                                    }
                                    // Segment
                                    const segIdR = readId(data, pos);
                                    if (segIdR) {
                                        const segSzR = readSz(data, pos + segIdR.len);
                                        if (segSzR) {
                                            pos += segIdR.len + segSzR.len; // now at Segment content
                                            // Walk children
                                            const limit = Math.min(data.length, pos + 4096);
                                            while (pos < limit) {
                                                const cId = readId(data, pos);
                                                if (!cId) break;
                                                const cSz = readSz(data, pos + cId.len);
                                                if (!cSz) break;
                                                const name = NAMES[cId.id] || `0x${cId.id.toString(16).toUpperCase()}`;
                                                const sizeStr = cSz.size === -1 ? 'unknown' : cSz.size;
                                                console.log(`[DidiMse EBML]   ${name} (id=0x${cId.id.toString(16)}, size=${sizeStr}) @ byte ${pos}`);

                                                // For Info and Tracks, also list their children
                                                if (cId.id === 0x1549A966 || cId.id === 0x1654AE6B) {
                                                    const contentStart = pos + cId.len + cSz.len;
                                                    const contentEnd = Math.min(contentStart + cSz.size, data.length);
                                                    let cp = contentStart;
                                                    while (cp < contentEnd) {
                                                        const chId = readId(data, cp);
                                                        if (!chId) break;
                                                        const chSz = readSz(data, cp + chId.len);
                                                        if (!chSz || chSz.size < 0) break;
                                                        const chName = NAMES[chId.id] || `0x${chId.id.toString(16).toUpperCase()}`;
                                                        const totalLen = chId.len + chSz.len + chSz.size;
                                                        // For TrackEntry, also show its children
                                                        if (chId.id === 0xAE) {
                                                            console.log(`[DidiMse EBML]     ${chName} (size=${chSz.size}) @ byte ${cp}`);
                                                            let tp = cp + chId.len + chSz.len;
                                                            const tEnd = Math.min(tp + chSz.size, data.length);
                                                            while (tp < tEnd) {
                                                                const tId = readId(data, tp);
                                                                if (!tId) break;
                                                                const tSz = readSz(data, tp + tId.len);
                                                                if (!tSz || tSz.size < 0) break;
                                                                const tName = NAMES[tId.id] || `0x${tId.id.toString(16).toUpperCase()}`;
                                                                // Show value for small elements
                                                                let valStr = '';
                                                                if (tSz.size <= 8 && tSz.size > 0) {
                                                                    const vStart = tp + tId.len + tSz.len;
                                                                    const vBytes = Array.from(data.subarray(vStart, vStart + tSz.size));
                                                                    if (tId.id === 0x86 || tId.id === 0x22B59C || tId.id === 0x536E) {
                                                                        valStr = ` = "${new TextDecoder().decode(data.subarray(vStart, vStart + tSz.size))}"`;
                                                                    } else {
                                                                        let v = 0; vBytes.forEach(b => v = v * 256 + b);
                                                                        valStr = ` = ${v}`;
                                                                    }
                                                                }
                                                                console.log(`[DidiMse EBML]       ${tName} (size=${tSz.size})${valStr}`);
                                                                tp += tId.len + tSz.len + tSz.size;
                                                            }
                                                        } else {
                                                            console.log(`[DidiMse EBML]     ${chName} (size=${chSz.size}) @ byte ${cp}`);
                                                        }
                                                        cp += totalLen;
                                                    }
                                                }
                                                // Stop at first Cluster (we only want the init segment)
                                                if (cId.id === 0x1F43B675) break;
                                                if (cSz.size === -1) break; // unknown size = Cluster
                                                pos += cId.len + cSz.len + cSz.size;
                                            }
                                        }
                                    }
                                } catch (e) { console.warn('[DidiMse EBML] decode error:', e); }

                                data = patchForMse(data);
                            }

                            // Split large buffers into ≤ MAX_APPEND slices
                            for (let offset = 0; offset < data.byteLength; offset += MAX_APPEND) {
                                if (controller.signal.aborted || ms.readyState !== 'open') return;
                                const slice = data.subarray(offset, Math.min(offset + MAX_APPEND, data.byteLength));

                                if (sb.updating) {
                                    await new Promise(r => sb.addEventListener('updateend', r, { once: true }));
                                }
                                if (controller.signal.aborted || ms.readyState !== 'open') return;

                                appendCount++;
                                const hexPreview = Array.from(slice.subarray(0, Math.min(32, slice.length))).map(b => b.toString(16).padStart(2, '0')).join(' ');
                                if (appendCount <= 8) console.log(`[DidiMse] appendBuffer #${appendCount}: ${slice.byteLength} bytes, hex: ${hexPreview}`);
                                sb.appendBuffer(slice);
                                await new Promise(r => sb.addEventListener('updateend', r, { once: true }));
                                if (appendCount <= 8) {
                                    const bufInfo = sb.buffered.length > 0 ? sb.buffered.end(sb.buffered.length - 1).toFixed(1) + 's' : 'none';
                                    console.log(`[DidiMse] appendBuffer #${appendCount} done, buffered: ${bufInfo}, readyState: ${ms.readyState}`);
                                }

                                // Throttle: if buffer is >30s ahead of playhead, wait
                                if (sb.buffered.length > 0) {
                                    let ahead = sb.buffered.end(sb.buffered.length - 1) - this.video.currentTime;
                                    while (ahead > 30 && !controller.signal.aborted && ms.readyState === 'open') {
                                        await new Promise(r => setTimeout(r, 1000));
                                        if (sb.buffered.length === 0) break;
                                        ahead = sb.buffered.end(sb.buffered.length - 1) - this.video.currentTime;
                                    }
                                }
                            }
                            pendingBuf = null;
                            pendingLen = 0;
                        };

                        try {
                            while (true) {
                                if (controller.signal.aborted || ms.readyState !== 'open') {
                                    console.warn(`[DidiMse] pump aborted at loop top (aborted=${controller.signal.aborted}, readyState=${ms.readyState}, chunks=${chunkCount})`);
                                    return 'aborted';
                                }

                                const { done, value } = await reader.read();
                                if (done) {
                                    // Flush any remaining buffered data
                                    await flushPending();
                                    console.log(`[DidiMse] pump done after ${chunkCount} chunks, ${appendCount} appends`);
                                    return 'done';
                                }
                                chunkCount++;
                                if (chunkCount <= 5) console.log(`[DidiMse] chunk #${chunkCount}: ${value.byteLength} bytes`);

                                // Feed chunk to subtitle scanner inline (naturally throttled with pump)
                                if (scanner) {
                                    scanner.feed(value);
                                    const cues = scanner.getCues();
                                    if (cues.length > processedCuesCount) {
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
                                }

                                // Accumulate chunk into pending buffer
                                accumulate(value);

                                // Decide whether to flush: use a larger threshold for the
                                // first append (must contain full init segment) and a smaller
                                // one for subsequent appends.
                                const threshold = (appendCount === 0) ? MIN_INITIAL_APPEND : MIN_APPEND;
                                if (pendingLen >= threshold) {
                                    await flushPending();
                                    if (controller.signal.aborted || ms.readyState !== 'open') {
                                        console.warn(`[DidiMse] pump aborted after flush (readyState=${ms.readyState})`);
                                        return 'aborted';
                                    }
                                }
                            }
                        } catch (e) {
                            if (e.name === 'AbortError' || controller.signal.aborted) { console.warn('[DidiMse] pump caught AbortError'); return 'aborted'; }
                            console.error('[DidiMse] Pump error:', e);
                            return 'error';
                        }
                    };

                    // Create initial subtitle scanner (one per stream segment)
                    let activeScanner = useInlineSubs
                        ? new EbmlSubtitleScanner(this._inlineSubOutputTrack)
                        : null;

                    // Run with reconnection on network errors
                    (async () => {
                        let result = await pumpStream(res.body.getReader(), activeScanner);
                        let backoff = 1000;

                        while (result === 'error' && ms.readyState === 'open' && !controller.signal.aborted) {
                            // Resume from end of buffered data, or current playback position
                            // (NOT the original seek position which may be minutes stale)
                            let resumeAt;
                            if (sb.buffered.length > 0) {
                                resumeAt = sb.buffered.end(sb.buffered.length - 1) + this.currentSeekOffset;
                            } else {
                                resumeAt = this.video.currentTime + (this.currentSeekOffset || 0);
                            }
                            console.log(`[DidiMse] Reconnecting from ${resumeAt.toFixed(1)}s in ${(backoff / 1000).toFixed(0)}s...`);
                            await new Promise(r => setTimeout(r, backoff));
                            backoff = Math.min(backoff * 2, 10000);
                            if (controller.signal.aborted || ms.readyState !== 'open') break;

                            try {
                                const res2 = await fetch(
                                    `${this.apiBase}/remux?mappings=${mappings}&seek=prev_keyframe&start=${resumeAt}`,
                                    { signal: controller.signal, headers: { 'Accept-Encoding': 'identity' } }
                                );
                                if (controller.signal.aborted || ms.readyState !== 'open') break;
                                if (!res2.ok) { continue; }

                                const s = parseFloat(res2.headers.get('x-media-start-sec'));
                                if (!isNaN(s)) {
                                    if (sb.updating) await new Promise(r => sb.addEventListener('updateend', r, { once: true }));
                                    if (controller.signal.aborted || ms.readyState !== 'open') break;
                                    sb.timestampOffset = s - this.currentSeekOffset;
                                }
                                backoff = 1000;
                                // Fresh scanner for the new segment; cues from previous segment remain in the track
                                if (useInlineSubs) {
                                    activeScanner = new EbmlSubtitleScanner(this._inlineSubOutputTrack);
                                }
                                result = await pumpStream(res2.body.getReader(), activeScanner);
                            } catch (err) {
                                if (err.name === 'AbortError' || err.name === 'InvalidStateError' || controller.signal.aborted || ms.readyState !== 'open') break;
                                console.error('[DidiMse] Reconnect failed:', err);
                                result = 'error';
                            }
                        }

                        console.log(`[DidiMse] pump exited with result: "${result}", ms.readyState: ${ms.readyState}`);
                        if (result === 'done' && ms.readyState === 'open') {
                            try { ms.endOfStream(); } catch (e) { }
                        } else if (result === 'error' && ms.readyState === 'open') {
                            this._emitError(DidiErrorType.NETWORK, 'Stream failed');
                            try { ms.endOfStream('network'); } catch (e) { }
                        }
                    })();
                }, { once: true });


            } else {
                // MSE is the only supported path — never fall back to direct video.src.
                controller.abort();
                console.error(
                    `[DidiMse] FATAL: MSE rejected the stream.\n` +
                    `  Content-Type from server: "${mimeType}"\n` +
                    `  _MSE available: ${!!_MSE}\n` +
                    `  isTypeSupported("${mimeType}"): ${_MSE ? _MSE.isTypeSupported(mimeType) : 'N/A'}\n` +
                    `  isTypeSupported("video/webm; codecs=\\"vp9,opus\\""): ${_MSE ? _MSE.isTypeSupported('video/webm; codecs="vp9,opus"') : 'N/A'}\n` +
                    `  → Fix: the backend must send Content-Type with codec parameters ` +
                    `(e.g. 'video/webm; codecs="vp9,opus"') instead of bare "${mimeType}".`
                );
                this._emitError(DidiErrorType.DECODE,
                    `MSE does not support mimeType "${mimeType}". The server must include codec parameters in Content-Type.`);
                return;
            }
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
