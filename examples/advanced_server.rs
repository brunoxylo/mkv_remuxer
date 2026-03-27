/// Advanced MKV server example using warp and mkv_remuxer
///
/// Features:
/// - Serve static frontend files (HTML/JS)
/// - API Endpoint for listing media files with metadata
/// - Remuxing endpoint with support for complex mappings (multiple files), seeking, and track selection
/// - Direct file serving endpoint (via redirect to static server) for proper Range support
///
/// Usage:
///   cargo run --example advanced_server
///   Open http://localhost:3031/
///

use bytes::Bytes;
use log::{error, info, warn};
use mkv_remuxer::{
    ContainerFormat, MkvBasicInfo, Remuxer, RemuxerCutMode, RemuxerState,
    sink::{ChannelWriterWrapper, SinkSender, OutputSink, StreamSink, VttSink, Uninitialized},
    source::{CutInterval, FileSource, InputSource, WebVttSource, Source},
    Result as MkvResult, Error as MkvError,
    remux,
};
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use tokio::sync::mpsc;
use warp::hyper;
use warp::Filter;
use mkv_remuxer::sink::Sink;

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    info!("Starting Advanced MKV Remuxer Server on http://localhost:3031");

    // Static files
    let static_files = warp::fs::dir("examples/static");
    // Also serve the current directory as static files for "Direct Stream" support
    // Be careful in production, this exposes all files.
    let static_media = warp::path("static_media").and(warp::fs::dir("."));

    // GET /video/list
    let list_route = warp::get()
        .and(warp::path("video"))
        .and(warp::path("list"))
        .and(warp::path::end())
        .and_then(handle_list_files);

    // GET /video/stream?mappings=0_1,1_2&start=10&end=20&seek=squeeze
    let stream_route = warp::get()
        .and(warp::path("video"))
        .and(warp::path("stream"))
        .and(warp::path::end())
        .and(warp::query::<HashMap<String, String>>())
        .and_then(handle_stream_request);

    // GET /video/direct/:index
    let direct_route = warp::get()
        .and(warp::path("video"))
        .and(warp::path("direct"))
        .and(warp::path::param::<usize>())
        .and(warp::path::end())
        .and_then(handle_direct_request);

    let cors = warp::cors()
        .allow_any_origin()
        .allow_methods(vec!["GET", "POST", "DELETE"]);

    let routes = static_files
        .or(static_media)
        .or(list_route)
        .or(stream_route)
        .or(direct_route)
        .with(cors);

    warp::serve(routes).run(([127, 0, 0, 1], 3031)).await;
}

fn scan_media_files() -> Vec<PathBuf> {
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut paths: Vec<PathBuf> = match std::fs::read_dir(&project_root) {
        Ok(read_dir) => read_dir
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && matches!(
                    p.extension().and_then(std::ffi::OsStr::to_str).unwrap_or(""),
                    "mkv" | "webm" | "vtt"
                )
        })
        .collect(),
        Err(_) => vec![]
    };
    paths.sort();
    paths
}

async fn handle_list_files() -> Result<warp::reply::Response, Infallible> {
    let infos: Vec<MkvBasicInfo> = tokio::task::block_in_place(|| {
        scan_media_files()
            .into_iter()
            .filter_map(|path| {
                let is_vtt = path.extension().and_then(std::ffi::OsStr::to_str).unwrap_or("") == "vtt";
                 match std::fs::File::open(&path) {
                    Ok(file) => {
                        let file_name = path.file_name().and_then(std::ffi::OsStr::to_str).unwrap_or("unknown").to_string();
                        let file_stem = path.file_stem().and_then(std::ffi::OsStr::to_str).unwrap_or("unknown").to_string();
                         let result: mkv_remuxer::Result<Box<dyn Source>> = if is_vtt {
                             match WebVttSource::new(file, file_stem.to_string(), false) {
                                 Ok(s) => Ok(Box::new(s.with_file_name(file_name))),
                                 Err(e) => Err(e),
                             }
                        } else {
                            match FileSource::new(file) {
                                Ok(s) => Ok(Box::new(s.with_file_name(file_name))),
                                Err(e) => Err(e),
                            }
                        };
                        match result {
                            Ok(src) => src.get_basic_info().ok(),
                            Err(e) => {
                                warn!("Failed to probe {:?}: {}", path, e);
                                None
                            }
                        }
                    },
                    Err(_) => None
                 }
            })
            .collect()
    });

    let json = serde_json::to_string_pretty(&infos).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}" ));
    let response = warp::http::Response::builder()
        .status(200)
        .header("Content-Type", "application/json")
        .body(hyper::Body::from(json))
        .unwrap();
    Ok(response)
}

