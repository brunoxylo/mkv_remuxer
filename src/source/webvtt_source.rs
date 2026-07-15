use super::{SeekType, Source};
use crate::source::CutInterval;
use crate::source::util::basic_info::MkvBasicInfo;
use crate::{Error, Result};
use bytes::BytesMut;
use log::trace;
use mkv_element::ClusterBlock;
use mkv_element::io::blocking_impl::*;
use mkv_element::prelude::*;
use std::fmt;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use uuid::Uuid;

#[derive(Debug, Clone)]
struct VttCue {
    /// Start time in nanoseconds
    start_ns: u64,
    /// End time in nanoseconds
    end_ns: u64,
    /// Cue text (payload)
    text: String,
    /// Optional cue identifier
    id: Option<String>,
    /// Optional cue settings
    settings: Option<String>,
}

pub struct WebVttSource {
    /// Parsed cues
    cues: Vec<VttCue>,
    /// Current cue index for iteration
    current_cue_idx: usize,
    /// Timecode scale (nanoseconds per tick) - WebVTT uses milliseconds typically
    timecode_scale: u64,
    /// Target output timecode scale
    output_timecode_scale: u64,
    /// Track number to assign to this subtitle track
    track_number: u64,
    /// Track UID
    track_uid: u64,
    /// Language code (default: "eng")
    language: String,
    forced: bool,
    /// Track name
    track_name: Option<String>,
    /// Cut parameters
    start_ns: Option<u64>,
    end_ns: Option<u64>,
    /// The resolved output interval (updated by initialize() and cut())
    output_interval: CutInterval,
    /// Batch size for clusters (number of cues per cluster)
    cluster_batch_size: usize,
    /// Is the source finished?
    finished: bool,
    bytes_read: u64,
    file_name: String,
}

impl WebVttSource {
    pub fn new<R: Read + Send>(reader: R, language: String, forced: bool) -> Result<Self> {
        // Parse the VTT file
        let (cues, bytes_read) = Self::parse_vtt_file(reader)?;

        Ok(Self {
            cues,
            current_cue_idx: 0,
            timecode_scale: 1_000_000, // 1ms in nanoseconds
            output_timecode_scale: 1_000_000,
            track_number: 1,
            track_uid: Uuid::new_v4().as_u128() as u64, // Generate random UID from UUID
            language: language.to_string(),
            forced,
            track_name: None,
            start_ns: None,
            end_ns: None,
            output_interval: CutInterval::new(),
            cluster_batch_size: 10,
            finished: false,
            bytes_read,
            file_name: language.to_string(), // Placeholder, since we don't have a file path
        })
    }

    pub fn with_file_name(mut self, file_name: String) -> Self {
        self.file_name = file_name;
        self
    }

    /// Set the language code for the subtitle track
    pub fn with_language(mut self, lang: &str) -> Self {
        self.language = lang.to_string();
        self
    }

    /// Set the track name
    pub fn with_track_name(mut self, name: &str) -> Self {
        self.track_name = Some(name.to_string());
        self
    }

    /// Set the cluster batch size (number of cues per cluster)
    pub fn with_cluster_batch_size(mut self, size: usize) -> Self {
        self.cluster_batch_size = size.max(1);
        self
    }

