/**
 * DidiErrorType — Error category enum for structured error reporting.
 */
const DidiErrorType = Object.freeze({
    NETWORK:   'network',
    DECODE:    'decode',
    SUBTITLE:  'subtitle',
    PLAYBACK:  'playback',
});

/**
 * DidiError — Lightweight error data structure emitted on the error channel.
 */
class DidiError {
    constructor(type, message) {
        this.type = type;
        this.message = message;
        this.timestamp = Date.now();
    }

    toString() {
        return `[${this.type}] ${this.message}`;
    }
}

/**
 * DidiPlayer — Base class with shared state & track management.
 * Platform-specific playback lives in DidiMse / DidiLegacy.
 */
class DidiPlayer {
    /** Subtitle codecs that can be rendered by the player (WebVTT only). */
    static SUPPORTED_SUBTITLE_CODECS = ['webvtt', 's_text/webvtt'];

    constructor(videoElement, endpointPath) {
        this.video = videoElement;
        this.apiBase = endpointPath;
        this.files = [];
        this.activeFileIndex = -1;
        this.activeVideoTrackId = -1;
        this.activeAudioTrackId = -1;
        this.activeSubtitleTrackId = -1;
        this.activeSubtitleFileIndex = -1;
        this.activeAudioFileIndex = -1;
        this.videoDuration = 0;
        this.currentSeekOffset = 0;

        // Subtitle cache — raw VTT text fetched once per track selection.
        this._subtitleCache = null;       // { fileIndex, trackId, vttText }
        this._subtitleBlobUrl = null;     // current blob URL (revoked on next reload)

        /** @type {EventTarget} — subscribe with player.errors.addEventListener('error', …) */
        this._errorChannel = new EventTarget();
    }

    /** Returns the error channel EventTarget. Listen for 'error' events whose detail is a DidiError. */
    get errors() {
        return this._errorChannel;
    }

    /**
     * Emit a structured error to the error channel.
     * @param {string} type  - One of DidiErrorType values.
     * @param {string} message - Human-readable description.
     */
    _emitError(type, message) {
        const error = new DidiError(type, message);
        this._errorChannel.dispatchEvent(new CustomEvent('error', { detail: error }));
    }

    async loadlibs() {
        const res = await fetch(`${this.apiBase}`);
        if (!res.ok) {
            this._emitError(DidiErrorType.NETWORK, 'Failed to load video list');
            throw new Error('Failed to load video list');
        }
        this.files = await res.json();
        // Exclude thumbnail-only files from track selection
        this.files = this.files.filter(f => {
            const name = (f.file_name || '').toLowerCase();
            return name !== 'thumbnails.mkv' && name !== 'thumbnails.webm';
        });
        console.log('Loaded files:', this.files);
    }

    getAllVideoTracks() {
        const allVideo = [];
        this.files.forEach((file, idx) => {
            file.video_tracks.forEach(t => {
                allVideo.push({ ...t, fileIndex: idx, fileName: file.file_name });
            });
        });
        return allVideo;
    }

    setVideoTrack(trackId, fileIndex) {
        const currentAbsTime = this.activeFileIndex === -1
            ? 0
            : (this.video.currentTime + (this.currentSeekOffset || 0));

        let prevAudioLanguage = null;
        if (this.activeAudioTrackId !== -1 && this.activeAudioFileIndex !== undefined) {
            const prevFile = this.files[this.activeAudioFileIndex];
            if (prevFile) {
                const prevTrack = prevFile.audio_tracks.find(t => t.track_id === this.activeAudioTrackId);
                if (prevTrack) prevAudioLanguage = prevTrack.language || null;
            }
        }

        this.activeFileIndex = fileIndex;
        this.activeVideoTrackId = trackId;

        const aTracks = this.getAudioTracks(fileIndex);
        if (aTracks.length > 0) {
            const langMatch = prevAudioLanguage
                ? aTracks.find(t => (t.language || null) === prevAudioLanguage)
                : null;
            const internal = aTracks.find(t => t.fileIndex === fileIndex);
            const chosen = langMatch || internal || aTracks[0];
            this.activeAudioTrackId = chosen.track_id;
            this.activeAudioFileIndex = chosen.fileIndex;
        } else {
            this.activeAudioTrackId = -1;
            this.activeAudioFileIndex = -1;
        }

        this._onVideoTrackSet(currentAbsTime);
    }

    /** Override in subclass */
    _onVideoTrackSet(currentAbsTime) {
        this.seek(currentAbsTime);
    }

    getAudioTracks(fileIndex) {
        if (!this.files[fileIndex]) return [];
        const mainFile = this.files[fileIndex];
        const mainAudioTracks = mainFile.audio_tracks.map(t => ({ ...t, fileIndex: fileIndex, origin: 'main' }));
        const allAudio = [...mainAudioTracks];

        this.files.forEach((file, idx) => {
            if (idx === fileIndex) return;
            file.audio_tracks.forEach(track => {
                const isDuplicate = mainFile.audio_tracks.some(mainTrack =>
                    (mainTrack.language === track.language) &&
                    (mainTrack.language === track.language)
                );
                if (!isDuplicate) {
                    allAudio.push({ ...track, fileIndex: idx, origin: 'external' });
                }
            });
        });
        return allAudio;
    }

