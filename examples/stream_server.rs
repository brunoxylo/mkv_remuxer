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
    Remuxer, RemuxerCutMode, RemuxerState,
    sink::{OutputSink, StreamSink, VttSink},
    source::{CutInterval, FileSource, InputSource, SeekType, WebVttSource},
    MkvBasicInfo,
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

    info!("Starting MKV streaming server on http://localhost:3030");
    info!("  GET /video          → JSON array of MkvBasicInfo for all .mkv/.webm files");
    info!("  GET /video/0        → stream file at index 0");
    info!("  GET /video/0?start=5&end=15");
    info!("  GET /video/0?tracks=2,3");
    info!("  GET /video/0?seek=snap&start=10");

    // GET /video           → list all media files as JSON
    let list_route = warp::get()
        .and(warp::path("video"))
        .and(warp::path::end())
        .and_then(handle_files_request);

    // GET /video/{index}   → stream file by index
    let stream_route = warp::get()
        .and(warp::path("video"))
        .and(warp::path::param::<usize>())
        .and(warp::path::end())
        .and(warp::query::<HashMap<String, String>>())
        .and_then(handle_video_request);

    let routes = list_route.or(stream_route);
    warp::serve(routes).run(([127, 0, 0, 1], 3030)).await;
}

fn scan_media_files() -> Vec<PathBuf> {
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&project_root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && matches!(
                    p.extension().and_then(|e| e.to_str()).unwrap_or(""),
                    "mkv" | "webm" | "vtt"
                )
        })
        .collect();
    paths.sort();
    paths
}

async fn handle_video_request(
    index: usize,
    params: HashMap<String, String>,
) -> Result<warp::reply::Response, Infallible> {
    let files = scan_media_files();
    let input_path = match files.get(index) {
        Some(p) => p.clone(),
        None => {
            let response = warp::http::Response::builder()
                .status(404)
                .body(hyper::Body::from(format!(
                    "No media file at index {index}. {} file(s) available. GET /video for the list.",
                    files.len()
                )))
                .unwrap();
            return Ok(response);
        }
    };
    let safe_name = input_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("file[{index}]"));
    let is_vtt = input_path.extension().and_then(|e| e.to_str()).unwrap_or("") == "vtt";

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
        "Request: index={} file={} start={}s, end={:?}s, tracks={:?}, cut_mode={:?}",
        index, safe_name, start_sec, end_sec, tracks, cut_mode
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

    let content_type = if is_vtt { "text/vtt; charset=utf-8" } else { "video/webm" };

    let response = warp::http::Response::builder()
        .status(200)
        .header("Content-Type", content_type)
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
    let is_vtt = input_path.extension().and_then(|e| e.to_str()).unwrap_or("") == "vtt";
    let input: InputSource = if is_vtt {
        InputSource::from(WebVttSource::new(&input_path, "und")?)
    } else {
        InputSource::from(FileSource::new(&input_path)?)
    };

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
    let output = if is_vtt {
        OutputSink::from(Box::new(VttSink::new(stream_writer)) as Box<dyn mkv_remuxer::Sink>)
    } else {
        let stream_sink = StreamSink::new(stream_writer)?;
        OutputSink::from(Box::new(stream_sink) as Box<dyn mkv_remuxer::Sink>)
    };

    info!("Starting remux...");
    Remuxer::new(vec![input], output, cut_interval, cut_mode, mappings)
}

async fn handle_files_request() -> Result<warp::reply::Response, Infallible> {
    let infos: Vec<MkvBasicInfo> = tokio::task::block_in_place(|| {
        use mkv_remuxer::source::Source;
        scan_media_files()
            .into_iter()
            .filter_map(|path| {
                let is_vtt = path.extension().and_then(|e| e.to_str()).unwrap_or("") == "vtt";
                let result: mkv_remuxer::Result<Box<dyn Source>> = if is_vtt {
                    WebVttSource::new(&path, "und").map(|s| Box::new(s) as Box<dyn Source>)
                } else {
                    FileSource::new(&path).map(|s| Box::new(s) as Box<dyn Source>)
                };
                match result {
                    Ok(src) => src.get_basic_info().ok(),
                    Err(e) => {
                        error!("Failed to open {:?}: {}", path, e);
                        None
                    }
                }
            })
            .collect()
    });

    let json = serde_json::to_string_pretty(&infos).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}" ));
    let response = warp::http::Response::builder()
        .status(200)
        .header("Content-Type", "application/json")
        .body(warp::hyper::Body::from(json))
        .unwrap();
    Ok(response)
}
