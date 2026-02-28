use crate::block_ext::TrackKind;
use mkv_element::prelude::{Info, Tracks};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoTrack {
    pub track_id: u32,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub color_space: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioTrack {
    pub track_id: u32,
    pub codec: String,
    pub channels: u32,
    pub sample_rate: u32,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtitleTrack {
    pub track_id: u32,
    pub codec: String,
    pub forced: bool,
    pub language: Option<String>,
}

impl SubtitleTrack {
pub fn is_text_based(&self) -> bool {
        let text_based_subtitle_codecs = [
            "ass",
            "eia_608",
            "hdmv_text_subtitle",
            "jacosub",
            "microdvd",
            "mpl2",
            "pjs",
            "realtext",
            "sami",
            "srt",
            "ssa",
            "stl",
            "subrip",
            "subviewer",
            "subviewer1",
            "text",
            "ttml",
            "vplayer",
            "webvtt",
            "mov_text",
        ];
        if text_based_subtitle_codecs.contains(&self.codec.as_str()) {
            return true;
        } else {
            return false;
        }
    }
}


/// holds a more compact summary of the MKV file's basic info, extracted from the `Info` and `Tracks` elements
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MkvBasicInfo {
    pub file_name: String,
    pub duration_ms: u64,
    pub file_size: u64,
    pub video_tracks: Vec<VideoTrack>,
    pub audio_tracks: Vec<AudioTrack>,
    pub subtitle_tracks: Vec<SubtitleTrack>,
}

impl MkvBasicInfo {
    pub fn new(tracks: &Tracks, info: &Info, file_size: u64, file_name: String) -> Self {
        let timestamp_scale = info.timestamp_scale.0;
        let duration_ms = info
            .duration
            .as_ref()
            .map(|d| (d.0 * timestamp_scale as f64 / 1_000_000.0) as u64)
            .unwrap_or(0);

        let mut video_tracks = Vec::new();
        let mut audio_tracks = Vec::new();
        let mut subtitle_tracks = Vec::new();

        for entry in &tracks.track_entry {
            let track_id = entry.track_number.0 as u32;
            let codec = entry.codec_id.0.clone();
            let kind = TrackKind::from_u64(entry.track_type.0);

            match kind {
                TrackKind::Video => {
                    if let Some(video) = &entry.video {
                        let color_space = entry.clone()
                            .video
                            .and_then(|v| v.colour.as_ref().map(|c| c.matrix_coefficients.0.to_string()));
                        video_tracks.push(VideoTrack {
                            track_id,
                            codec,
                            width: video.pixel_width.0 as u32,
                            height: video.pixel_height.0 as u32,
                            color_space,
                        });
                    }
                }
                TrackKind::Audio => {
                    if let Some(audio) = &entry.audio {
                        let language = entry.language.0.clone();
                        audio_tracks.push(AudioTrack {
                            track_id,
                            codec,
                            channels: audio.channels.0 as u32,
                            sample_rate: audio.sampling_frequency.0 as u32,
                            language: (!language.is_empty()).then_some(language),
                        });
                    }
                }
                TrackKind::Subtitle => {
                    let language = entry.language.0.clone();
                    subtitle_tracks.push(SubtitleTrack {
                        track_id,
                        codec,
                        forced: entry.flag_forced.0 != 0,
                        language: (!language.is_empty()).then_some(language),
                    });
                }
                _ => {}
            }
        }

        MkvBasicInfo {
            file_name,
            duration_ms,
            file_size,
            video_tracks,
            audio_tracks,
            subtitle_tracks,
        }
    }
}