    getSubtitleTracks() {
        const supported = DidiPlayer.SUPPORTED_SUBTITLE_CODECS;
        const groups = {};
        this.files.forEach((file, fIdx) => {
            file.subtitle_tracks.forEach(track => {
                const codec = (track.codec || '').toLowerCase();
                if (!supported.some(c => codec.includes(c))) return;

                const key = `${track.language || 'und'}_${track.forced}`;
                if (!groups[key]) groups[key] = [];
                groups[key].push({ ...track, fileIndex: fIdx, fileSize: file.file_size });
            });
        });
        const result = [];
        for (const key in groups) {
            const candidates = groups[key];
            candidates.sort((a, b) => a.fileSize - b.fileSize);
            result.push(candidates[0]);
        }
        return result;
    }

    selectAudio(trackId, fileIndex) {
        this.activeAudioTrackId = trackId;
        this.activeAudioFileIndex = fileIndex;
    }

    /** Override in subclass */
    selectSubtitle(_trackId, _fileIndex, _delayMs = 0) {
        throw new Error('selectSubtitle() must be implemented by subclass');
    }

    /** Override in subclass */
    async seek(_seconds) {
        throw new Error('seek() must be implemented by subclass');
    }

    async reloadSubtitles() {
        this.video.querySelectorAll('track').forEach(t => {
            if (t.track && t.track.mode !== 'hidden') t.track.mode = 'hidden';
            t.remove();
        });
        if (this._subtitleBlobUrl) {
            URL.revokeObjectURL(this._subtitleBlobUrl);
            this._subtitleBlobUrl = null;
        }
        if (this.activeSubtitleTrackId === -1) return;

        try {
            const cacheHit = this._subtitleCache
                && this._subtitleCache.fileIndex === this.activeSubtitleFileIndex
                && this._subtitleCache.trackId === this.activeSubtitleTrackId;

            let rawVtt;
            if (cacheHit) {
                rawVtt = this._subtitleCache.vttText;
            } else {
                const map = `${this.activeSubtitleFileIndex}_${this.activeSubtitleTrackId}`;
                const url = `${this.apiBase}/remux?mappings=${map}&vtt_output=true`;
                const res = await fetch(url);
                if (!res.ok) throw new Error(`Subtitle fetch failed: ${res.status}`);
                rawVtt = await res.text();
                this._subtitleCache = {
                    fileIndex: this.activeSubtitleFileIndex,
                    trackId: this.activeSubtitleTrackId,
                    vttText: rawVtt,
                };
            }

            const blob = new Blob([rawVtt], { type: 'text/vtt' });
            this._subtitleBlobUrl = URL.createObjectURL(blob);

            let lang = 'und';
            if (this.files[this.activeSubtitleFileIndex]) {
                const st = this.files[this.activeSubtitleFileIndex].subtitle_tracks
                    .find(t => t.track_id === this.activeSubtitleTrackId);
                if (st) lang = st.language || 'und';
            }

            const track = document.createElement('track');
            track.kind = 'subtitles';
            track.label = lang;
            track.srclang = lang;
            track.src = this._subtitleBlobUrl;
            track.default = true;
            this.video.appendChild(track);

            if (track.track) track.track.mode = 'showing';
            track.addEventListener('load', () => {
                if (track.track) track.track.mode = 'showing';
            });
            const currentSrc = track.src;
            track.src = '';
            track.src = currentSrc;
        } catch (e) {
            this._emitError(DidiErrorType.SUBTITLE, e.message || 'Subtitle load failed');
        }
    }



    getDuration() {
        if (this.activeFileIndex === -1) return 0;
        return this.files[this.activeFileIndex].duration_ms / 1000;
    }

    /** Get the absolute playback time in seconds (accounts for seek offsets in MSE mode). */
    getAbsoluteTime() {
        return this.video.currentTime + (this.currentSeekOffset || 0);
    }

    /**
     * Factory: create the right DidiPlayer subclass for this browser.
     * @param {HTMLVideoElement} videoElement
     * @param {string} endpointPath
     * @param {Function|null} forceClass - pass DidiMse or DidiLegacy to override auto-detection
     * @returns {DidiPlayer}
     */
    static load(videoElement, endpointPath, forceClass = null) {
        const PlayerClass = forceClass || DidiPlayer.detectPlatform();
        console.log(`[DidiPlayer] Loading ${PlayerClass.name}`);
        return new PlayerClass(videoElement, endpointPath);
    }

    /**
     * Detect the appropriate subclass based on MSE + WebM support.
     * Uses DidiMse if MediaSource or ManagedMediaSource supports WebM,
     * falls back to DidiLegacy (direct src) otherwise.
     */
    static detectPlatform() {
        const MSE = window.ManagedMediaSource || window.MediaSource;
        if (MSE && MSE.isTypeSupported('video/webm; codecs="vp9,opus"')) {
            return DidiMse;
        }
        return DidiLegacy;
    }
}

window.DidiPlayer = DidiPlayer;
window.DidiErrorType = DidiErrorType;
window.DidiError = DidiError;
