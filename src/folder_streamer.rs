use crate::{
    ContainerFormat, Error, MkvBasicInfo, Remuxer, RemuxerCutMode, RemuxerState, Result,
    sink::{
        ChannelWriterWrapper, OutputSink, Sink, SinkSender, StreamSink, Uninitialized, VttSink,
    },
    source::{CutInterval, FileSource, InputSource, Source, WebVttSource},
};
use bytes::Bytes;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use tokio::sync::mpsc;

pub struct FolderStreamer {
    pub root_dir: PathBuf,
}

impl FolderStreamer {
    pub fn new(root_dir: PathBuf) -> Self {
        Self { root_dir }
    }

    fn scan_media_files(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = match std::fs::read_dir(&self.root_dir) {
            Ok(read_dir) => read_dir
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && matches!(
                            p.extension()
                                .and_then(std::ffi::OsStr::to_str)
                                .unwrap_or(""),
                            "mkv" | "webm" | "vtt"
                        )
                })
                .collect(),
            Err(_) => vec![],
        };
        paths.sort();
        paths
    }

    pub fn list_files(&self) -> Vec<MkvBasicInfo> {
        self.scan_media_files()
            .into_iter()
            .filter_map(|path| {
                let is_vtt = path
                    .extension()
                    .and_then(std::ffi::OsStr::to_str)
                    .unwrap_or("")
                    == "vtt";
                match std::fs::File::open(&path) {
                    Ok(file) => {
                        let file_name = path
                            .file_name()
                            .and_then(std::ffi::OsStr::to_str)
                            .unwrap_or("unknown")
                            .to_string();
                        let file_stem = path
                            .file_stem()
                            .and_then(std::ffi::OsStr::to_str)
                            .unwrap_or("unknown")
                            .to_string();
                        let result: Result<Box<dyn Source>> = if is_vtt {
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
                                log::warn!("Failed to probe {:?}: {}", path, e);
                                None
                            }
                        }
                    }
                    Err(_) => None,
                }
            })
            .collect()
    }

    /// Starts the streaming process with remuxing.
    ///
    /// **Warning:** This method is blocking. You need to wrap it inside a thread
    /// (e.g., using `tokio::task::spawn_blocking`) when running in an asynchronous context.
    ///
    /// # Returns
    ///
    /// A `Result` containing a tuple with:
    /// - `ContainerFormat`
    /// - `start_sec: f64`
    /// - `end_sec: f64`
    pub fn start_remuxing_stream(
        &self,
        mappings_str: String,
        start_sec: f64,
        end_sec: Option<f64>,
        cut_mode: Option<RemuxerCutMode>,
        vtt_output: bool,
        tx: mpsc::Sender<Bytes>,
        ready_tx: tokio::sync::oneshot::Sender<Result<(ContainerFormat, f64, f64)>>,
    ) {
        let all_files = self.scan_media_files();

        let init_result = Self::initialize_remuxer(
            all_files,
            mappings_str,
            start_sec,
            end_sec,
            cut_mode,
            vtt_output,
            tx,
        );

        let (mut remuxer, output_interval) = match init_result {
            Ok(r) => r,
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };

        let output_format = remuxer.get_output_container_format();
        let start_sec_out = output_interval
            .start_ns
            .map(|ns| ns as f64 / 1e9)
            .unwrap_or(0.0);
        let end_sec_out = output_interval
            .end_ns
            .map(|ns| ns as f64 / 1e9)
            .unwrap_or(0.0);

        // Notify caller that headers are ready
        if ready_tx
            .send(Ok((output_format, start_sec_out, end_sec_out)))
            .is_err()
        {
            return; // Caller dropped receiver
        }

        loop {
            match remuxer.process() {
                Ok(state) => match state {
                    RemuxerState::Processing(r) => {
                        remuxer = r;
                    }
                    RemuxerState::Done(_) => break,
                },
                Err(e) => {
                    log::trace!("Remuxing loop error: {}", e);
                    break;
                }
            }
        }
    }

    fn initialize_remuxer(
        all_files: Vec<PathBuf>,
        mappings_str: String,
        start_sec: f64,
        end_sec: Option<f64>,
        cut_mode: Option<RemuxerCutMode>,
        vtt_output: bool,
        tx: mpsc::Sender<Bytes>,
    ) -> Result<(Remuxer, CutInterval)> {
        let user_mappings: Vec<(usize, u32)> = mappings_str
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| {
                let parts: Vec<&str> = s.split('_').collect();
                if parts.len() != 2 {
                    return Err(Error::InvalidConfig(format!("Invalid mapping: {}", s)));
                }
                let f_idx = parts[0]
                    .parse::<usize>()
                    .map_err(|_| Error::InvalidConfig("Invalid file index".into()))?;
                let t_id = parts[1]
                    .parse::<u32>()
                    .map_err(|_| Error::InvalidConfig("Invalid track id".into()))?;
                Ok((f_idx, t_id))
            })
            .collect::<Result<Vec<_>>>()?;

        if user_mappings.is_empty() {
            return Err(Error::InvalidConfig("No mappings provided".into()));
        }

        let mut unique_file_indices: Vec<usize> = user_mappings.iter().map(|(f, _)| *f).collect();
        unique_file_indices.sort();
        unique_file_indices.dedup();

        let mut user_to_remuxer_map: HashMap<usize, usize> = HashMap::new();
        let mut sources: Vec<InputSource> = Vec::new();

        for (remuxer_idx, user_idx) in unique_file_indices.iter().enumerate() {
            if *user_idx >= all_files.len() {
                return Err(Error::InvalidConfig(format!(
                    "File index {} out of bounds",
                    user_idx
                )));
            }
            let path = &all_files[*user_idx];
            let is_vtt = path
                .extension()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or("")
                == "vtt";
            let input_source = if is_vtt {
                let file = std::fs::File::open(path)?;
                let file_name = path
                    .file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .unwrap_or("unknown");
                let file_stem = path
                    .file_stem()
                    .and_then(std::ffi::OsStr::to_str)
                    .unwrap_or("unknown");
                let src = WebVttSource::new(file, file_stem.to_string(), false)?
                    .with_file_name(file_name.to_string());
                InputSource::from(src)
            } else {
                let file = std::fs::File::open(path)?;
                let file_name = path
                    .file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .unwrap_or("unknown");
                let src = FileSource::new(file)?.with_file_name(file_name.to_string());
                InputSource::from(src)
            };

            sources.push(input_source);
            user_to_remuxer_map.insert(*user_idx, remuxer_idx);
        }

        let remuxer_mappings: Vec<(u64, u64)> = user_mappings
            .iter()
            .map(|(u_idx, t_id)| {
                let r_idx = user_to_remuxer_map.get(u_idx).unwrap();
                (*r_idx as u64, *t_id as u64)
            })
            .collect();

        let writer = ChannelWriterWrapper::new(SinkSender::Tokio(tx));
        let output_sink: OutputSink<Uninitialized> = if vtt_output {
            let vtt_sink = VttSink::new(writer);
            OutputSink::new(Box::new(vtt_sink) as Box<dyn Sink>)
        } else {
            let stream_sink = StreamSink::new(writer)?;
            OutputSink::new(Box::new(stream_sink) as Box<dyn Sink>)
        };

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

        Remuxer::new(
            sources,
            output_sink,
            cut_interval,
            cut_mode,
            Some(remuxer_mappings),
        )
    }

    /// Starts the streaming process without remuxing (just stream the file as is).
    ///
    /// **Warning:** This method is blocking. You need to wrap it inside a thread
    /// (e.g., using `tokio::task::spawn_blocking`) when running in an asynchronous context.
    ///
    /// # Returns
    ///
    /// A `Result` containing the `file_size` as a `u64`.
    pub fn start_direct_stream(
        &self,
        file_index: usize,
        start_byte: u64,
        end_byte: Option<u64>,
        tx: mpsc::Sender<Bytes>,
        ready_tx: tokio::sync::oneshot::Sender<Result<u64>>,
    ) {
        let all_files = self.scan_media_files();
        if file_index >= all_files.len() {
            let _ = ready_tx.send(Err(Error::InvalidConfig("File index out of bounds".into())));
            return;
        }
        let path = all_files[file_index].clone();

        let run = || -> Result<u64> {
            let mut file = std::fs::File::open(&path)?;
            let file_size = file.metadata()?.len();

            let end = end_byte.unwrap_or(file_size.saturating_sub(1));
            let end = std::cmp::min(end, file_size.saturating_sub(1));

            if start_byte > end || start_byte >= file_size {
                return Err(Error::InvalidConfig("Invalid byte range".into()));
            }

            file.seek(SeekFrom::Start(start_byte))?;
            Ok(file_size)
        };

        let file_size = match run() {
            Ok(size) => size,
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };

        if ready_tx.send(Ok(file_size)).is_err() {
            return;
        }

        // Processing loop
        let _ = (|| -> Result<()> {
            let mut file = std::fs::File::open(&path)?;
            file.seek(SeekFrom::Start(start_byte))?;

            let end = std::cmp::min(
                end_byte.unwrap_or(file_size.saturating_sub(1)),
                file_size.saturating_sub(1),
            );
            let mut remaining = end - start_byte + 1;
            let mut buffer = [0u8; 64 * 1024];

            while remaining > 0 {
                let to_read = std::cmp::min(remaining as usize, buffer.len());
                let n = file.read(&mut buffer[..to_read])?;
                if n == 0 {
                    break;
                }

                let chunk = Bytes::copy_from_slice(&buffer[..n]);
                if tx.blocking_send(chunk).is_err() {
                    break;
                }
                remaining -= n as u64;
            }
            Ok(())
        })();
    }
}