async fn handle_direct_request(index: usize) -> Result<warp::reply::Response, Infallible> {
    let files = scan_media_files();
    match files.get(index) {
        Some(path) => {
            // make path relative to CWD to serve via static_media
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let relative_path = path.strip_prefix(&cwd).unwrap_or(path);
            let url_path = relative_path.to_string_lossy().replace('\\', "/"); 
            // Redirect
            let location = format!("/static_media/{}", url_path);
            let response = warp::http::Response::builder()
                .status(307)
                .header("Location", location)
                .body(hyper::Body::empty())
                .unwrap();
            Ok(response)
        },
        None => {
            Ok(warp::http::Response::builder().status(404).body(hyper::Body::from("Not Found")).unwrap())
        }
    }
}

async fn handle_stream_request(params: HashMap<String, String>) -> Result<warp::reply::Response, Infallible> {
    // 1. Parse Params
    let start_sec = params.get("start").and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
    let end_sec = params.get("end").and_then(|s| s.parse::<f64>().ok());
    let seek_mode_str = params.get("seek").map(|s| s.as_str()).unwrap_or("squeeze");
    // vtt_output=true → use VttSink and respond with text/vtt instead of video/webm.
    // The client must set this explicitly when requesting a subtitle-only track.
    // Mirrors the CLI behaviour where the output filename extension decides the sink type.
    let vtt_output = params.get("vtt_output").map(|v| v == "true").unwrap_or(false);
    
    let cut_mode = match seek_mode_str {
        "squeeze" => Some(RemuxerCutMode::Squeeze),
        "dirty" => Some(RemuxerCutMode::DirtyCut),
        "snap" => Some(RemuxerCutMode::SnapNearestKeyframe),
        "snap_prev" => Some(RemuxerCutMode::SnapPreviousKeyframe),
        "snap_next" => Some(RemuxerCutMode::SnapNextKeyframe),
        _ => Some(RemuxerCutMode::Squeeze), // Default to squeeze as per user request
    };

    // mappings=fileIdx_trackId,fileIdx_trackId
    let mappings_str = params.get("mappings").unwrap_or(&String::new()).clone();
    
    // Scan files once to map indices
    let all_files = scan_media_files();

    let (tx, rx) = mpsc::channel::<Bytes>(4); // Increased buffer size a bit

    let result = tokio::task::spawn_blocking(move || {
        initialize_remuxer(all_files, mappings_str, start_sec, end_sec, cut_mode, vtt_output, tx)
    }).await.unwrap();

    let (remuxer, output_interval) = match result {
        Ok(r) => r,
        Err(e) => {
            error!("Remuxer init failed: {}", e);
            return Ok(warp::http::Response::builder().status(400).body(hyper::Body::from(format!("Error: {}", e))).unwrap());
        }
    };

    let output_format = remuxer.get_output_container_format();

    // Spawn processing loop
    tokio::task::spawn_blocking(move || {
        let mut remuxer = remuxer;
        let mut count = 0;
        loop {
            match remuxer.process() {
                Ok(state) => match state {
                    RemuxerState::Processing(r) => {
                        remuxer = r;
                        count += 1;
                        if count % 100 == 0 {
                            // Yield to avoid blocking thread too long? 
                            // Not really needed in spawn_blocking unless we want to be nice
                        }
                    },
                    RemuxerState::Done(_) => break,
                },
                Err(e) => {
                    error!("Remuxing loop error: {}", e);
                    break;
                }
            }
        }
    });

    // Output Headers
    let content_type = match output_format {
        ContainerFormat::Vtt => "text/vtt; charset=utf-8",
        ContainerFormat::WebM => "video/webm",
        ContainerFormat::Mkv => "video/mkv", // or video/x-matroska
    };

    let start_sec_out = output_interval.start_ns.map(|ns| ns as f64 / 1e9).unwrap_or(0.0);
    // If end_sec is None, we don't know the duration until processed, so we omit header or send 0?
    // RemuxerCutMode might have adjusted it.

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let mapped = tokio_stream::StreamExt::map(stream, |c| Ok::<_, Infallible>(c));
    let body = hyper::Body::wrap_stream(mapped);

    let response = warp::http::Response::builder()
        .status(200)
        .header("Content-Type", content_type)
        .header("X-Media-Start", format!("{:.3}", start_sec_out))
        .body(body)
        .unwrap();

    Ok(response)
}

