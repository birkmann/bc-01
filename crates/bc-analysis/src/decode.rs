//! Streaming decode with symphonia: native sample rate, interleaved f32 chunks, flat memory.

use std::fs::File;
use std::path::Path;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("cannot open file: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported or corrupt audio: {0}")]
    Format(String),
    #[error("no audio track")]
    NoTrack,
    #[error("no decodable audio frames")]
    Empty,
}

/// What the decoder tells the sink before the first chunk.
#[derive(Debug, Clone, Copy)]
pub struct StreamInfo {
    pub sample_rate: u32,
    pub channels: usize,
    /// Frame count from the container, if declared (VBR MP3 often lies; treat as a hint).
    pub n_frames_hint: Option<u64>,
}

/// Decode `path`, calling `sink.info` once, then `sink.data(interleaved_f32)` per packet.
/// Individual corrupt packets are skipped (MP3 resync), a file with no decodable frame at
/// all is `DecodeError::Empty`. Returns the number of frames delivered.
pub trait Sink {
    fn info(&mut self, info: StreamInfo) -> Result<(), DecodeError>;
    fn data(&mut self, interleaved: &[f32]);
}

pub fn decode_stream<S: Sink>(path: &Path, sink: &mut S) -> Result<u64, DecodeError> {
    let file = File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions { enable_gapless: true, ..Default::default() }, &MetadataOptions::default())
        .map_err(|e| DecodeError::Format(e.to_string()))?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or(DecodeError::NoTrack)?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| DecodeError::Format(e.to_string()))?;
    let n_frames_hint = track.codec_params.n_frames;

    let mut buf: Option<SampleBuffer<f32>> = None;
    let mut frames = 0u64;
    let mut informed = false;
    let mut consecutive_errors = 0u32;
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            Err(SymError::IoError(_)) => break,
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors > 50 {
                    return Err(DecodeError::Format(e.to_string()));
                }
                continue;
            }
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                consecutive_errors = 0;
                let spec = *decoded.spec();
                let ch = spec.channels.count();
                if !informed {
                    sink.info(StreamInfo { sample_rate: spec.rate, channels: ch, n_frames_hint })?;
                    informed = true;
                }
                let cap = decoded.capacity();
                if buf.as_ref().is_none_or(|b| b.capacity() < cap * ch) {
                    buf = Some(SampleBuffer::<f32>::new(cap as u64, spec));
                }
                let b = buf.as_mut().expect("buffer allocated above");
                b.copy_interleaved_ref(decoded);
                let s = b.samples();
                if !s.is_empty() {
                    frames += (s.len() / ch.max(1)) as u64;
                    sink.data(s);
                }
            }
            Err(SymError::DecodeError(_)) | Err(SymError::IoError(_)) => {
                consecutive_errors += 1;
                if consecutive_errors > 200 {
                    return Err(DecodeError::Format("too many corrupt packets".into()));
                }
            }
            Err(e) => return Err(DecodeError::Format(e.to_string())),
        }
    }
    if frames == 0 {
        return Err(DecodeError::Empty);
    }
    Ok(frames)
}
