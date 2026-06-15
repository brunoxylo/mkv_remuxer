/// Advanced MKV server example using warp and mkv_remuxer
///
/// Features:
/// - Serve static frontend files (HTML/JS)
/// - API Endpoint for listing media files with metadata
/// - Remuxing endpoint with support for complex mappings (multiple files), seeking, and track selection
/// - Direct file serving endpoint (via redirect to static server) for proper Range support
/// - **Session-based chunked streaming** for resilient, segment-by-segment delivery
///
/// Session API:
///   GET  /my_video/start_stream_session?mappings=0_1,1_2&start=10&end=20&seek=squeeze  → create session
///   GET  /sessions/{id}/segment          → get current segment (idempotent, retry-safe)
///   POST /sessions/{id}/next             → advance to next segment
///   GET  /sessions/{id}/step             → get current step index
///   DELETE /sessions/{id}                → destroy session
///
/// Usage:
///   cargo run --example advanced_server
///   Open http://localhost:3031/
///
use bytes::Bytes;
use log::{error, info};
use mkv_remuxer::RemuxerCutMode;
use mkv_remuxer::session_streamer::SessionStreamer;
use mkv_remuxer::session::SessionStore;
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
    let streamer = Arc::new(SessionStreamer::new(root_dir));
    let streamer_filter = warp::any().map(move || streamer.clone());

    let session_store = SessionStore::new();
    let session_filter = {
        let store = Arc::clone(&session_store);
        warp::any().map(move || Arc::clone(&store))
    };

    // Static files
    let static_files = warp::fs::dir("examples/static");
    let static_media = warp::path("static_media").and(warp::fs::dir("."));

    // GET /my_video
    let list_route = warp::get()
        .and(warp::path("my_video"))
        .and(warp::path::end())
        .and(streamer_filter.clone())
        .and_then(handle_list_files);


    // GET /my_video/direct/:index
    let direct_route = warp::get()
        .and(warp::path("my_video"))
        .and(warp::path("direct"))
        .and(warp::path::param::<usize>())
        .and(warp::path::end())
        .and(warp::header::optional::<String>("range"))
        .and(streamer_filter.clone())
        .and_then(handle_direct_request);

    // ── Session routes ──────────────────────────────────────────────────

    // GET /my_video/start_stream_session?mappings=...&start=...&end=...&seek=...  (create session)
    let session_create = warp::get()
        .and(warp::path("my_video"))
        .and(warp::path("start_stream_session"))
        .and(warp::path::end())
        .and(warp::query::<HashMap<String, String>>())
        .and(streamer_filter.clone())
        .and(session_filter.clone())
        .and_then(handle_session_create);



    // GET /sessions/{id}/segment
    let session_segment = warp::get()
        .and(warp::path("sessions"))
        .and(warp::path::param::<String>())
        .and(warp::path("segment"))
        .and(warp::path::end())
        .and(session_filter.clone())
        .and_then(handle_session_segment);

    // POST /sessions/{id}/next
    let session_next = warp::post()
        .and(warp::path("sessions"))
        .and(warp::path::param::<String>())
        .and(warp::path("next"))
        .and(warp::path::end())
        .and(session_filter.clone())
        .and_then(handle_session_next);

    // GET /sessions/{id}/step
    let session_step = warp::get()
        .and(warp::path("sessions"))
        .and(warp::path::param::<String>())
        .and(warp::path("step"))
        .and(warp::path::end())
        .and(session_filter.clone())
        .and_then(handle_session_step);

    // DELETE /sessions/{id}
    let session_destroy = warp::delete()
        .and(warp::path("sessions"))
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(session_filter.clone())
        .and_then(handle_session_destroy);

    let cors = warp::cors()
        .allow_any_origin()
        .allow_methods(vec!["GET", "POST", "DELETE"])
        .allow_header("content-type")
        .expose_headers(vec![
            "X-Media-Start-Sec",
            "X-Media-End-Sec",
            "X-Session-Id",
            "X-Mime-Type",
            "X-Container-Format",
        ]);

    let routes = static_files
        .or(static_media)
        .or(list_route)
        .or(direct_route)
        .or(session_create)
        .or(session_segment)
        .or(session_next)
        .or(session_step)
        .or(session_destroy)
        .with(cors);

    warp::serve(routes).run(([127, 0, 0, 1], 3031)).await;
}

// ── Existing handlers (unchanged) ──────────────────────────────────────────