    /// Parse a WebVTT file into cues
    fn parse_vtt_file<R: Read>(reader: R) -> Result<(Vec<VttCue>, u64)> {
        let mut bytes_read: u64 = 0;
        let reader = BufReader::new(reader);
        let mut lines = reader.lines();

        // Check for WEBVTT header
        if let Some(Ok(first_line)) = lines.next() {
            bytes_read += first_line.len() as u64 + 1; // +1 for newline
            if !first_line.starts_with("WEBVTT") {
                return Err(Error::InvalidConfig(
                    "Invalid VTT file: missing WEBVTT header".to_string(),
                ));
            }
        } else {
            return Err(Error::InvalidConfig("Empty VTT file".to_string()));
        }

        let mut cues = Vec::new();
        let mut current_id: Option<String> = None;
        let mut in_cue = false;

        let mut line_iter = lines.peekable();

        while let Some(Ok(line)) = line_iter.next() {
            bytes_read += line.len() as u64 + 1; // +1 for newline
            let trimmed = line.trim();

            // Skip empty lines between cues
            if trimmed.is_empty() {
                in_cue = false;
                current_id = None;
                continue;
            }

            // Skip NOTE blocks
            if trimmed.starts_with("NOTE") {
                while let Some(Ok(note_line)) = line_iter.next() {
                    bytes_read += note_line.len() as u64 + 1; // +1 for newline
                    if note_line.trim().is_empty() {
                        break;
                    }
                }
                continue;
            }

            // Check if this is a timestamp line
            if trimmed.contains("-->") {
                in_cue = true;
                let cue = Self::parse_cue_timing(trimmed, current_id.take())?;

                // Read cue text (may be multiple lines)
                let mut text_lines = Vec::new();
                loop {
                    match line_iter.peek() {
                        Some(Ok(next_line)) if !next_line.trim().is_empty() => {
                            if let Some(Ok(text_line)) = line_iter.next() {
                                bytes_read += text_line.len() as u64 + 1; // +1 for newline
                                text_lines.push(text_line);
                            }
                        }
                        _ => break,
                    }
                }

                let mut cue = cue;
                cue.text = text_lines.join("\n");
                cues.push(cue);
            } else if !in_cue {
                // This might be a cue identifier
                current_id = Some(trimmed.to_string());
            }
        }

        Ok((cues, bytes_read))
    }

    /// Parse a timing line like "00:00:10.500 --> 00:00:13.000" or with settings
    fn parse_cue_timing(line: &str, id: Option<String>) -> Result<VttCue> {
        let parts: Vec<&str> = line.split("-->").collect();
        if parts.len() != 2 {
            return Err(Error::InvalidConfig(format!(
                "Invalid VTT timing line: {}",
                line
            )));
        }

        let start_str = parts[0].trim();
        let end_part = parts[1].trim();

        // End part might have cue settings after the timestamp
        let (end_str, settings) = if let Some(space_idx) = end_part.find(' ') {
            (
                &end_part[..space_idx],
                Some(end_part[space_idx + 1..].to_string()),
            )
        } else {
            (end_part, None)
        };

        let start_ns = Self::parse_timestamp(start_str)?;
        let end_ns = Self::parse_timestamp(end_str)?;

        Ok(VttCue {
            start_ns,
            end_ns,
            text: String::new(),
            id,
            settings,
        })
    }

    /// Parse a VTT timestamp like "00:00:10.500" or "00:10.500"
    fn parse_timestamp(timestamp: &str) -> Result<u64> {
        let parts: Vec<&str> = timestamp.split(':').collect();

        let (hours, minutes, seconds_str) = match parts.len() {
            2 => {
                // MM:SS.mmm format
                (
                    0.0,
                    parts[0].parse::<f64>().map_err(|_| {
                        Error::InvalidConfig(format!("Invalid minutes in timestamp: {}", timestamp))
                    })?,
                    parts[1],
                )
            }
            3 => {
                // HH:MM:SS.mmm format
                let h = parts[0].parse::<f64>().map_err(|_| {
                    Error::InvalidConfig(format!("Invalid hours in timestamp: {}", timestamp))
                })?;
                let m = parts[1].parse::<f64>().map_err(|_| {
                    Error::InvalidConfig(format!("Invalid minutes in timestamp: {}", timestamp))
                })?;
                (h, m, parts[2])
            }
            _ => {
                return Err(Error::InvalidConfig(format!(
                    "Invalid timestamp format: {}",
                    timestamp
                )));
            }
        };

        let seconds = seconds_str.parse::<f64>().map_err(|_| {
            Error::InvalidConfig(format!("Invalid seconds in timestamp: {}", timestamp))
        })?;

        let total_seconds = hours * 3600.0 + minutes * 60.0 + seconds;
        Ok((total_seconds * 1_000_000_000.0) as u64)
    }

