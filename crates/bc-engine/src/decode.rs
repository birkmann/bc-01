//! Decode workers: one thread per deck decodes a file (memory-mapped) or an
//! HTTP stream (range reader with read-ahead) with symphonia, resamples to the
//! device rate with rubato and feeds fixed-size [`Chunk`]s to the deck's
//! lock-free ring. Everything allocating or blocking lives here, never in the
//! audio callback.

use bc_dsp::deck::{CHUNK_FRAMES, Chunk};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use rtrb::{Producer, PushError};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{TimeBase, Time};

pub use rubato;

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("io: {0}")]
    Io(String),
    #[error("unsupported or corrupt media: {0}")]
    Media(String),
    #[error("http: {0}")]
    Http(String),
    #[error("decoder stopped")]
    Stopped,
}

impl From<std::io::Error> for DecodeError {
    fn from(e: std::io::Error) -> Self {
        DecodeError::Io(e.to_string())
    }
}

/// Where audio is read from.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    File(PathBuf),
    Http { url: String },
}

/// What the worker tells the control side after opening a source.
#[derive(Debug, Clone, PartialEq)]
pub struct Opened {
    pub epoch: u32,
    pub src_rate: u32,
    pub channels: usize,
    /// Track length in frames at the *device* rate, 0 when unknown.
    pub len_frames: u64,
}

pub enum DecodeCmd {
    Load { epoch: u32, source: Source, start_frame: u64, reply: Sender<Result<Opened, DecodeError>> },
    Seek { epoch: u32, frame: u64 },
    Stop,
    Shutdown,
}

/// Handle to a decode worker thread.
pub struct DecodeWorker {
    tx: Sender<DecodeCmd>,
    join: Option<std::thread::JoinHandle<()>>,
    alive: Arc<AtomicBool>,
}

impl DecodeWorker {
    pub fn spawn(name: &str, sr: u32, producer: Producer<Chunk>) -> Self {
        let (tx, rx) = unbounded();
        let alive = Arc::new(AtomicBool::new(true));
        let a2 = alive.clone();
        let join = std::thread::Builder::new()
            .name(format!("bc-decode-{name}"))
            .spawn(move || {
                worker_main(sr, producer, rx);
                a2.store(false, Ordering::Relaxed);
            })
            .ok();
        Self { tx, join, alive }
    }

    /// Open `source` and start decoding at `start_frame` (device rate); the
    /// container facts arrive on the returned channel.
    pub fn load_async(&self, epoch: u32, source: Source, start_frame: u64) -> Receiver<Result<Opened, DecodeError>> {
        let (rtx, rrx) = bounded(1);
        if self.tx.send(DecodeCmd::Load { epoch, source, start_frame, reply: rtx.clone() }).is_err() {
            let _ = rtx.send(Err(DecodeError::Stopped));
        }
        rrx
    }

    /// Blocking variant of [`load_async`](Self::load_async).
    pub fn load(&self, epoch: u32, source: Source, start_frame: u64, timeout: Duration) -> Result<Opened, DecodeError> {
        let rrx = self.load_async(epoch, source, start_frame);
        match rrx.recv_timeout(timeout) {
            Ok(r) => r,
            Err(_) => Err(DecodeError::Http("timed out opening source".into())),
        }
    }

    pub fn seek(&self, epoch: u32, frame: u64) {
        let _ = self.tx.send(DecodeCmd::Seek { epoch, frame });
    }

