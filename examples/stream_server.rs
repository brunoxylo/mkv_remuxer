/// Stream MKV server example using warp
///
/// This example demonstrates how to use StreamSink to serve MKV video on-the-fly
/// via HTTP with support for:
/// - Start/end position parameters
/// - Audio track selection
/// - Dynamic remuxing based on request parameters
///
/// Usage:
///   cargo run --example stream_server
///
/// Then open in browser:
///   http://localhost:3030/video?start=5&end=15&audio_track=2
///
/// Parameters:
///   - start: Start position in seconds (default: 0)
///   - end: End position in seconds (default: entire file)
///   - tracks: Comma-separated list of track numbers to include (e.g. tracks=2,3). If not specified, includes all tracks
///
use bytes::Bytes;
use log::{error, info};
use mkv_remuxer::{
    Remuxer, RemuxerCutMode, RemuxerState, sink::{OutputSink, StreamSink}, source::{CutInterval, FileSource, InputSource, SeekType}
};
use std::collections::HashMap;
use std::convert::Infallible;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use tokio::sync::mpsc;
use warp::hyper;
use warp::Filter;

/// A streaming writer that sends chunks over a channel as they're written
struct StreamWriter {
    tx: mpsc::UnboundedSender<Result<Bytes, std::io::Error>>,
    buffer: Vec<u8>,
    position: u64,
}

impl StreamWriter {
    fn new(tx: mpsc::UnboundedSender<Result<Bytes, std::io::Error>>) -> Self {
        Self {
            tx,
            buffer: Vec::new(),
            position: 0,
        }
    }
}

impl Write for StreamWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        self.position += buf.len() as u64;

        // Send chunks when buffer reaches a reasonable size (64KB)
        const CHUNK_SIZE: usize = 64 * 1024;
        if self.buffer.len() >= CHUNK_SIZE {
            let chunk = std::mem::take(&mut self.buffer);
            if self.tx.send(Ok(Bytes::from(chunk))).is_err() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "Receiver dropped",
                ));
            }
        }

        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.buffer.is_empty() {
            let chunk = std::mem::take(&mut self.buffer);
            if self.tx.send(Ok(Bytes::from(chunk))).is_err() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "Receiver dropped",
                ));
            }
        }
        Ok(())
    }
}

impl Seek for StreamWriter {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        match pos {
            SeekFrom::Current(offset) => {
                self.position = (self.position as i64 + offset) as u64;
                Ok(self.position)
            }
            SeekFrom::Start(pos) => {
                self.position = pos;
                Ok(self.position)
            }
            SeekFrom::End(_) => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "SeekFrom::End not supported in streaming mode",
            )),
        }
    }
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    const DEFAULT_FILENAME: &str = "test_av1.webm";

    info!("Starting MKV streaming server on http://localhost:3030");
    info!("Example URLs:");
    info!("  - http://localhost:3030/video?file={}", DEFAULT_FILENAME);
    info!("  - http://localhost:3030/video?file={}&start=5&end=15", DEFAULT_FILENAME);
    info!("  - http://localhost:3030/video?file={}&start=10&tracks=2", DEFAULT_FILENAME);
    info!("  - http://localhost:3030/video?file={}&tracks=2,3", DEFAULT_FILENAME);
    info!("  - http://localhost:3030/video?file={}&tracks=2,4", DEFAULT_FILENAME);


    let video_route = warp::path("video")
        .and(warp::query::<HashMap<String, String>>())
        .and_then(handle_video_request);

    warp::serve(video_route).run(([127, 0, 0, 1], 3030)).await;
}

