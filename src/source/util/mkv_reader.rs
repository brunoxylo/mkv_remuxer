use std::fs::File;
use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};

/// Trait for a seekable reader that can be cloned and queried for length.
///
/// This abstracts over `File`, `Cursor<Vec<u8>>`, and any other `Read + Seek`
/// type so that `FileSource` and `KeyframePositionCache` are not tied to the
/// filesystem.
pub trait MkvReader: Read + Seek + Send {
    /// Create an independent clone of this reader (separate seek position).
    fn try_clone_reader(&self) -> io::Result<Box<dyn MkvReader>>;

    /// Total byte length of the underlying data stream.
    /// required for binary search in `KeyframePositionCache`.
    fn stream_length(&self) -> io::Result<u64>;
}

impl MkvReader for File {
    fn try_clone_reader(&self) -> io::Result<Box<dyn MkvReader>> {
        Ok(Box::new(self.try_clone()?))
    }

    fn stream_length(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }
}



impl MkvReader for io::Cursor<Vec<u8>> {
    fn try_clone_reader(&self) -> io::Result<Box<dyn MkvReader>> {
        let mut clone = io::Cursor::new(self.get_ref().clone());
        clone.set_position(self.position());
        Ok(Box::new(clone))
    }

    fn stream_length(&self) -> io::Result<u64> {
        Ok(self.get_ref().len() as u64)
    }
}

/// Blanket impl so `Box<dyn MkvReader>` itself satisfies `MkvReader`,
/// enabling seamless use via the struct fields.
impl MkvReader for Box<dyn MkvReader> {
    fn try_clone_reader(&self) -> io::Result<Box<dyn MkvReader>> {
        (**self).try_clone_reader()
    }

    fn stream_length(&self) -> io::Result<u64> {
        (**self).stream_length()
    }
}