    pub fn stop(&self) {
        let _ = self.tx.send(DecodeCmd::Stop);
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

impl Drop for DecodeWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(DecodeCmd::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

// ---------------------------------------------------------------------------
// media sources
// ---------------------------------------------------------------------------

/// A memory-mapped local file.
struct MmapSource {
    map: memmap2::Mmap,
    pos: u64,
}

impl Read for MmapSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let len = self.map.len() as u64;
        if self.pos >= len {
            return Ok(0);
        }
        let n = buf.len().min((len - self.pos) as usize);
        buf[..n].copy_from_slice(&self.map[self.pos as usize..self.pos as usize + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for MmapSource {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let len = self.map.len() as i64;
        let np = match from {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => len + d,
        };
        if np < 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = np as u64;
        Ok(self.pos)
    }
}

impl MediaSource for MmapSource {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.map.len() as u64)
    }
}

/// Block size of the HTTP range reader (about 30 s of a 128 kbps stream).
const HTTP_BLOCK: u64 = 512 * 1024;

type Block = (u64, Vec<u8>);

/// HTTP range reader with one block of read-ahead. Seekable when the server
/// answers `Range` requests; otherwise the whole body is fetched once.
pub struct HttpSource {
    client: reqwest::blocking::Client,
    url: String,
    len: u64,
    pos: u64,
    cur: Block,
    ahead: Option<std::thread::JoinHandle<Result<Block, String>>>,
    ranges: bool,
}

fn fetch_range(client: &reqwest::blocking::Client, url: &str, start: u64, len: u64) -> Result<(Block, Option<u64>, bool), String> {
    let mut last = String::new();
    for attempt in 0..3 {
        let end = start + len - 1;
        match client.get(url).header("Range", format!("bytes={start}-{end}")).send() {
            Ok(resp) => {
                let status = resp.status();
                let total = resp
                    .headers()
                    .get("content-range")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.rsplit('/').next())
                    .and_then(|v| v.parse::<u64>().ok());
                let content_len = resp.content_length();
                if status.as_u16() == 206 {
                    let body = resp.bytes().map_err(|e| e.to_string())?;
                    return Ok(((start, body.to_vec()), total, true));
                } else if status.is_success() {
                    // server ignored Range: this is the whole body
                    let body = resp.bytes().map_err(|e| e.to_string())?;
                    let total = content_len.or(Some(body.len() as u64));
                    return Ok(((0, body.to_vec()), total, false));
                } else if status.as_u16() == 416 {
                    return Ok(((start, Vec::new()), total, true));
                } else {
                    last = format!("HTTP {status}");
                }
            }
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(300 * (attempt + 1)));
    }
    Err(last)
}

impl HttpSource {
    pub fn open(url: &str) -> Result<Self, DecodeError> {
        let client = reqwest::blocking::Client::builder()
            .user_agent("Mozilla/5.0 (X11; Linux x86_64) bc-rust/0.1")
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| DecodeError::Http(e.to_string()))?;
        let (first, total, ranges) = fetch_range(&client, url, 0, HTTP_BLOCK).map_err(DecodeError::Http)?;
        let len = total.unwrap_or(first.1.len() as u64);
        let mut s = Self { client, url: url.to_string(), len, pos: 0, cur: first, ahead: None, ranges };
        s.schedule_ahead();
        Ok(s)
    }

    fn schedule_ahead(&mut self) {
        if !self.ranges || self.ahead.is_some() {
            return;
        }
        let next = self.cur.0 + self.cur.1.len() as u64;
        if next >= self.len || self.cur.1.is_empty() {
            return;
        }
        let (client, url) = (self.client.clone(), self.url.clone());
        self.ahead = std::thread::Builder::new()
            .name("bc-http-ahead".into())
            .spawn(move || fetch_range(&client, &url, next, HTTP_BLOCK).map(|(b, _, _)| b))
            .ok();
    }

    fn load_block_at(&mut self, pos: u64) -> std::io::Result<()> {
        // use the read-ahead block when it is the one we need
        if let Some(h) = self.ahead.take()
            && let Ok(Ok(b)) = h.join()
                && b.0 <= pos && pos < b.0 + b.1.len() as u64 {
                    self.cur = b;
                    self.schedule_ahead();
                    return Ok(());
                }
        let start = pos - pos % (64 * 1024);
        let (b, _, _) = fetch_range(&self.client, &self.url, start, HTTP_BLOCK)
            .map_err(|e| std::io::Error::other(format!("stream fetch failed: {e}")))?;
        self.cur = b;
        self.schedule_ahead();
        Ok(())
    }
}

