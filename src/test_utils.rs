use crate::source::FileSource;
use crate::source::InputSource;
use crate::source::Uninitialized;
use std::path::{Path, PathBuf};

pub fn test_file_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("test.webm")
}

pub fn sources_implementations() -> Vec<InputSource<Uninitialized>> {
    vec![
        FileSource::new(test_file_path()).unwrap().into(),
        // Add other Source implementations here as needed
    ]
}
