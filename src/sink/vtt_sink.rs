use super::Sink;
use crate::{ContainerFormat, Error, Result};
use mkv_element::prelude::*;
use std::io::Write;

/// VTT-based sink implementation for writing WebVTT subtitle files
/// 
/// This sink extracts subtitle data from MKV clusters and writes it
/// as WebVTT format to any writer (file, memory buffer, HTTP response, etc.)
pub struct VttSink<W: Write + Send> {
    writer: W,
    initialized: bool,
    subtitle_track_number: Option<u64>,
    timecode_scale: u64,
    /// Buffer to store cues before writing (to ensure proper ordering)
    cues: Vec<VttCue>,
}

#[derive(Debug, Clone)]
struct VttCue {
    start_ms: u64,
    end_ms: u64,
    id: Option<String>,
    settings: Option<String>,
    text: String,
}

impl<W: Write + Send> VttSink<W> {
    /// Create a new VTT sink that writes to the specified writer
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            initialized: false,
            subtitle_track_number: None,
            timecode_scale: 1_000_000, // Default 1ms
            cues: Vec::new(),
        }
    }

    /// Format a timestamp in WebVTT format (MM:SS.mmm or HH:MM:SS.mmm)
    fn format_timestamp(timestamp_ms: u64) -> String {
        let hours = timestamp_ms / 3_600_000;
        let minutes = (timestamp_ms % 3_600_000) / 60_000;
        let seconds = (timestamp_ms % 60_000) / 1000;
        let milliseconds = timestamp_ms % 1000;

        if hours > 0 {
            format!("{:02}:{:02}:{:02}.{:03}", hours, minutes, seconds, milliseconds)
        } else {
            format!("{:02}:{:02}.{:03}", minutes, seconds, milliseconds)
        }
    }

    /// Extract cue data from a block's frame data
    /// According to WebVTT-in-WebM spec, the format is:
    /// 1. Cue identifier + \n (or just \n if no ID)
    /// 2. Cue settings + \n (or just \n if no settings)
    /// 3. Cue payload text
    fn parse_block_data(data: &[u8]) -> Result<(Option<String>, Option<String>, String)> {
        let text = String::from_utf8(data.to_vec())
            .map_err(|_| Error::InvalidConfig("Invalid UTF-8 in subtitle block".to_string()))?;

        let mut lines = text.lines();

        // First line: ID or empty
        let id = match lines.next() {
            Some(line) if !line.is_empty() => Some(line.to_string()),
            _ => None,
        };

        // Second line: settings or empty
        let settings = match lines.next() {
            Some(line) if !line.is_empty() => Some(line.to_string()),
            _ => None,
        };

        // Rest: cue text
        let cue_text = lines.collect::<Vec<_>>().join("\n");

        Ok((id, settings, cue_text))
    }
}

impl<W: Write + Send> Sink for VttSink<W> {
    fn initialize(
        &mut self,
        tracks: &Tracks,
        info: &Info,
        ebml_header: &Ebml, // not needed for vtt
        _chapters: Option<&Chapters>,
    ) -> Result<()> {
        if self.initialized {
            return Err(Error::InvalidConfig("VttSink already initialized".to_string()));
        }

        // Find subtitle tracks
        let subtitle_tracks: Vec<_> = tracks
            .track_entry
            .iter()
            .filter(|track| {
                track.track_type.0 == 17 // Subtitle track type
                    && track.codec_id.0.starts_with("D_WEBVTT")
            })
            .collect();

        // Validate exactly one subtitle track
        if subtitle_tracks.is_empty() {
            return Err(Error::InvalidConfig(
                "No WebVTT subtitle track found in input".to_string(),
            ));
        }

        if subtitle_tracks.len() > 1 {
            return Err(Error::InvalidConfig(format!(
                "Multiple subtitle tracks found ({}), VttSink requires exactly one",
                subtitle_tracks.len()
            )));
        }

        let subtitle_track = subtitle_tracks[0];
        self.subtitle_track_number = Some(subtitle_track.track_number.0);
        self.timecode_scale = info.timestamp_scale.0;

        // Write WEBVTT header
        writeln!(self.writer, "WEBVTT")?;
        writeln!(self.writer)?;

        self.initialized = true;
        Ok(())
    }

