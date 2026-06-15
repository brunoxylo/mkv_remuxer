use crate::remuxer::{ChunkedRemuxer, ChunkedRemuxerResponse};
use crate::{Codecs, ContainerFormat, Error, RemuxStats, Result};
use bytes::Bytes;
use log::{debug, info, warn};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SESSION_TIMEOUT: Duration = Duration::from_secs(180); // 3 minutes

/// Metadata returned when a session is created.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub mime_type: String,
    pub container_format: ContainerFormat,
    pub start_sec: f64,
    pub end_sec: f64,
}

/// A single streaming session wrapping a ChunkedRemuxer.
///
/// Each session gets its own `Mutex` so blocking I/O on one session
/// never stalls lookups or work on other sessions.
struct Session {
    remuxer: ChunkedRemuxer,
    client_id: String,
    /// The current segment bytes — always populated.
    /// Initialized with the init segment on creation.
    current_segment: Bytes,
    /// Step index (incremented each time `advance()` is called).
    step: u64,
    finished: bool,
    last_activity: Instant,
    mime_type: String,
    container_format: ContainerFormat,
    start_sec: f64,
    end_sec: f64,
}

/// Thread-safe session store. Wrap in `Arc` and share across routes.
///
/// The outer `Mutex` on the `HashMap` is only held briefly to insert/remove/lookup
/// an `Arc<Mutex<Session>>`. The per-session lock is then acquired separately,
/// so blocking remuxer I/O on one session cannot deadlock another.
pub struct SessionStore {
    sessions: Mutex<HashMap<String, Arc<Mutex<Session>>>>,
    /// Maps client_id → session_id for the one-session-per-client constraint.
    client_sessions: Mutex<HashMap<String, String>>,
}

/// Response from advancing the session iterator.
#[derive(Debug, Clone)]
pub struct AdvanceResponse {
    pub step: u64,
}

impl SessionStore {
    pub fn new() -> Arc<Self> {
        let store = Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            client_sessions: Mutex::new(HashMap::new()),
        });

        // Spawn cleanup task
        let store_ref = Arc::clone(&store);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                store_ref.cleanup_expired();
            }
        });

        store
    }

    /// Look up a session Arc without holding the map lock.
    fn get_session(&self, session_id: &str) -> Result<Arc<Mutex<Session>>> {
        let sessions = self.sessions.lock()?;
        sessions
            .get(session_id)
            .cloned()
            .ok_or_else(|| Error::InvalidConfig("Session not found".into()))
    }

    /// Create a new session. Destroys any existing session for the same `client_id`.
    ///
    /// The init segment (EBML header + tracks) is obtained by calling
    /// `remuxer.next_segment()` and becomes the first `current_segment`.
    pub fn create_session(
        &self,
        client_id: String,
        mut remuxer: ChunkedRemuxer,
        mime_type: String,
        container_format: ContainerFormat,
        start_sec: f64,
        end_sec: f64,
    ) -> Result<SessionInfo> {
        // Destroy existing session for this client
        {
            let client_sessions = self.client_sessions.lock()?;
            if let Some(old_id) = client_sessions.get(&client_id) {
                let old_id = old_id.clone();
                drop(client_sessions);
                self.destroy_session(&old_id);
            }
        }

        // Get the init segment as the first current_segment
        let init_segment = match remuxer.next_segment()? {
            ChunkedRemuxerResponse::Segment(bytes) => bytes,
            ChunkedRemuxerResponse::Finished(_) => {
                return Err(Error::InvalidConfig(
                    "Remuxer finished immediately — no data to stream".into(),
                ));
            }
        };

        let session_id = uuid::Uuid::new_v4().to_string();

        let session = Session {
            remuxer,
            client_id: client_id.clone(),
            current_segment: init_segment,
            step: 0,
            finished: false,
            last_activity: Instant::now(),
            mime_type: mime_type.clone(),
            container_format,
            start_sec,
            end_sec,
        };

        let info = SessionInfo {
            session_id: session_id.clone(),
            mime_type,
            container_format,
            start_sec,
            end_sec,
        };

        self.sessions
            .lock()?
            .insert(session_id.clone(), Arc::new(Mutex::new(session)));
        self.client_sessions.lock()?.insert(client_id, session_id);

        info!("Session created: {}", info.session_id);
        Ok(info)
    }

    /// Get the current segment bytes (idempotent — safe to retry on network failure).
    /// Always returns data; the segment is never empty.
    pub fn get_segment(&self, session_id: &str) -> Result<Bytes> {
        let session_arc = self.get_session(session_id)?;
        let mut session = session_arc.lock()?;
        session.last_activity = Instant::now();
        Ok(session.current_segment.clone())
    }

    /// Advance the remuxer to the next segment. Returns the new step index.
    /// Returns an error if the remuxer has already finished.
    ///
    /// This is blocking (file I/O) — call from `spawn_blocking`.
    pub fn advance(&self, session_id: &str) -> Result<AdvanceResponse> {
        let session_arc = self.get_session(session_id)?;
        let mut session = session_arc.lock()?;
        session.last_activity = Instant::now();

        if session.finished {
            return Err(Error::InvalidConfig(
                "Session already finished — no more segments".into(),
            ));
        }

        match session.remuxer.next_segment()? {
            ChunkedRemuxerResponse::Segment(bytes) => {
                session.step += 1;
                session.current_segment = bytes;
                Ok(AdvanceResponse { step: session.step })
            }
            ChunkedRemuxerResponse::Finished(_) => {
                session.finished = true;
                Err(Error::InvalidConfig(
                    "Remuxer finished — no more segments".into(),
                ))
            }
        }
    }

    /// Get the current step index.
    pub fn get_step(&self, session_id: &str) -> Result<(u64, bool)> {
        let session_arc = self.get_session(session_id)?;
        let mut session = session_arc.lock()?;
        session.last_activity = Instant::now();
        Ok((session.step, session.finished))
    }

    /// Destroy a session by ID.
    pub fn destroy_session(&self, session_id: &str) -> Result<()> {
        let mut sessions = self.sessions.lock()?;
        if let Some(session_arc) = sessions.remove(session_id) {
            let session = session_arc.lock()?;
            let mut client_sessions = self.client_sessions.lock()?;
            client_sessions.remove(&session.client_id);
            info!("Session destroyed: {}", session_id);
        }
        Ok(())
    }

    /// Remove expired sessions.
    fn cleanup_expired(&self) -> Result<()> {
        let session_ids: Vec<String> = {
            let sessions = self.sessions.lock()?;
            sessions
                .iter()
                .filter_map(|(id, arc)| {
                    if let Ok(session) = arc.try_lock() {
                        if session.last_activity.elapsed() > SESSION_TIMEOUT {
                            Some(id.clone())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
                .collect()
        };

        for id in &session_ids {
            self.destroy_session(id);
            info!("Session expired and removed: {}", id);
        }

        if !session_ids.is_empty() {
            debug!("Cleaned up {} expired sessions", session_ids.len());
        }
        Ok(())
    }
}
