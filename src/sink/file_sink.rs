use crate::Result;
use super::Sink;
use mkv_element::prelude::*;
use mkv_element::io::blocking_impl::*;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

/// File-based sink implementation for writing MKV files (legacy trait implementation)
pub struct FileSink {
    writer: BufWriter<File>,
    segment_started: bool,
}

impl FileSink {
    /// Create a new file sink that writes to the specified path
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = File::create(path)?;
        let writer = BufWriter::new(file);
        Ok(Self {
            writer,
            segment_started: false,
        })
    }
}

impl Sink for FileSink {
    fn initialize(&mut self, tracks: &Tracks, info: &Info, chapters: Option<&Chapters>) -> Result<()> {
        // Write EBML header
        let ebml_header = Ebml {
            ebml_version: Some(EbmlVersion(1)),
            ebml_read_version: Some(EbmlReadVersion(1)),
            ebml_max_id_length: EbmlMaxIdLength(4),
            ebml_max_size_length: EbmlMaxSizeLength(8),
            doc_type: Some(DocType("matroska".to_string())),
            doc_type_version: Some(DocTypeVersion(4)),
            doc_type_read_version: Some(DocTypeReadVersion(2)),
            crc32: None,
            void: None,
        };
        ebml_header.write_to(&mut self.writer)?;
        
        // Write Segment start with unknown size for streaming
        // Segment ID is 0x18538067
        self.writer.write_all(&[0x18, 0x53, 0x80, 0x67])?;
        // Unknown size marker (all 1s in VINT encoding)
        self.writer.write_all(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])?;
        
        // Write Info element inside the segment
        info.write_to(&mut self.writer)?;
        
        // Write Tracks element inside the segment
        tracks.write_to(&mut self.writer)?;
        
        // Write Chapters element inside the segment (if present)
        if let Some(chapters) = chapters {
            chapters.write_to(&mut self.writer)?;
        }
    
        self.writer.flush()?;
        self.segment_started = true;
        Ok(())
    }
    
    fn write_cluster(&mut self, cluster: &Cluster, _track_number: u64) -> Result<()> {
        if !self.segment_started {
            return Err(crate::Error::InvalidConfig(
                "Cannot write cluster before initialize() is called".to_string()
            ));
        }
        
        cluster.write_to(&mut self.writer)?;
        self.writer.flush()?;
        Ok(())
    }
    
    fn finalize(&mut self) -> Result<()> {
        self.writer.flush()?;
        Ok(())
    }
}