    fn write_cluster(&mut self, cluster: &Cluster, _track_number: u64) -> Result<()> {
        if !self.initialized {
            return Err(Error::InvalidConfig("VttSink not initialized".to_string()));
        }

        let subtitle_track = self.subtitle_track_number.ok_or_else(|| {
            Error::InvalidConfig("No subtitle track configured".to_string())
        })?;

        let cluster_timestamp_ns = cluster.timestamp.0 * self.timecode_scale;

        // Import the trait for block methods
        use crate::ClusterBlockExt;

        // Process each block in the cluster
        for block in &cluster.blocks {
            // Get block track number
            let block_track = block.track_number()?;

            // Only process blocks for our subtitle track
            if block_track != subtitle_track {
                continue;
            }

            // Extract block data and timing
            let (block_data, relative_timestamp, duration_ticks) = match block {
                mkv_element::ClusterBlock::Simple(_) => {
                    // SimpleBlock doesn't support BlockDuration, skip
                    continue;
                }
                mkv_element::ClusterBlock::Group(_) => {
                    let data = block.get_data()?.clone();
                    let timestamp = block.timestamp()?;
                    let duration = block.get_block_duration()
                        .ok_or_else(|| Error::InvalidConfig("Subtitle block missing duration".to_string()))?;
                    (data, timestamp, duration)
                }
            };

            // Calculate absolute timing
            let block_timestamp_ns = cluster_timestamp_ns
                + (relative_timestamp as i64 * self.timecode_scale as i64) as u64;
            let duration_ns = duration_ticks * self.timecode_scale;
            let end_timestamp_ns = block_timestamp_ns + duration_ns;

            let start_ms = block_timestamp_ns / 1_000_000;
            let end_ms = end_timestamp_ns / 1_000_000;

            // Skip the block header (track number VINT + 2 bytes timestamp + 1 byte flags)
            // to get to the frame data
            let track_vint_len = if block_data[0] & 0x80 != 0 {
                1
            } else if block_data[0] & 0x40 != 0 {
                2
            } else if block_data[0] & 0x20 != 0 {
                3
            } else if block_data[0] & 0x10 != 0 {
                4
            } else {
                return Err(Error::InvalidConfig("Invalid track number VINT".to_string()));
            };

            let frame_data_offset = track_vint_len + 3; // VINT + 2 bytes timestamp + 1 byte flags
            if block_data.len() < frame_data_offset {
                return Err(Error::InvalidConfig("Block data too short".to_string()));
            }

            let frame_data = &block_data[frame_data_offset..];

            // Parse frame data according to WebVTT-in-WebM spec
            let (id, settings, text) = Self::parse_block_data(frame_data)?;

            self.cues.push(VttCue {
                start_ms,
                end_ms,
                id,
                settings,
                text,
            });
        }

        Ok(())
    }

    fn finalize(&mut self) -> Result<()> {
        if !self.initialized {
            return Err(Error::InvalidConfig("VttSink not initialized".to_string()));
        }

        // Sort cues by start time
        self.cues.sort_by_key(|cue| cue.start_ms);

        // Write all cues
        for cue in &self.cues {
            // Write timing line
            writeln!(
                self.writer,
                "{} --> {}",
                Self::format_timestamp(cue.start_ms),
                Self::format_timestamp(cue.end_ms)
            )?;

            // Write cue ID if present
            if let Some(ref id) = cue.id {
                writeln!(self.writer, "{}", id)?;
            }

            // Write cue settings if present
            if let Some(ref settings) = cue.settings {
                writeln!(self.writer, "{}", settings)?;
            }

            // Write cue text
            writeln!(self.writer, "{}", cue.text)?;

            // Empty line between cues
            writeln!(self.writer)?;
        }

        self.writer.flush()?;
        Ok(())
    }
    
    fn does_support_container_format(&self, format: crate::ContainerFormat) -> bool {
        match format {
            ContainerFormat::Mkv => false,
            ContainerFormat::WebM => false,
            ContainerFormat::Vtt => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_timestamp() {
        assert_eq!(VttSink::<Vec<u8>>::format_timestamp(0), "00:00.000");
        assert_eq!(VttSink::<Vec<u8>>::format_timestamp(5000), "00:05.000");
        assert_eq!(VttSink::<Vec<u8>>::format_timestamp(65000), "01:05.000");
        assert_eq!(VttSink::<Vec<u8>>::format_timestamp(3665000), "01:01:05.000");
        assert_eq!(VttSink::<Vec<u8>>::format_timestamp(12345), "00:12.345");
    }

    #[test]
    fn test_parse_block_data() {
        // Test with ID and settings
        let data = b"cue-1\nline:90%\nHello World";
        let (id, settings, text) = VttSink::<Vec<u8>>::parse_block_data(data).unwrap();
        assert_eq!(id, Some("cue-1".to_string()));
        assert_eq!(settings, Some("line:90%".to_string()));
        assert_eq!(text, "Hello World");

        // Test with no ID, no settings
        let data = b"\n\nHello World";
        let (id, settings, text) = VttSink::<Vec<u8>>::parse_block_data(data).unwrap();
        assert_eq!(id, None);
        assert_eq!(settings, None);
        assert_eq!(text, "Hello World");

        // Test multiline text
        let data = b"\n\nLine 1\nLine 2\nLine 3";
        let (id, settings, text) = VttSink::<Vec<u8>>::parse_block_data(data).unwrap();
        assert_eq!(text, "Line 1\nLine 2\nLine 3");
    }
}
