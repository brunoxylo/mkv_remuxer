class DidiPlayer {
    constructor(videoElement) {
        this.video = videoElement;
        this.apiBase = window.location.origin; // or specific backend URL
        this.files = [];
        this.activeFileIndex = -1;
        this.activeVideoTrackId = -1;
        this.activeAudioTrackId = -1;
        this.activeSubtitleTrackId = -1;
        this.activeSubtitleFileIndex = -1;
        this.videoDuration = 0;
        
        // Subtitle cache — raw (unadjusted) VTT text fetched once per track selection.
        // Avoids re-fetching the full subtitle file on every seek.
        this._subtitleCache = null;       // { fileIndex, trackId, vttText }
        this._subtitleBlobUrl = null;     // current blob URL (revoked on next reload)

        // Safari check
        this.isSafari = /^((?!chrome|android).)*safari/i.test(navigator.userAgent);
    }

    async loadlibs() {
        // fetch list of files
        const res = await fetch(`${this.apiBase}/video/list`);
        if (!res.ok) throw new Error("Failed to load video list");
        this.files = await res.json();
        console.log("Loaded files:", this.files);
    }

    getAllVideoTracks() {
        const allVideo = [];
        this.files.forEach((file, idx) => {
            file.video_tracks.forEach(t => {
                allVideo.push({...t, fileIndex: idx, fileName: file.file_name});
            });
        });
        return allVideo;
    }

    setVideoTrack(trackId, fileIndex) {
        // Capture current absolute playback position before switching tracks
        const currentAbsTime = this.activeFileIndex === -1
            ? 0
            : (this.video.currentTime + (this.currentSeekOffset || 0));

        // Remember the language of the currently selected audio track so we
        // can try to preserve it when switching to a different video track.
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
        
        // Try to keep the same audio language across video track switches.
        // Fall back to the first available track only when no language match exists.
        const aTracks = this.getAudioTracks(fileIndex);
        if (aTracks.length > 0) {
            // 1. Try to match the previously selected language
            const langMatch = prevAudioLanguage
                ? aTracks.find(t => (t.language || null) === prevAudioLanguage)
                : null;
            // 2. Otherwise prefer an internal track from the new file
            const internal = aTracks.find(t => t.fileIndex === fileIndex);
            const chosen = langMatch || internal || aTracks[0];
            this.activeAudioTrackId = chosen.track_id;
            this.activeAudioFileIndex = chosen.fileIndex;
        } else {
            this.activeAudioTrackId = -1;
            this.activeAudioFileIndex = -1;
        }

        // Keep subtitle selection intact across video track switches
        // (caller can reset it explicitly if needed)

        if (this.isSafari) {
            // Direct stream for Safari
            this.video.src = `${this.apiBase}/video/direct/${fileIndex}`;
        } else {
            // Seek to the same position in the new track (seamless switch)
            this.seek(currentAbsTime);
        }
    }

    getAudioTracks(fileIndex) {
        // "we only use audio track from the file wehere we also stream the video from if the same (language, froced) is present in another file  to avoid reading from two files"
        // "we dont show these 'duplicate' audio track to the user"
        
        if (!this.files[fileIndex]) return [];
        
        const mainFile = this.files[fileIndex];
        const mainAudioTracks = mainFile.audio_tracks.map(t => ({...t, fileIndex: fileIndex, origin: 'main'}));
        
        const allAudio = [...mainAudioTracks];
        
        // Iterate other files
        this.files.forEach((file, idx) => {
            if (idx === fileIndex) return;
            
            file.audio_tracks.forEach(track => {
                // Check for duplicate in main file
                const isDuplicate = mainFile.audio_tracks.some(mainTrack => 
                    (mainTrack.language === track.language) && 
                    // (mainTrack.forced === track.forced) // AudioTrack doesn't have forced in MkvBasicInfo struct shown earlier, checking struct...
                    // The MkvBasicInfo AudioTrack struct has: track_id, codec, channels, sample_rate, language. 
                    // It does NOT have forced flag in the struct I saw.
                    // Assuming language and codec/channels matching constitutes duplicate or just language?
                    // "same (language, froced)" - The prompt implies forced exists. Maybe it is missing in my MkvBasicInfo view or implied.
                    // I will check language.
                   (mainTrack.language === track.language)
                );
                
                if (!isDuplicate) {
                    allAudio.push({...track, fileIndex: idx, origin: 'external'});
                }
            });
        });
        
        return allAudio;
    }

    getSubtitleTracks() {
        // "prefer using the subtile from the file that is the smallest bc the subtiltes are alwys downloaded as a whole"
        // Group by language/forced/codec? Or identical track?
        // Let's group by (language, forced).
        // Find best candidate for each group.
        
        const groups = {};
        
        this.files.forEach((file, fIdx) => {
            file.subtitle_tracks.forEach(track => {
                const key = `${track.language || 'und'}_${track.forced}`;
                if (!groups[key]) {
                    groups[key] = [];
                }
                groups[key].push({...track, fileIndex: fIdx, fileSize: file.file_size});
            });
        });
        
        const result = [];
        for (const key in groups) {
            const candidates = groups[key];
            // Sort by file size ascending
            candidates.sort((a, b) => a.fileSize - b.fileSize);
            // Pick smallest
            result.push(candidates[0]);
        }
        return result;
    }

    selectAudio(trackId, fileIndex) {
        this.activeAudioTrackId = trackId;
        // We need to know which file this track belongs to.
        // The getAudioTracks returns objects with .fileIndex. 
        // We assume the stored active configuration includes fileIndex.
        this.activeAudioFileIndex = fileIndex; // Helper state
    }

    selectSubtitle(trackId, fileIndex) {
        this.activeSubtitleTrackId = trackId;
        this.activeSubtitleFileIndex = fileIndex;
        this.reloadSubtitles();
    }

    async seek(seconds) {
        if (this.isSafari) {
            this.video.currentTime = seconds;
            return;
        }

        // Remember whether the video was playing so we can resume after src change
        const wasPlaying = !this.video.paused;

        let currentAbsTime = this.video.currentTime + (this.currentSeekOffset || 0);
        let diff = Math.abs(seconds - currentAbsTime);
        let seekMode = (diff > 60) ? 'snap' : 'squeeze'; 

        // Construct mappings
        let mappings = `${this.activeFileIndex}_${this.activeVideoTrackId}`;
        if (this.activeAudioFileIndex === undefined) this.activeAudioFileIndex = this.activeFileIndex;
        mappings += `,${this.activeAudioFileIndex}_${this.activeAudioTrackId}`;
        
        const url = `${this.apiBase}/video/stream?mappings=${mappings}&seek=${seekMode}&start=${seconds}`;
        
        try {
            // Use fetch just to read the headers/errors without MSE overhead
            const controller = new AbortController();
            const res = await fetch(url, { signal: controller.signal });
            
            if (!res.ok) {
                const errText = await res.text();
                this.video.dispatchEvent(new CustomEvent('didiError', { detail: errText }));
                return;
            }

            const headerStart = parseFloat(res.headers.get('x-media-start-sec'));
            this.currentSeekOffset = !isNaN(headerStart) ? headerStart : seconds;
            
            // Abort the fetch so the browser doesn't download the body in JS space
            controller.abort();

            const onCanPlay = () => {
                this.video.removeEventListener('canplay', onCanPlay);
                if (wasPlaying) {
                    this.video.play().catch(() => {});
                }
                this.reloadSubtitles();
            };
            this.video.addEventListener('canplay', onCanPlay);

            this.video.src = url;
            this.video.querySelectorAll('track').forEach(t => t.remove());
            if (this._subtitleBlobUrl) {
                URL.revokeObjectURL(this._subtitleBlobUrl);
                this._subtitleBlobUrl = null;
            }
        } catch(e) {
            if (e.name !== 'AbortError') {
                this.video.dispatchEvent(new CustomEvent('didiError', { detail: e.message || 'Unknown error' }));
            }
        }
    }
    
    async reloadSubtitles() {
        // Remove existing <track> elements and revoke the old blob URL
        this.video.querySelectorAll('track').forEach(t => t.remove());
        if (this._subtitleBlobUrl) {
            URL.revokeObjectURL(this._subtitleBlobUrl);
            this._subtitleBlobUrl = null;
        }

        if (this.activeSubtitleTrackId === -1) return;

        try {
            // Use cached raw VTT if the same track is still selected; otherwise fetch once.
            const cacheHit = this._subtitleCache
                && this._subtitleCache.fileIndex === this.activeSubtitleFileIndex
                && this._subtitleCache.trackId   === this.activeSubtitleTrackId;

            let rawVtt;
            if (cacheHit) {
                rawVtt = this._subtitleCache.vttText;
            } else {
                // Fetch the full subtitle track as VTT text.
                // vtt_output=true tells the server to use VttSink.
                const map = `${this.activeSubtitleFileIndex}_${this.activeSubtitleTrackId}`;
                const url = `${this.apiBase}/video/stream?mappings=${map}&vtt_output=true`;
                const res = await fetch(url);
                if (!res.ok) throw new Error(`Subtitle fetch failed: ${res.status}`);
                rawVtt = await res.text();
                // Store in cache keyed by (fileIndex, trackId)
                this._subtitleCache = {
                    fileIndex: this.activeSubtitleFileIndex,
                    trackId:   this.activeSubtitleTrackId,
                    vttText:   rawVtt,
                };
            }

            // The raw VTT has absolute timestamps (0 … movie_end).
            // The video stream starts at currentSeekOffset (squeeze mode), so
            // video.currentTime=0 corresponds to absolute time currentSeekOffset.
            // Shift subtitle cues by -currentSeekOffset so they align with the stream.
            const offset = this.isSafari ? 0 : (this.currentSeekOffset || 0);
            const adjustedVtt = this.adjustVttTimestamps(rawVtt, -offset);

            const blob = new Blob([adjustedVtt], { type: 'text/vtt' });
            this._subtitleBlobUrl = URL.createObjectURL(blob);

            // Look up the language for the label
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

            // Force the track to load and show
            // track.default only works on initial video load, not when dynamically added
            if (track.track) {
                track.track.mode = 'showing';
            }
            track.addEventListener('load', () => {
                if (track.track) {
                    track.track.mode = 'showing';
                }
            });
            // Also try to reload the track to force processing
            const currentSrc = track.src;
            track.src = '';
            track.src = currentSrc;

        } catch (e) {
            console.error("Subtitle load failed", e);
        }
    }

    adjustVttTimestamps(vttText, offsetSeconds) {
        // Parse timestamp string to seconds
        const parseTimestamp = (ts) => {
            // Support both HH:MM:SS.mmm and MM:SS.mmm formats
            const m = ts.match(/^(?:(\d{2}):)?(\d{2}):(\d{2})\.(\d{3})$/);
            if (!m) return null;
            const h = m[1] ? parseInt(m[1]) : 0;
            const mins = parseInt(m[2]);
            const secs = parseInt(m[3]);
            const millis = parseInt(m[4]);
            return h * 3600 + mins * 60 + secs + millis / 1000;
        };

        // Format seconds back to timestamp
        const formatTimestamp = (total) => {
            const pad = (n, w = 2) => n.toString().padStart(w, '0');
            const nh = Math.floor(total / 3600);
            const rem = total % 3600;
            const nm = Math.floor(rem / 60);
            const ns = Math.floor(rem % 60);
            const nms = Math.round((rem - nm * 60 - ns) * 1000);
            return `${pad(nh)}:${pad(nm)}:${pad(ns)}.${pad(nms, 3)}`;
        };

        const lines = vttText.split('\n');
        const result = [];
        let foundFirstValidCue = false;
        let i = 0;

        // Keep header (WEBVTT and any initial comments/settings)
        while (i < lines.length) {
            const line = lines[i];
            // Cue timing line pattern: "00:00:00.000 --> 00:00:00.000" or "00:00.000 --> 00:00.000"
            const timingMatch = line.match(/^(?:(\d{2}):)?(\d{2}):(\d{2})\.(\d{3})\s*-->\s*(?:(\d{2}):)?(\d{2}):(\d{2})\.(\d{3})/);
            if (timingMatch) {
                // This is a cue timing line - check if start time is valid after offset
                // Reconstruct timestamps from match groups: [1]=startH [2]=startM [3]=startS [4]=startMs [5]=endH [6]=endM [7]=endS [8]=endMs
                const startTs = (timingMatch[1] ? timingMatch[1] + ':' : '') + timingMatch[2] + ':' + timingMatch[3] + '.' + timingMatch[4];
                const endTs = (timingMatch[5] ? timingMatch[5] + ':' : '') + timingMatch[6] + ':' + timingMatch[7] + '.' + timingMatch[8];
                const startTime = parseTimestamp(startTs);
                const endTime = parseTimestamp(endTs);
                if (startTime !== null) {
                    const adjustedStart = startTime + offsetSeconds;
                    if (adjustedStart >= 0) {
                        // First valid (non-negative) cue found
                        foundFirstValidCue = true;
                        // Adjust both timestamps and add the cue
                        const adjustedEnd = endTime + offsetSeconds;
                        result.push(`${formatTimestamp(adjustedStart)} --> ${formatTimestamp(adjustedEnd)}`);
                        i++;
                        // Add all cue text lines until blank line or next cue
                        while (i < lines.length && lines[i].trim() !== '' && !lines[i].match(/^(?:\d{2}:)?\d{2}:\d{2}\.\d{3}/)) {
                            result.push(lines[i]);
                            i++;
                        }
                        // Add blank line if present (separator between cues)
                        if (i < lines.length && lines[i].trim() === '') {
                            result.push(lines[i]);
                            i++;
                        }
                    } else {
                        // Negative timestamp - skip this entire cue
                        i++;
                        // Skip cue text lines
                        while (i < lines.length && lines[i].trim() !== '' && !lines[i].match(/^(?:\d{2}:)?\d{2}:\d{2}\.\d{3}/)) {
                            i++;
                        }
                        // Skip blank line separator
                        if (i < lines.length && lines[i].trim() === '') {
                            i++;
                        }
                    }
                } else {
                    i++;
                }
            } else {
                // Not a cue line - keep it if we haven't found valid cues yet (header section)
                // or if it's between cues
                if (!foundFirstValidCue) {
                    result.push(line);
                }
                i++;
            }
        }

        return result.join('\n');
    }

    getDuration() {
        // Return duration of current active file
        if (this.activeFileIndex === -1) return 0;
        return this.files[this.activeFileIndex].duration_ms / 1000;
    }
}

// Export to global
window.DidiPlayer = DidiPlayer;
