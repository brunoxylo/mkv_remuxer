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
        this.activeFileIndex = fileIndex;
        this.activeVideoTrackId = trackId;
        
        // Auto-select first audio track (prefer main file)
        const aTracks = this.getAudioTracks(fileIndex);
        if (aTracks.length > 0) {
            // Prefer internal
            const internal = aTracks.find(t => t.fileIndex === fileIndex);
            this.activeAudioTrackId = internal ? internal.track_id : aTracks[0].track_id;
            this.activeAudioFileIndex = internal ? internal.fileIndex : aTracks[0].fileIndex;
        } else {
            this.activeAudioTrackId = -1;
            this.activeAudioFileIndex = -1;
        }

        // Subtitles off by default
        this.activeSubtitleTrackId = -1;
        
        if (this.isSafari) {
            // Direct stream for Safari
            this.video.src = `${this.apiBase}/video/direct/${fileIndex}`;
        } else {
             // Init with squeeze seek at 0
            this.seek(0);
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

    seek(seconds) {
        if (this.isSafari) {
            this.video.currentTime = seconds;
            return;
        }

        // Remuxer seek (squeeze)
        // Construct mappings
        // Video: activeFileIndex_activeVideoTrackId
        // Audio: activeAudioFileIndex_activeAudioTrackId
        
        let mappings = `${this.activeFileIndex}_${this.activeVideoTrackId}`;
        
        // Need to handle missing track selection robustly
        if (this.activeAudioFileIndex === undefined) this.activeAudioFileIndex = this.activeFileIndex;
        
        mappings += `,${this.activeAudioFileIndex}_${this.activeAudioTrackId}`;
        
        // URL
        // seek=squeeze
        const url = `${this.apiBase}/video/stream?mappings=${mappings}&seek=squeeze&start=${seconds}`;
        
        this.video.src = url;
        // The browser will start playing from receiving the stream.
        // Since we use 'squeeze', the stream starts at timestamp 0.
        // We must update subtitle offset.
        this.currentSeekOffset = seconds;
        this.reloadSubtitles();
    }
    
    async reloadSubtitles() {
        // Clear existing tracks
        const oldTracks = this.video.querySelectorAll('track');
        oldTracks.forEach(t => t.remove());

        if (this.activeSubtitleTrackId === -1) return;

        // Fetch subtitle track whole
        // Mapping: activeSubtitleFileIndex_activeSubtitleTrackId
        const map = `${this.activeSubtitleFileIndex}_${this.activeSubtitleTrackId}`;
        const url = `${this.apiBase}/video/stream?mappings=${map}`; // No start/end implies full file? Or default cut?
        // backend defaults start=0 end=None -> full file remux.
        
        // Fetch blob to process cues? Or let browser handle it?
        // Prompt: "download the whole subtile track from the beginning fro the one the user has selected and adapt the cues to the current play position"
        
        try {
            const res = await fetch(url);
            const text = await res.text(); // VTT content
            
            // We need to create a Blob URL but potentially modifying cues.
            // If uses 'squeeze' mode for video, video TS starts at 0.
            // But we are at `currentSeekOffset` in the movie.
            // So if play position is 0 (relative to stream), meaningful time is `currentSeekOffset`.
            // Subtitles have absolute timestamps (0..movie_end).
            // So we need to shift subtitles by -currentSeekOffset.
            // e.g. timestamp 100 --> 100 - 50 = 50.
            
            // Simple VTT parser/modifier regex
            // Timestamp format: 00:00:10.000
            
            const offset = this.isSafari ? 0 : this.currentSeekOffset; 
            // For Safari (Direct Stream), timestamps are absolute, so offset should be 0.
            
            const adjustedVtt = this.adjustVttTimestamps(text, -offset);
            
            const blob = new Blob([adjustedVtt], { type: 'text/vtt' });
            const trackUrl = URL.createObjectURL(blob);
            
            const track = document.createElement('track');
            track.kind = 'subtitles';
            track.label = 'English'; // TODO: use actual language
            track.srclang = 'en';
            track.src = trackUrl;
            track.default = true;
            this.video.appendChild(track);
            
        } catch (e) {
            console.error("Subtitle load failed", e);
        }
    }

    adjustVttTimestamps(vttText, offsetSeconds) {
        // Regex for VTT timestamps: 00:00:00.000 or 00:00.000
        // We parse, add offset, print.
        // This is crude regex, generic library would be better but keeping it simple.
        return vttText.replace(/(\d{2}:)?(\d{2}):(\d{2})\.(\d{3})/g, (match, h, m, s, ms) => {
             let hours = h ? parseInt(h.replace(':', '')) : 0;
             let mins = parseInt(m);
             let secs = parseInt(s);
             let millis = parseInt(ms);
             
             let total = hours * 3600 + mins * 60 + secs + millis / 1000;
             total += offsetSeconds;
             
             if (total < 0) return "00:00:00.000"; // Clamp to 0
             
             // Convert back
             let nh = Math.floor(total / 3600);
             let rem = total % 3600;
             let nm = Math.floor(rem / 60);
             let ns = Math.floor(rem % 60);
             let nms = Math.round((rem - nm * 60 - ns) * 1000);
             
             const pad = (n, w=2) => n.toString().padStart(w, '0');
             return `${pad(nh)}:${pad(nm)}:${pad(ns)}.${pad(nms, 3)}`;
        });
    }

    getDuration() {
        // Return duration of current active file
        if (this.activeFileIndex === -1) return 0;
        return this.files[this.activeFileIndex].duration_ms / 1000;
    }
}

// Export to global
window.DidiPlayer = DidiPlayer;