impl Read for HttpSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.len {
            return Ok(0);
        }
        let (start, ref data) = self.cur;
        if !(start <= self.pos && self.pos < start + data.len() as u64) {
            let pos = self.pos;
            self.load_block_at(pos)?;
        }
        let (start, ref data) = self.cur;
        if !(start <= self.pos && self.pos < start + data.len() as u64) {
            return Ok(0);
        }
        let off = (self.pos - start) as usize;
        let n = buf.len().min(data.len() - off);
        buf[..n].copy_from_slice(&data[off..off + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for HttpSource {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let np = match from {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => self.len as i64 + d,
        };
        if np < 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = np as u64;
        Ok(self.pos)
    }
}

impl MediaSource for HttpSource {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

// ---------------------------------------------------------------------------
// pipeline
// ---------------------------------------------------------------------------

struct Pipeline {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    src_rate: u32,
    dev_rate: u32,
    chans: usize,
    resampler: Option<Fft<f32>>,
    in_chunk: usize,
    /// pending interleaved stereo input at the source rate
    in_buf: Vec<f32>,
    in_pos: usize,
    res_in: Vec<f32>,
    res_out: Vec<f32>,
    /// pending interleaved stereo output at the device rate
    out_q: Vec<f32>,
    out_pos: usize,
    /// output frames still to drop (resampler delay, seek pre-roll)
    skip_out: usize,
    /// device-rate frame index of the next frame to be emitted in a chunk
    next_frame: u64,
    /// real (decoded) input frames since the last seek, excluding flush padding
    real_in: u64,
    /// resampler output frames produced since the last seek, delay included
    raw_out: u64,
    flushed: bool,
    eof_input: bool,
    finished: bool,
    sample_scratch: Vec<f32>,
    delay: usize,
}

fn open_media(source: &Source) -> Result<(Box<dyn FormatReader>, Hint), DecodeError> {
    let mut hint = Hint::new();
    let boxed: Box<dyn MediaSource> = match source {
        Source::File(p) => {
            if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                hint.with_extension(ext);
            }
            let f = std::fs::File::open(p)?;
            match unsafe { memmap2::Mmap::map(&f) } {
                Ok(map) => {
                    // let the kernel read ahead (a cold file on a busy disk must not stall the decoder)
                    #[cfg(unix)]
                    {
                        let advice = if map.len() <= 256 << 20 { memmap2::Advice::WillNeed } else { memmap2::Advice::Sequential };
                        let _ = map.advise(advice);
                    }
                    Box::new(MmapSource { map, pos: 0 })
                }
                Err(_) => Box::new(f),
            }
        }
        Source::Http { url } => {
            if let Some(ext) = Path::new(url.split('?').next().unwrap_or(url)).extension().and_then(|e| e.to_str())
                && ext.len() <= 4 {
                    hint.with_extension(ext);
                }
            Box::new(HttpSource::open(url)?)
        }
    };
    let mss = MediaSourceStream::new(boxed, MediaSourceStreamOptions::default());
    let format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| DecodeError::Media(e.to_string()))?;
    Ok((format, hint))
}