fn initialize_remuxer(
    all_files: Vec<PathBuf>,
    mappings_str: String,
    start_sec: f64,
    end_sec: Option<f64>,
    cut_mode: Option<RemuxerCutMode>,
    vtt_output: bool,
    tx: mpsc::Sender<Bytes>
) -> mkv_remuxer::Result<(Remuxer, CutInterval)> {
    
    // Parse mappings: [(user_file_idx, track_id)]
    let user_mappings: Vec<(usize, u32)> = mappings_str.split(',')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let parts: Vec<&str> = s.split('_').collect();
            if parts.len() != 2 {
                return Err(mkv_remuxer::Error::InvalidConfig(format!("Invalid mapping: {}", s)));
            }
            let f_idx = parts[0].parse::<usize>().map_err(|_| mkv_remuxer::Error::InvalidConfig("Invalid file index".into()))?;
            let t_id = parts[1].parse::<u32>().map_err(|_| mkv_remuxer::Error::InvalidConfig("Invalid track id".into()))?;
            Ok((f_idx, t_id))
        })
        .collect::<Result<Vec<_>, _>>()?;

    if user_mappings.is_empty() {
        return Err(mkv_remuxer::Error::InvalidConfig("No mappings provided".into()));
    }

    // Determine unique files needed
    let mut unique_file_indices: Vec<usize> = user_mappings.iter().map(|(f, _)| *f).collect();
    unique_file_indices.sort();
    unique_file_indices.dedup();

    // Map user_file_index -> remuxer_source_index
    let mut user_to_remuxer_map: HashMap<usize, usize> = HashMap::new();
    let mut sources: Vec<InputSource> = Vec::new();

    for (remuxer_idx, user_idx) in unique_file_indices.iter().enumerate() {
        if *user_idx >= all_files.len() {
            return Err(mkv_remuxer::Error::InvalidConfig(format!("File index {} out of bounds", user_idx)));
        }
        let path = &all_files[*user_idx];
        let is_vtt = path.extension().and_then(std::ffi::OsStr::to_str).unwrap_or("") == "vtt";
        let input_source = if is_vtt {
            let file = std::fs::File::open(path)?;
            let file_name = path.file_name().and_then(std::ffi::OsStr::to_str).unwrap_or("unknown");
             let file_stem = path.file_stem().and_then(std::ffi::OsStr::to_str).unwrap_or("unknown");
            let src = WebVttSource::new(file, file_stem.to_string(), false)?.with_file_name(file_name.to_string());
            InputSource::from(src)
        } else {
            let file = std::fs::File::open(path)?;
             let file_name = path.file_name().and_then(std::ffi::OsStr::to_str).unwrap_or("unknown");
            let src = FileSource::new(file)?.with_file_name(file_name.to_string());
            InputSource::from(src)
        };
        
        sources.push(input_source);
        user_to_remuxer_map.insert(*user_idx, remuxer_idx);
    }

    // Prepare Remuxer Mappings: [(remuxer_source_index, track_id)]
    let remuxer_mappings: Vec<(u64, u64)> = user_mappings.iter().map(|(u_idx, t_id)| {
        let r_idx = user_to_remuxer_map.get(u_idx).unwrap(); // Should exist
        (*r_idx as u64, *t_id as u64)
    }).collect();

    // Use VttSink only when the caller explicitly requests it via vtt_output=true.
    // This mirrors the CLI behaviour where the output filename extension decides the sink type.
    // If the client sets vtt_output=true but the mapping does not resolve to a single text
    // subtitle track, Remuxer::new() will return an error (which the caller will receive as
    // a 400 response).
    let writer = ChannelWriterWrapper::new(SinkSender::Tokio(tx));
    let output_sink: OutputSink<Uninitialized> = if vtt_output {
        let vtt_sink = VttSink::new(writer);
        OutputSink::new(Box::new(vtt_sink) as Box<dyn Sink>)
    } else {
        let stream_sink = StreamSink::new(writer)?;
        OutputSink::new(Box::new(stream_sink) as Box<dyn Sink>)
    };

    // Cut Interval
    let cut_interval = if start_sec > 0.0 || end_sec.is_some() {
        let mut interval = CutInterval::new();
        if start_sec > 0.0 {
            interval = interval.with_start((start_sec * 1e9) as u64);
        }
        if let Some(end) = end_sec {
            interval = interval.with_end((end * 1e9) as u64);
        }
        Some(interval)
    } else {
        None
    };

    Remuxer::new(sources, output_sink, cut_interval, cut_mode, Some(remuxer_mappings))
}
