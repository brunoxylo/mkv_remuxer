use std::fmt;
use std::io;

/// Result type alias for remuxer operations
pub type Result<T> = std::result::Result<T, Error>;

/// Error types that can occur during MKV remuxing operations
#[derive(Debug)]
pub enum Error {
    /// IO error occurred during read/write operations
    Io(io::Error),

    /// mkv_element library error
    MkvElement(mkv_element::Error),

    /// Track not found with the specified number
    TrackNotFound(u64),

    /// No tracks of the requested type found (e.g., no video tracks)
    NoTracksOfType(String),

    /// Invalid or missing required element
    MissingElement(String),

    /// Invalid timestamp or seek position
    InvalidTimestamp(String),

    /// Seek operation failed
    SeekFailed {
        target_ns: u64,
        reason: String,
    },

    /// Invalid configuration
    InvalidConfig(String),

    /// Track mapping error (e.g., duplicate track numbers)
    TrackMappingError(String),

    /// Codec incompatibility or unsupported codec
    UnsupportedCodec {
        codec_id: String,
        reason: String,
    },

    /// Timecode scale mismatch or conversion error
    TimecodeScaleError(String),

    /// Block data is corrupted or invalid
    InvalidBlockData(String),

    /// End of stream reached unexpectedly
    UnexpectedEof,

    /// Remuxing has completed — returned by `Remuxer::process()` when there are no more clusters
    Done,

    /// Unknown or unsupported element size
    UnknownElementSize(String),

    /// Operation not supported in the current context
    UnsupportedOperation(String),

    /// General remuxing error
    RemuxError(String),
    ClusterIsFull(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(err) => write!(f, "IO error: {}", err),
            Error::MkvElement(err) => write!(f, "MKV element error: {}", err),
            Error::TrackNotFound(num) => write!(f, "Track #{} not found", num),
            Error::NoTracksOfType(track_type) => {
                write!(f, "No tracks of type '{}' found", track_type)
            }
            Error::MissingElement(name) => write!(f, "Missing required element: {}", name),
            Error::InvalidTimestamp(msg) => write!(f, "Invalid timestamp: {}", msg),
            Error::SeekFailed { target_ns, reason } => {
                write!(f, "Seek to {} ns failed: {}", target_ns, reason)
            }
            Error::InvalidConfig(msg) => write!(f, "Invalid configuration: {}", msg),
            Error::TrackMappingError(msg) => write!(f, "Track mapping error: {}", msg),
            Error::UnsupportedCodec { codec_id, reason } => {
                write!(f, "Unsupported codec '{}': {}", codec_id, reason)
            }
            Error::TimecodeScaleError(msg) => write!(f, "Timecode scale error: {}", msg),
            Error::InvalidBlockData(msg) => write!(f, "Invalid block data: {}", msg),
            Error::UnexpectedEof => write!(f, "Unexpected end of file"),
            Error::UnknownElementSize(element) => {
                write!(f, "Unknown element size for: {}", element)
            }
            Error::UnsupportedOperation(msg) => write!(f, "Unsupported operation: {}", msg),
            Error::RemuxError(msg) => write!(f, "Remux error: {}", msg),
            Error::ClusterIsFull(msg) => write!(f, "Cluster is full: {}", msg),
            Error::Done => write!(f, "Remuxing completed"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(err) => Some(err),
            Error::MkvElement(err) => Some(err),
            _ => None,
        }
    }
}

// Conversion from std::io::Error
impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io(err)
    }
}

// Conversion from mkv_element::Error
impl From<mkv_element::Error> for Error {
    fn from(err: mkv_element::Error) -> Self {
        Error::MkvElement(err)
    }
}

// Convenience methods for creating common errors
impl Error {
    /// Create a track not found error
    pub fn track_not_found(track_number: u64) -> Self {
        Error::TrackNotFound(track_number)
    }

    /// Create a missing element error
    pub fn missing_element(name: impl Into<String>) -> Self {
        Error::MissingElement(name.into())
    }

    /// Create an invalid timestamp error
    pub fn invalid_timestamp(msg: impl Into<String>) -> Self {
        Error::InvalidTimestamp(msg.into())
    }

    /// Create a seek failed error
    pub fn seek_failed(target_ns: u64, reason: impl Into<String>) -> Self {
        Error::SeekFailed {
            target_ns,
            reason: reason.into(),
        }
    }

    /// Create an invalid config error
    pub fn invalid_config(msg: impl Into<String>) -> Self {
        Error::InvalidConfig(msg.into())
    }

    /// Create a remux error
    pub fn remux_error(msg: impl Into<String>) -> Self {
        Error::RemuxError(msg.into())
    }
}
