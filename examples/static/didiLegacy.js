/**
 * DidiLegacy — Fallback playback using direct src with native range requests.
 *
 * Used when MSE is unavailable or as a debug/testing alternative:
 *  - Video/Audio from the SAME file: single <video> element with direct src.
 *  - Audio from a DIFFERENT file: mute the <video>, spawn a hidden <audio>
 *    element pointing to the other file. Sync play/pause/seek.
 *  - Subtitles: always fetched via the server's VTT remux endpoint
 *    (the base class reloadSubtitles handles this).
 */
class DidiLegacy extends DidiPlayer {
    constructor(videoElement, endpointPath) {
        super(videoElement, endpointPath);
        this._externalAudio = null;
        this._syncListenersAttached = false;

        // Catch media errors (e.g., WebM codec issues on non-Safari browsers)
        this.video.addEventListener('error', () => {
            const err = this.video.error;
            if (err) {
                this._emitError(DidiErrorType.DECODE,
                    `Video decode error: code=${err.code} ${err.message || ''}`);
            }
        });
    }

    // ── Video track selection ──────────────────────────────────────────

    _onVideoTrackSet(currentAbsTime) {
        try {
            this.video.src = `${this.apiBase}/direct/${this.activeFileIndex}`;
            this._applyAudioSource();

            const seekTarget = currentAbsTime;
            if (seekTarget > 0) {
                const onLoaded = () => {
                    this.video.removeEventListener('loadedmetadata', onLoaded);
                    this.video.currentTime = seekTarget;
                };
                this.video.addEventListener('loadedmetadata', onLoaded);
            }

            this.reloadSubtitles();
        } catch (e) {
            this._emitError(DidiErrorType.PLAYBACK, 'Video track setup failed: ' + e.message);
        }
    }

    // ── Seek ───────────────────────────────────────────────────────────

    async seek(seconds) {
        try {
            this.video.currentTime = seconds;
            if (this._externalAudio) {
                this._externalAudio.currentTime = seconds;
            }
        } catch (e) {
            this._emitError(DidiErrorType.PLAYBACK, 'Seek failed: ' + e.message);
        }
    }

    // ── Audio selection ────────────────────────────────────────────────

    selectAudio(trackId, fileIndex) {
        this.activeAudioTrackId = trackId;
        this.activeAudioFileIndex = fileIndex;
        this._applyAudioSource();
    }

    // ── Subtitle selection ─────────────────────────────────────────────
    // Uses server-side VTT conversion via the inherited reloadSubtitles().

    selectSubtitle(trackId, fileIndex) {
        this.activeSubtitleTrackId = trackId;
        this.activeSubtitleFileIndex = fileIndex;
        this.reloadSubtitles();
    }

    /**
     * Decide whether to use the video element's native audio or an external <audio>.
     */
    _applyAudioSource() {
        const sameFile = (this.activeAudioFileIndex === this.activeFileIndex);

        if (sameFile) {
            // Audio comes from the same file as video — use native playback.
            this._removeExternalAudio();
            this.video.muted = false;
        } else {
            // Audio is from a different file — mute video, spawn <audio>.
            this.video.muted = true;
            this._createExternalAudio();
        }
    }

    /**
     * Create (or update) the hidden <audio> element for cross-file audio.
     */
    _createExternalAudio() {
        try {
            if (!this._externalAudio) {
                this._externalAudio = document.createElement('audio');
                this._externalAudio.style.display = 'none';
                this._externalAudio.addEventListener('error', () => {
                    this._emitError(DidiErrorType.DECODE, 'External audio decode error');
                });
                document.body.appendChild(this._externalAudio);
            }

            this._externalAudio.src = `${this.apiBase}/direct/${this.activeAudioFileIndex}`;
            this._externalAudio.currentTime = this.video.currentTime;
            this._attachSyncListeners();
        } catch (e) {
            this._emitError(DidiErrorType.PLAYBACK, 'External audio setup failed: ' + e.message);
        }
    }

    /**
     * Remove the external <audio> element and clean up listeners.
     */
    _removeExternalAudio() {
        if (this._externalAudio) {
            this._externalAudio.pause();
            this._externalAudio.removeAttribute('src');
            this._externalAudio.load(); // release resources
            this._externalAudio.remove();
            this._externalAudio = null;
        }
        this._detachSyncListeners();
    }

    // ── Play/Pause/Seek sync between <video> and <audio> ──────────────

    _attachSyncListeners() {
        if (this._syncListenersAttached) return;
        this._syncListenersAttached = true;

        this._onVideoPlay = () => {
            if (this._externalAudio && this._externalAudio.paused) {
                this._externalAudio.currentTime = this.video.currentTime;
                this._externalAudio.play().catch(() => {});
            }
        };
        this._onVideoPause = () => {
            if (this._externalAudio && !this._externalAudio.paused) {
                this._externalAudio.pause();
                this._externalAudio.currentTime = this.video.currentTime;
            }
        };
        this._onVideoSeeked = () => {
            if (this._externalAudio) {
                this._externalAudio.currentTime = this.video.currentTime;
            }
        };
        this.video.addEventListener('play', this._onVideoPlay);
        this.video.addEventListener('pause', this._onVideoPause);
        this.video.addEventListener('seeked', this._onVideoSeeked);

        // Periodic drift correction every 5s: force re-sync if >500ms apart
        this._driftCheckInterval = setInterval(() => {
            if (!this._externalAudio || this.video.paused) return;
            const drift = Math.abs(this.video.currentTime - this._externalAudio.currentTime);
            if (drift > 0.5) {
                this._externalAudio.currentTime = this.video.currentTime;
            }
        }, 5000);
    }

    _detachSyncListeners() {
        if (!this._syncListenersAttached) return;
        this._syncListenersAttached = false;

        this.video.removeEventListener('play', this._onVideoPlay);
        this.video.removeEventListener('pause', this._onVideoPause);
        this.video.removeEventListener('seeked', this._onVideoSeeked);
        if (this._driftCheckInterval) {
            clearInterval(this._driftCheckInterval);
            this._driftCheckInterval = null;
        }
    }



    // ── Cleanup ────────────────────────────────────────────────────────

    destroy() {
        this._removeExternalAudio();
    }
}

window.DidiLegacy = DidiLegacy;