impl Pipeline {
    fn open(source: &Source, dev_rate: u32, start_frame: u64) -> Result<(Self, Opened), DecodeError> {
        let (format, _) = open_media(source)?;
        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| DecodeError::Media("no audio track".into()))?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| DecodeError::Media("no audio codec parameters".into()))?;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(|e| DecodeError::Media(e.to_string()))?;
        let src_rate = params.sample_rate.unwrap_or(44_100);
        let chans = params.channels.as_ref().map(|c| c.count()).unwrap_or(2).max(1);
        let track_id = track.id;
        let time_base = track.time_base;
        let num_frames = track.num_frames;
        let len_src = num_frames.or_else(|| {
            let (tb, d) = (track.time_base?, track.duration?);
            let t = tb.calc_duration(d)?;
            Some((t.as_secs_f64() * src_rate as f64).round() as u64)
        });
        let len_frames = len_src.map(|n| (n as f64 * dev_rate as f64 / src_rate as f64).round() as u64).unwrap_or(0);

        let (resampler, delay) = if src_rate != dev_rate {
            let r = Fft::<f32>::new(src_rate as usize, dev_rate as usize, 1024, 2, FixedSync::Input)
                .map_err(|e| DecodeError::Media(format!("resampler: {e}")))?;
            let d = r.output_delay();
            (Some(r), d)
        } else {
            (None, 0)
        };
        let in_chunk = resampler.as_ref().map(|r| r.input_frames_next()).unwrap_or(0);
        let res_out_cap = resampler.as_ref().map(|r| r.output_frames_max()).unwrap_or(0);
        let mut p = Pipeline {
            format,
            decoder,
            track_id,
            time_base,
            src_rate,
            dev_rate,
            chans,
            resampler,
            in_chunk,
            in_buf: Vec::with_capacity(1 << 16),
            in_pos: 0,
            res_in: vec![0.0; in_chunk.max(1) * 2],
            res_out: vec![0.0; res_out_cap.max(1) * 2],
            out_q: Vec::with_capacity(1 << 15),
            out_pos: 0,
            skip_out: delay,
            next_frame: start_frame,
            real_in: 0,
            raw_out: 0,
            flushed: false,
            eof_input: false,
            finished: false,
            sample_scratch: Vec::new(),
            delay,
        };
        p.seek_to(start_frame)?;
        let opened = Opened { epoch: 0, src_rate, channels: chans, len_frames };
        Ok((p, opened))
    }

    /// Position the stream so the first emitted frame is `frame` (device rate).
    fn seek_to(&mut self, frame: u64) -> Result<(), DecodeError> {
        self.in_buf.clear();
        self.in_pos = 0;
        self.out_q.clear();
        self.out_pos = 0;
        self.eof_input = false;
        self.finished = false;
        self.real_in = 0;
        self.raw_out = 0;
        self.flushed = false;
        if let Some(r) = self.resampler.as_mut() {
            r.reset();
        }
        self.skip_out = self.delay;
        self.next_frame = frame;
        if frame == 0 {
            // start of file: plain rewind (some containers cannot seek to 0 cheaply)
            let _ = self.format.seek(
                SeekMode::Accurate,
                SeekTo::Time { time: Time::ZERO, track_id: Some(self.track_id) },
            );
            self.decoder.reset();
            return Ok(());
        }
        let secs = frame as f64 / self.dev_rate as f64;
        let time = Time::try_from_secs_f64(secs).unwrap_or(Time::ZERO);
        let seeked = self
            .format
            .seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(self.track_id) })
            .map_err(|e| DecodeError::Media(format!("seek: {e}")))?;
        self.decoder.reset();
        // where did the container actually land (device-rate frames)?
        let actual_s = self
            .time_base
            .and_then(|tb| tb.calc_time(seeked.actual_ts))
            .map(|t| t.as_secs_f64())
            .unwrap_or(secs);
        let actual_dev = (actual_s * self.dev_rate as f64).round() as i64;
        let preroll = (frame as i64 - actual_dev).max(0) as usize;
        self.skip_out += preroll;
        if actual_dev > frame as i64 {
            self.next_frame = actual_dev as u64;
        }
        Ok(())
    }

    /// Read and decode one packet into `in_buf` (stereo interleaved, source rate).
    fn decode_one(&mut self) -> Result<(), DecodeError> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => {
                    self.eof_input = true;
                    return Ok(());
                }
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    self.eof_input = true;
                    return Ok(());
                }
                Err(SymError::ResetRequired) => {
                    self.eof_input = true;
                    return Ok(());
                }
                Err(e) => return Err(DecodeError::Media(e.to_string())),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(buf) => {
                    let before = self.in_buf.len();
                    append_stereo(&buf, self.chans, &mut self.sample_scratch, &mut self.in_buf);
                    self.real_in += ((self.in_buf.len() - before) / 2) as u64;
                    return Ok(());
                }
                Err(SymError::DecodeError(_)) => continue,
                Err(SymError::IoError(_)) => continue,
                Err(e) => return Err(DecodeError::Media(e.to_string())),
            }
        }
    }

    fn out_frames(&self) -> usize {
        (self.out_q.len() - self.out_pos) / 2
    }

    fn push_out(&mut self, data: &[f32]) {
        let mut data = data;
        if self.skip_out > 0 {
            let drop = self.skip_out.min(data.len() / 2);
            self.skip_out -= drop;
            data = &data[drop * 2..];
        }
        if self.out_pos > (1 << 15) && self.out_pos * 2 > self.out_q.len() {
            self.out_q.drain(..self.out_pos);
            self.out_pos = 0;
        }
        self.out_q.extend_from_slice(data);
    }

    /// Run the resampler on whatever input is pending (full chunks only unless `flush`).
    fn resample(&mut self, flush: bool) -> Result<(), DecodeError> {
        if self.resampler.is_none() {
            // same rate: pass through
            let frames = (self.in_buf.len() - self.in_pos) / 2;
            if frames > 0 {
                let slice: Vec<f32> = self.in_buf[self.in_pos..].to_vec();
                self.in_buf.clear();
                self.in_pos = 0;
                self.push_out(&slice);
            }
            return Ok(());
        }
        loop {
            let avail = (self.in_buf.len() - self.in_pos) / 2;
            let need = self.in_chunk;
            let partial = if avail >= need {
                None
            } else if flush && avail > 0 {
                Some(avail)
            } else {
                break;
            };
            let take = partial.unwrap_or(need);
            self.res_in[..take * 2].copy_from_slice(&self.in_buf[self.in_pos..self.in_pos + take * 2]);
            self.res_in[take * 2..need * 2].iter_mut().for_each(|s| *s = 0.0);
            self.in_pos += take * 2;
            self.run_resampler(need, partial)?;
            if partial.is_some() {
                break;
            }
        }
        if self.in_pos > 0 {
            self.in_buf.drain(..self.in_pos);
            self.in_pos = 0;
        }
        Ok(())
    }

    /// One resampler call over `res_in` (`need` frames, `partial` of them real).
    fn run_resampler(&mut self, need: usize, partial: Option<usize>) -> Result<(), DecodeError> {
        let Some(r) = self.resampler.as_mut() else { return Ok(()) };
        let out_cap = r.output_frames_max();
        let input = InterleavedSlice::new(&self.res_in, 2, need).map_err(|e| DecodeError::Media(e.to_string()))?;
        let mut output =
            InterleavedSlice::new_mut(&mut self.res_out, 2, out_cap).map_err(|e| DecodeError::Media(e.to_string()))?;
        let idx = Indexing { input_offset: 0, output_offset: 0, partial_len: partial, active_channels_mask: None };
        let (_, written) =
            r.process_into_buffer(&input, &mut output, Some(&idx)).map_err(|e| DecodeError::Media(e.to_string()))?;
        self.raw_out += written as u64;
        let out = self.res_out[..written * 2].to_vec();
        self.push_out(&out);
        Ok(())
    }

    /// End of input: push the remaining input and silence through the
    /// resampler until its delayed tail is out, then trim to the exact length.
    fn flush_tail(&mut self) -> Result<(), DecodeError> {
        if self.flushed {
            return Ok(());
        }
        self.flushed = true;
        if self.resampler.is_none() {
            return Ok(());
        }
        let ratio = self.dev_rate as f64 / self.src_rate as f64;
        self.resample(true)?;
        let expected_raw = self.delay as u64 + (self.real_in as f64 * ratio).round() as u64;
        let need = self.in_chunk;
        let mut guard = 0;
        while self.raw_out < expected_raw && guard < 32 {
            self.res_in[..need * 2].iter_mut().for_each(|s| *s = 0.0);
            self.run_resampler(need, Some(0))?;
            guard += 1;
        }
        if self.raw_out > expected_raw {
            let excess = ((self.raw_out - expected_raw) as usize).min(self.out_frames());
            let keep = self.out_q.len() - excess * 2;
            self.out_q.truncate(keep);
        }
        Ok(())
    }

    /// Produce the next chunk, or `None` once the track has been fully emitted.
    fn next_chunk(&mut self, epoch: u32) -> Result<Option<Box<Chunk>>, DecodeError> {
        if self.finished {
            return Ok(None);
        }
        while self.out_frames() < CHUNK_FRAMES && !self.eof_input {
            self.decode_one()?;
            self.resample(false)?;
        }
        let mut last = false;
        if self.eof_input && self.out_frames() < CHUNK_FRAMES {
            self.flush_tail()?;
            last = true;
        }
        let have = self.out_frames();
        let take = have.min(CHUNK_FRAMES);
        let mut chunk = Box::new(Chunk::empty());
        chunk.epoch = epoch;
        chunk.start_frame = self.next_frame;
        chunk.frames = take as u32;
        chunk.data[..take * 2].copy_from_slice(&self.out_q[self.out_pos..self.out_pos + take * 2]);
        self.out_pos += take * 2;
        self.next_frame += take as u64;
        if last && self.out_frames() == 0 {
            chunk.last = true;
            self.finished = true;
        }
        Ok(Some(chunk))
    }
}