    /// Create a cluster containing a batch of cues
    fn create_cluster_from_cues(&self, cues: &[VttCue]) -> Result<Cluster> {
        if cues.is_empty() {
            return Err(Error::InvalidConfig(
                "No cues to create cluster".to_string(),
            ));
        }

        let shift_ns = self.start_ns.unwrap_or(0);

        // Cluster timestamp is the first cue's start time, shifted by the cut start
        let cluster_timestamp_ticks =
            cues[0].start_ns.saturating_sub(shift_ns) / self.output_timecode_scale;

        let mut blocks = Vec::new();

        for cue in cues {
            // Create a BlockGroup (needed for BlockDuration)
            let duration_ns = cue.end_ns.saturating_sub(cue.start_ns);
            let duration_ticks = duration_ns / self.output_timecode_scale;

            // Block timestamp is relative to cluster, shifted by cut start
            let block_timestamp_ns = cue.start_ns.saturating_sub(shift_ns);
            let block_timestamp_ticks = block_timestamp_ns / self.output_timecode_scale;
            let relative_timestamp = (block_timestamp_ticks as i64 - cluster_timestamp_ticks as i64)
                .clamp(i16::MIN as i64, i16::MAX as i64)
                as i16;

            // Encode block data: track number (vint) + timestamp (2 bytes) + flags (1 byte) + frame data
            let mut block_data = BytesMut::new();

            // Track number as VINT — write_to needs std::io::Write; use a small scratch Vec
            let track_vint = VInt64::new(self.track_number);
            let mut vint_bytes = Vec::with_capacity(8);
            track_vint.write_to(&mut vint_bytes).map_err(|e| {
                Error::InvalidConfig(format!("Failed to write track number: {}", e))
            })?;
            block_data.extend_from_slice(&vint_bytes);

            // Timestamp (2 bytes, big-endian signed)
            block_data.extend_from_slice(&relative_timestamp.to_be_bytes());

            // Flags byte (0x00 - no special flags for subtitles)
            block_data.extend_from_slice(&[0x00]);

            // Frame data format according to WebVTT-in-WebM spec:
            // 1. Cue identifier + line terminator (or just line terminator if no ID)
            // 2. Cue settings + line terminator (or just line terminator if no settings)
            // 3. Cue payload text

            // Write cue identifier or empty line
            if let Some(ref id) = cue.id {
                block_data.extend_from_slice(id.as_bytes());
            }
            block_data.extend_from_slice(&[b'\n']);

            // Write cue settings or empty line
            if let Some(ref settings) = cue.settings {
                block_data.extend_from_slice(settings.as_bytes());
            }
            block_data.extend_from_slice(&[b'\n']);

            // Write cue payload text
            block_data.extend_from_slice(cue.text.as_bytes());

            // Create BlockGroup with Block and BlockDuration
            let block_group = BlockGroup {
                crc32: None,
                void: None,
                block: Block(block_data.freeze()),
                block_duration: Some(BlockDuration(duration_ticks)),
                reference_priority: ReferencePriority(0),
                reference_block: Vec::new(), // Empty for subtitles (no references)
                block_additions: None,
                codec_state: None,
                discard_padding: None,
            };

            blocks.push(ClusterBlock::Group(block_group));
        }

        Ok(Cluster {
            crc32: None,
            void: None,
            timestamp: Timestamp(cluster_timestamp_ticks),
            position: None,
            prev_size: None,
            blocks,
        })
    }
}

impl fmt::Display for WebVttSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "WebVTT: {} forced: {}({} cues)",
            self.language,
            self.forced,
            self.cues.len()
        )
    }
}

impl Source for WebVttSource {
    fn get_tracks(&self) -> Result<Tracks> {
        // Create a subtitle track entry
        let track_entry = TrackEntry {
            crc32: None,
            void: None,
            track_number: TrackNumber(self.track_number),
            track_uid: TrackUid(self.track_uid),
            track_type: TrackType(17), // 17 = Subtitle track type
            flag_enabled: FlagEnabled(1),
            flag_default: FlagDefault(1),
            flag_forced: FlagForced(self.forced as u64),
            flag_hearing_impaired: None,
            flag_visual_impaired: None,
            flag_text_descriptions: None,
            flag_original: None,
            flag_commentary: None,
            flag_lacing: FlagLacing(0),
            default_duration: None,
            default_decoded_field_duration: None,
            max_block_addition_id: MaxBlockAdditionId(0),
            block_addition_mapping: Vec::new(),
            name: self.track_name.as_ref().map(|n| Name(n.clone())),
            language: Language(self.language.clone()),
            language_bcp47: None,
            codec_id: CodecId("D_WEBVTT/SUBTITLES".to_string()),
            codec_private: None, // Could store WebVTT header/styles here if needed
            codec_name: Some(CodecName("WebVTT".to_string())),
            codec_delay: CodecDelay(0),
            seek_pre_roll: SeekPreRoll(0),
            track_translate: Vec::new(),
            video: None,
            audio: None,
            track_operation: None,
            content_encodings: None,
        };

        Ok(Tracks {
            crc32: None,
            void: None,
            track_entry: vec![track_entry],
        })
    }