async fn handle_video_request(
    params: HashMap<String, String>,
) -> Result<warp::reply::Response, Infallible> {
    // Resolve file param: basename only, anchored to the project root
    let file_name = params.get("file").map(|s| s.as_str()).unwrap_or("");
    if file_name.is_empty() {
        let response = warp::http::Response::builder()
            .status(400)
            .body(hyper::Body::from("Missing 'file' query parameter. Example: /video?file=myvideo.webm"))
            .unwrap();
        return Ok(response);
    }
    // Strip any path separators to prevent traversal
    let safe_name = std::path::Path::new(file_name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if safe_name.is_empty() || safe_name != file_name {
        let response = warp::http::Response::builder()
            .status(400)
            .body(hyper::Body::from("Invalid filename. Use a plain filename, e.g. myvideo.webm"))
            .unwrap();
        return Ok(response);
    }
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let input_path = project_root.join(&safe_name);
    if !input_path.exists() {
        let response = warp::http::Response::builder()
            .status(404)
            .body(hyper::Body::from(format!("File not found: {safe_name}")))
            .unwrap();
        return Ok(response);
    }

    let start_sec = params
        .get("start")
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);

    let end_sec = params.get("end").and_then(|s| s.parse::<f64>().ok());

    let tracks: Vec<u64> = params
        .get("tracks")
        .map(|s| {
            s.split(',')
                .filter_map(|t| t.trim().parse::<u64>().ok())
                .collect()
        })
        .unwrap_or_default();

    let cut_mode = match params.get("seek").map(|s| s.as_str()) {
        Some("squeeze") => Some(RemuxerCutMode::Squeeze),
        Some("dirty") => Some(RemuxerCutMode::DirtyCut),
        Some("snap_prev") => Some(RemuxerCutMode::SnapPreviousKeyframe),
        Some("snap_next") => Some(RemuxerCutMode::SnapNextKeyframe),
        Some("snap") => Some(RemuxerCutMode::SnapNearestKeyframe),
        _ => None,
    };

    info!(
        "Request: file={} start={}s, end={:?}s, tracks={:?}, cut_mode={:?}",
        safe_name, start_sec, end_sec, tracks, cut_mode
    );

    // Create a channel for streaming chunks
    let (tx, rx) = mpsc::unbounded_channel::<Result<Bytes, std::io::Error>>();

    // Spawn blocking task for remuxer intialization since it may involve file I/O and processing
    let (remuxer, output_interval) =  match tokio::task::spawn_blocking(move || {
        process_video_request(input_path, start_sec, end_sec, tracks, cut_mode, tx.clone())
    }).await.unwrap() {
        Ok(result) => result,
        Err(e) => {
            error!("Error initializing remuxer: {}", e);
            let response = warp::http::Response::builder()
                .status(500)
                .body(hyper::Body::from(format!("Internal server error: {}", e)))
                .unwrap();
            return Ok(response);
        }
    };


    // spawn another blocking task to run the remuxer loop so it doesn't block the async response handling
    tokio::task::spawn_blocking(move || {
        let mut remuxer = remuxer;
        loop {
            remuxer = match remuxer.process() {
                Ok(state) => match state {
                    RemuxerState::Processing(remuxer) => remuxer,
                    RemuxerState::Done(stats) => {
                        info!("Remuxing completed: {:?}", stats);
                        return ();
                    }
                },
                Err(e) => {
                    error!("Error during remuxing: {}", e);
                    return ();
                }
            };
        }
    });

    // Convert receiver to a Stream, then wrap with hyper::Body::wrap_stream
    let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
    let body = warp::hyper::Body::wrap_stream(stream);

    // safe the start and end times of the segment we are streaming in custom headers so client can use them if needed
    let start_sec = output_interval.start_ns.map(|ns| ns as f64 / 1_000_000_000.0).unwrap_or(0.0);
    let end_sec = output_interval.end_ns.map(|ns| ns as f64 / 1_000_000_000.0).unwrap_or(0.0);

    let response = warp::http::Response::builder()
        .status(200)
        .header("Content-Type", "video/webm")
        .header("Cache-Control", "no-cache")
        .header("X-Media-Start-Sec", format!("{:.3}", start_sec))
        .header("X-Media-End-Sec", format!("{:.3}", end_sec))
        .body(body)
        .unwrap();

    Ok(response)
}

fn process_video_request(
    input_path: PathBuf,
    start_sec: f64,
    end_sec: Option<f64>,
    tracks: Vec<u64>,
    cut_mode: Option<RemuxerCutMode>,
    tx: mpsc::UnboundedSender<Result<Bytes, std::io::Error>>,
) -> mkv_remuxer::Result<(Remuxer, CutInterval)> {
    let source = FileSource::new(&input_path)?;
    let input = InputSource::from(source);

    let cut_interval = if start_sec > 0.0 || end_sec.is_some() {
        let start_ns = (start_sec * 1_000_000_000.0) as u64;
        let mut interval = CutInterval::new();
        if start_sec > 0.0 {
            interval = interval.with_start(start_ns);
        }
        if let Some(end_s) = end_sec {
            interval = interval.with_end((end_s * 1_000_000_000.0) as u64);
        }
        Some(interval)
    } else {
        None
    };

    let mappings = if !tracks.is_empty() {
        // Use exactly the tracks requested by the caller
        let m = tracks.iter().map(|&t| (0u64, t)).collect::<Vec<_>>();
        Some(m)
    } else {
        None // Include all tracks
    };

    let stream_writer = StreamWriter::new(tx);
    let stream_sink = StreamSink::new(stream_writer)?;
    let output = OutputSink::from(Box::new(stream_sink) as Box<dyn mkv_remuxer::Sink>);

    info!("Starting remux...");
    Remuxer::new(vec![input], output, cut_interval, cut_mode, mappings)
}