/// Convert any decoded buffer to stereo f32 appended to `dst`.
fn append_stereo(buf: &GenericAudioBufferRef<'_>, chans: usize, scratch: &mut Vec<f32>, dst: &mut Vec<f32>) {
    let n = buf.samples_interleaved();
    let frames = buf.frames();
    if frames == 0 {
        return;
    }
    scratch.resize(n, 0.0);
    buf.copy_to_slice_interleaved(scratch.as_mut_slice());
    let ch = (n / frames).max(1);
    let _ = chans;
    match ch {
        1 => {
            for &v in &scratch[..frames] {
                dst.push(v);
                dst.push(v);
            }
        }
        2 => dst.extend_from_slice(&scratch[..frames * 2]),
        c if c >= 6 => {
            // 5.1: L R C LFE Ls Rs
            for i in 0..frames {
                let f = &scratch[i * c..i * c + c];
                dst.push(0.5 * (f[0] + 0.707 * f[2] + 0.707 * f[4]));
                dst.push(0.5 * (f[1] + 0.707 * f[2] + 0.707 * f[5]));
            }
        }
        c => {
            for i in 0..frames {
                dst.push(scratch[i * c]);
                dst.push(scratch[i * c + 1]);
            }
        }
    }
}

fn worker_main(sr: u32, mut producer: Producer<Chunk>, rx: Receiver<DecodeCmd>) {
    let mut cur: Option<(Pipeline, u32)> = None;
    let mut pending: Option<Box<Chunk>> = None;
    loop {
        let cmd = if cur.is_none() && pending.is_none() {
            match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            }
        } else {
            rx.try_recv().ok()
        };
        if let Some(cmd) = cmd {
            match cmd {
                DecodeCmd::Shutdown => return,
                DecodeCmd::Stop => {
                    cur = None;
                    pending = None;
                }
                DecodeCmd::Load { epoch, source, start_frame, reply } => {
                    pending = None;
                    cur = None;
                    match Pipeline::open(&source, sr, start_frame) {
                        Ok((p, mut o)) => {
                            o.epoch = epoch;
                            cur = Some((p, epoch));
                            let _ = reply.send(Ok(o));
                        }
                        Err(e) => {
                            tracing::warn!("decode open failed: {e}");
                            let _ = reply.send(Err(e));
                        }
                    }
                }
                DecodeCmd::Seek { epoch, frame } => {
                    pending = None;
                    if let Some((p, e)) = cur.as_mut() {
                        match p.seek_to(frame) {
                            Ok(()) => *e = epoch,
                            Err(err) => {
                                tracing::warn!("decode seek failed: {err}");
                                cur = None;
                            }
                        }
                    }
                }
            }
            continue;
        }
        if let Some(c) = pending.take() {
            match producer.push(*c) {
                Ok(()) => {}
                Err(PushError::Full(c)) => {
                    pending = Some(Box::new(c));
                    std::thread::sleep(Duration::from_millis(3));
                }
            }
            continue;
        }
        if let Some((p, epoch)) = cur.as_mut() {
            match p.next_chunk(*epoch) {
                Ok(Some(c)) => pending = Some(c),
                Ok(None) => cur = None,
                Err(e) => {
                    tracing::warn!("decode error: {e}");
                    // end the track at the failure point so playback moves on
                    let mut c = Box::new(Chunk::empty());
                    c.epoch = *epoch;
                    c.start_frame = p.next_frame;
                    c.last = true;
                    pending = Some(c);
                    cur = None;
                }
            }
        }
    }
}

/// Decode `[start_s, end_s)` of a file or stream fully into memory as
/// interleaved stereo at `sr` (offline rendering, analysis helpers). Returns
/// the PCM and the track-timeline frame of its first sample.
pub fn decode_range(source: &Source, sr: u32, start_s: f64, end_s: Option<f64>) -> Result<(Vec<f32>, u64), DecodeError> {
    let start_frame = (start_s.max(0.0) * sr as f64).round() as u64;
    let (mut p, _) = Pipeline::open(source, sr, start_frame)?;
    let end_frame = end_s.map(|e| (e * sr as f64).round() as u64);
    let mut out: Vec<f32> = Vec::new();
    while let Some(c) = p.next_chunk(0)? {
        let frames = c.frames as usize;
        let at = c.start_frame;
        let mut take = frames;
        if let Some(e) = end_frame {
            if at >= e {
                break;
            }
            take = take.min((e - at) as usize);
        }
        out.extend_from_slice(&c.data[..take * 2]);
        if c.last || take < frames {
            break;
        }
    }
    Ok((out, start_frame))
}
