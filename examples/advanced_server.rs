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
use log::{error, info};
use mkv_remuxer::ContainerFormat;
use mkv_remuxer::RemuxerCutMode;
use mkv_remuxer::folder_streamer::FolderStreamer;
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use warp::Filter;
use warp::hyper;

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    info!("Starting Advanced MKV Remuxer Server on http://localhost:3031");

    let root_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let streamer = Arc::new(FolderStreamer::new(root_dir));
    let streamer_filter = warp::any().map(move || streamer.clone());

    // Static files
    let static_files = warp::fs::dir("examples/static");
    let static_media = warp::path("static_media").and(warp::fs::dir("."));

    // GET /video/list
    let list_route = warp::get()
        .and(warp::path("my_video"))
        .and(warp::path::end())
        .and(streamer_filter.clone())
        .and_then(handle_list_files);

    // GET /video/stream?mappings=0_1,1_2&start=10&end=20&seek=squeeze
    let stream_route = warp::get()
        .and(warp::path("my_video"))
        .and(warp::path("remux"))
        .and(warp::path::end())
        .and(warp::query::<HashMap<String, String>>())
        .and(streamer_filter.clone())
        .and_then(handle_stream_request);

    // GET /video/direct/:index
    let direct_route = warp::get()
        .and(warp::path("my_video"))
        .and(warp::path("direct"))
        .and(warp::path::param::<usize>())
        .and(warp::path::end())
        .and(warp::header::optional::<String>("range"))
        .and(streamer_filter.clone())
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

async fn handle_list_files(
    streamer: Arc<FolderStreamer>,
) -> Result<warp::reply::Response, Infallible> {
    use warp::Reply;
    match tokio::task::spawn_blocking(move || streamer.list_files()).await {
        Ok(infos) => Ok(warp::reply::json(&infos).into_response()),
        Err(e) => {
            let mut map = HashMap::new();
            map.insert("error", format!("Error: {}", e));
            Ok(warp::reply::with_status(
                warp::reply::json(&map),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            )
            .into_response())
        }
    }
}

async fn handle_direct_request(
    index: usize,
    range_header: Option<String>,
    streamer: Arc<FolderStreamer>,
) -> Result<warp::reply::Response, Infallible> {
    let mut start_byte = 0;
    let mut end_byte = None;

    if let Some(range) = range_header {
        if range.starts_with("bytes=") {
            let parts: Vec<&str> = range["bytes=".len()..].split('-').collect();
            if !parts.is_empty() {
                if let Ok(s) = parts[0].parse::<u64>() {
                    start_byte = s;
                }
                if parts.len() > 1 && !parts[1].is_empty() {
                    if let Ok(e) = parts[1].parse::<u64>() {
                        end_byte = Some(e);
                    }
                }
            }
        }
    }

    let (tx, rx) = mpsc::channel::<Bytes>(4);

    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();

    tokio::task::spawn_blocking(move || {
        streamer.start_direct_stream(index, start_byte, end_byte, tx, oneshot_tx);
    });

    let file_size_result = oneshot_rx
        .await
        .unwrap_or_else(|_| Err(mkv_remuxer::Error::InternalBug("Stream thread died".into())));

    match file_size_result {
        Ok(file_size) => {
            let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
            let mapped = tokio_stream::StreamExt::map(stream, |c| Ok::<_, Infallible>(c));
            let body = hyper::Body::wrap_stream(mapped);

            let end = end_byte.unwrap_or(file_size.saturating_sub(1));
            let content_range = format!("bytes {}-{}/{}", start_byte, end, file_size);

            let response = warp::http::Response::builder()
                .status(206) // Partial Content
                .header("Content-Range", content_range)
                .header("Accept-Ranges", "bytes")
                .header("Content-Length", (end - start_byte + 1).to_string())
                // Use application/octet-stream or let browser infer; ideally we'd guess based on extension
                .header("Content-Type", "video/webm")
                .body(body)
                .unwrap();

            Ok(response)
        }
        Err(e) => {
            let response = warp::http::Response::builder()
                .status(404)
                .body(hyper::Body::from(format!("Direct file error: {}", e)))
                .unwrap();
            Ok(response)
        }
    }
}

async fn handle_stream_request(
    params: HashMap<String, String>,
    streamer: Arc<FolderStreamer>,
) -> Result<warp::reply::Response, Infallible> {
    let start_sec = params
        .get("start")
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let end_sec = params.get("end").and_then(|s| s.parse::<f64>().ok());
    let seek_mode_str = params.get("seek").map(|s| s.as_str()).unwrap_or("squeeze");
    let vtt_output = params
        .get("vtt_output")
        .map(|v| v == "true")
        .unwrap_or(false);

    let cut_mode = match seek_mode_str {
        "squeeze" => Some(RemuxerCutMode::Squeeze),
        "dirty" => Some(RemuxerCutMode::DirtyCut),
        "snap" => Some(RemuxerCutMode::SnapNearestKeyframe),
        "snap_prev" => Some(RemuxerCutMode::SnapPreviousKeyframe),
        "snap_next" => Some(RemuxerCutMode::SnapNextKeyframe),
        _ => Some(RemuxerCutMode::Squeeze),
    };

    let mappings_str = params.get("mappings").unwrap_or(&String::new()).clone();

    let (tx, rx) = mpsc::channel::<Bytes>(4);

    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();

    tokio::task::spawn_blocking(move || {
        streamer.start_remuxing_stream(
            mappings_str,
            start_sec,
            end_sec,
            cut_mode,
            vtt_output,
            tx,
            oneshot_tx,
        );
    });

    let stream_result = oneshot_rx
        .await
        .unwrap_or_else(|_| Err(mkv_remuxer::Error::InternalBug("Stream thread died".into())));

    match stream_result {
        Ok((output_format, start_sec_out, end_sec_out)) => {
            let content_type = match output_format {
                ContainerFormat::Vtt => "text/vtt; charset=utf-8",
                ContainerFormat::WebM => "video/webm",
                ContainerFormat::Mkv => "video/mkv",
            };

            let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
            let mapped = tokio_stream::StreamExt::map(stream, |c| Ok::<_, Infallible>(c));
            let body = hyper::Body::wrap_stream(mapped);

            let response = warp::http::Response::builder()
                .status(200)
                .header("Content-Type", content_type)
                .header("X-Media-Start-Sec", format!("{:.3}", start_sec_out))
                .header("X-Media-End-Sec", format!("{:.3}", end_sec_out))
                .header(
                    "Access-Control-Expose-Headers",
                    "X-Media-Start-Sec, X-Media-End-Sec",
                )
                .body(body)
                .unwrap();

            Ok(response)
        }
        Err(e) => {
            error!("Remuxer stream error: {}", e);
            let response = warp::http::Response::builder()
                .status(400)
                .body(hyper::Body::from(format!("Error: {}", e)))
                .unwrap();
            Ok(response)
        }
    }
}