async fn handle_list_files(
    streamer: Arc<SessionStreamer>,
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
    streamer: Arc<SessionStreamer>,
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


// ── Session handlers ───────────────────────────────────────────────────────

fn parse_cut_mode(s: &str) -> Option<RemuxerCutMode> {
    match s {
        "squeeze" => Some(RemuxerCutMode::Squeeze),
        "dirty" => Some(RemuxerCutMode::DirtyCut),
        "snap" => Some(RemuxerCutMode::SnapNearestKeyframe),
        "snap_prev" => Some(RemuxerCutMode::SnapPreviousKeyframe),
        "snap_next" => Some(RemuxerCutMode::SnapNextKeyframe),
        _ => Some(RemuxerCutMode::Squeeze),
    }
}

/// GET /my_video/start_stream_session?mappings=0_1,1_2&start=10&end=20&seek=squeeze
async fn handle_session_create(
    params: HashMap<String, String>,
    streamer: Arc<SessionStreamer>,
    store: Arc<SessionStore>,
) -> Result<warp::reply::Response, Infallible> {
    use warp::Reply;

    let client_id = params
        .get("client_id")
        .cloned()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let mappings = params
        .get("mappings")
        .cloned()
        .unwrap_or_default();
    let start_sec = params
        .get("start")
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let end_sec = params.get("end").and_then(|s| s.parse::<f64>().ok());
    let cut_mode = params
        .get("seek")
        .map(|s| parse_cut_mode(s))
        .flatten();

    info!(
        "Creating session: client_id={}, mappings={}, start={}, end={:?}",
        client_id, mappings, start_sec, end_sec
    );

    let result = tokio::task::spawn_blocking(move || {
        streamer.create_chunked_session(mappings, start_sec, end_sec, cut_mode)
    })
    .await;

    let (remuxer, mime_type, container_format, start_out, end_out) = match result {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            error!("Session creation error: {}", e);
            return Ok(warp::reply::with_status(
                warp::reply::json(&serde_json::json!({"error": e.to_string()})),
                warp::http::StatusCode::BAD_REQUEST,
            )
            .into_response());
        }
        Err(e) => {
            error!("Session task panic: {}", e);
            return Ok(warp::reply::with_status(
                warp::reply::json(&serde_json::json!({"error": "Internal error"})),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            )
            .into_response());
        }
    };

    let session_info = match store.create_session(
        client_id,
        remuxer,
        mime_type,
        container_format,
        start_out,
        end_out,
    ) {
        Ok(info) => info,
        Err(e) => {
            error!("Session init error: {}", e);
            return Ok(warp::reply::with_status(
                warp::reply::json(&serde_json::json!({"error": e.to_string()})),
                warp::http::StatusCode::BAD_REQUEST,
            )
            .into_response());
        }
    };

    let response_json = serde_json::json!({
        "session_id": session_info.session_id,
        "mime_type": session_info.mime_type,
        "container_format": session_info.container_format.to_string(),
        "start_sec": session_info.start_sec,
        "end_sec": session_info.end_sec,
        "step": 0,
    });

    Ok(warp::reply::with_status(warp::reply::json(&response_json), warp::http::StatusCode::CREATED)
        .into_response())
}

/// GET /sessions/{id}/segment  → binary current segment (idempotent)
async fn handle_session_segment(
    session_id: String,
    store: Arc<SessionStore>,
) -> Result<warp::reply::Response, Infallible> {
    match store.get_segment(&session_id) {
        Ok(bytes) => {
            let response = warp::http::Response::builder()
                .status(200)
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", bytes.len().to_string())
                .body(hyper::Body::from(bytes))
                .unwrap();
            Ok(response)
        }
        Err(e) => {
            let response = warp::http::Response::builder()
                .status(404)
                .header("Content-Type", "application/json")
                .body(hyper::Body::from(
                    serde_json::json!({"error": e.to_string()}).to_string(),
                ))
                .unwrap();
            Ok(response)
        }
    }
}

/// POST /sessions/{id}/next  → advance iterator, return new step or error if finished
async fn handle_session_next(
    session_id: String,
    store: Arc<SessionStore>,
) -> Result<warp::reply::Response, Infallible> {
    use warp::Reply;

    let store_clone = Arc::clone(&store);
    let sid = session_id.clone();

    let result =
        tokio::task::spawn_blocking(move || store_clone.advance(&sid)).await;

    match result {
        Ok(Ok(resp)) => {
            Ok(warp::reply::json(&serde_json::json!({"step": resp.step})).into_response())
        }
        Ok(Err(e)) => {
            let status = if e.to_string().contains("not found") {
                404
            } else if e.to_string().contains("finished") {
                410 // Gone
            } else {
                500
            };
            Ok(warp::reply::with_status(
                warp::reply::json(&serde_json::json!({"error": e.to_string()})),
                warp::http::StatusCode::from_u16(status).unwrap(),
            )
            .into_response())
        }
        Err(e) => Ok(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({"error": format!("Task panic: {}", e)})),
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        )
        .into_response()),
    }
}

/// GET /sessions/{id}/step  → current step index
async fn handle_session_step(
    session_id: String,
    store: Arc<SessionStore>,
) -> Result<warp::reply::Response, Infallible> {
    use warp::Reply;
    match store.get_step(&session_id) {
        Ok((step, finished)) => {
            Ok(warp::reply::json(&serde_json::json!({"step": step, "finished": finished}))
                .into_response())
        }
        Err(e) => Ok(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({"error": e.to_string()})),
            warp::http::StatusCode::NOT_FOUND,
        )
        .into_response()),
    }
}

/// DELETE /sessions/{id}  → destroy session
async fn handle_session_destroy(
    session_id: String,
    store: Arc<SessionStore>,
) -> Result<warp::reply::Response, Infallible> {
    let _ = store.destroy_session(&session_id);
    let response = warp::http::Response::builder()
        .status(204)
        .body(hyper::Body::empty())
        .unwrap();
    Ok(response)
}