    fn get_chapters(&self) -> Result<Option<Chapters>> {
        Ok(None)
    }

    fn get_basic_info(&self) -> Result<MkvBasicInfo> {
        let tracks = self.get_tracks()?;
        let info = self.get_info()?;
        Ok(MkvBasicInfo::new(
            &tracks,
            &info,
            self.bytes_read,
            self.language.clone(),
        ))
    }

    fn get_info(&self) -> Result<Info> {
        // Calculate duration from last cue
        let duration_ns = self.cues.last().map(|c| c.end_ns).unwrap_or(0);
        let duration_ticks = duration_ns as f64 / self.timecode_scale as f64;

        Ok(Info {
            crc32: None,
            void: None,
            segment_uuid: None,
            segment_filename: None,
            prev_uuid: None,
            prev_filename: None,
            next_uuid: None,
            next_filename: None,
            segment_family: Vec::new(),
            chapter_translate: Vec::new(),
            timestamp_scale: TimestampScale(self.timecode_scale),
            duration: Some(Duration(duration_ticks)),
            date_utc: None,
            title: self.track_name.as_ref().map(|n| Title(n.clone())),
            muxing_app: MuxingApp("mkv_remuxer".to_string()),
            writing_app: WritingApp("mkv_remuxer".to_string()),
        })
    }

    fn get_next_cluster(&mut self) -> Result<Option<Cluster>> {
        if self.finished || self.current_cue_idx >= self.cues.len() {
            self.finished = true;
            return Ok(None);
        }

        // Collect cues for this cluster
        let mut batch = Vec::new();
        let start_idx = self.current_cue_idx;

        for i in 0..self.cluster_batch_size {
            let idx = start_idx + i;
            if idx >= self.cues.len() {
                break;
            }

            let cue = &self.cues[idx];

            // Apply cut filters
            if let Some(start) = self.start_ns {
                if cue.end_ns <= start {
                    trace!(
                        "dropping Subtitle cue (track {}) at {}-{} ns: ends before cut start ({} ns)",
                        self.track_number, cue.start_ns, cue.end_ns, start
                    );
                    self.current_cue_idx += 1;
                    continue;
                }
            }

            if let Some(end) = self.end_ns {
                if cue.start_ns >= end {
                    trace!(
                        "dropping Subtitle cue (track {}) at {}-{} ns: starts at/after cut end ({} ns)",
                        self.track_number, cue.start_ns, cue.end_ns, end
                    );
                    self.finished = true;
                    break;
                }
            }

            batch.push(cue.clone());
            self.current_cue_idx += 1;
        }

        if batch.is_empty() {
            if self.current_cue_idx >= self.cues.len() {
                self.finished = true;
            }
            return Ok(None);
        }

        let cluster = self.create_cluster_from_cues(&batch)?;
        Ok(Some(cluster))
    }

    fn get_own_timecode_scale(&self) -> Result<u64> {
        Ok(self.timecode_scale)
    }

    fn get_target_timecode_scale(&self) -> Result<u64> {
        Ok(self.output_timecode_scale)
    }

    fn get_output_interval(&mut self) -> Result<CutInterval> {
        Ok(self.output_interval.clone())
    }

    fn initialize(&mut self, time_scale: Option<u64>) -> Result<CutInterval> {
        if let Some(ts) = time_scale {
            self.output_timecode_scale = ts;
        }
        self.current_cue_idx = 0;
        self.finished = false;
        let start_ns = self.start_ns.unwrap_or(0);
        let end_ns = self.cues.last().map(|c| c.end_ns).unwrap_or(start_ns);
        self.output_interval = CutInterval::new().with_start(start_ns).with_end(end_ns);
        Ok(self.output_interval.clone())
    }

    fn cut(&mut self, _seek_type: SeekType, cut_interval: CutInterval) -> Result<CutInterval> {
        self.start_ns = cut_interval.start_ns;
        self.end_ns = cut_interval.end_ns;
        // WebVTT has no keyframes — return the interval unchanged.
        self.output_interval = cut_interval.clone();
        Ok(cut_interval)
    }

    fn start_remuxing(&mut self) -> Result<()> {
        self.current_cue_idx = 0;
        self.finished = false;
        Ok(())
    }
}
