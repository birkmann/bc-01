//! `.bcw2` container: header, planar level blocks, encode/decode. See
//! `docs/api/waveform-format.md`.

use bc_types::analysis::WaveformMeta;

/// Container magic.
pub const MAGIC: [u8; 4] = *b"BCW2";
/// Container version written by this crate.
pub const VERSION: u16 = 3;
/// Header size in bytes.
pub const HEADER_LEN: usize = 64;
/// Number of overview points.
pub const OVERVIEW_POINTS: usize = 2048;
/// Detail points per second at 44.1 kHz (hop 256).
pub const BASE_HOP: u32 = 256;

pub const FLAG_OVERVIEW: u16 = 1;
pub const FLAG_DETAIL: u16 = 2;
pub const FLAG_RAW: u16 = 4;

/// Number of planes per point.
pub const PLANES: usize = 6;
/// Plane indices.
pub const PEAK_POS: usize = 0;
pub const PEAK_NEG: usize = 1;
pub const RMS: usize = 2;
pub const LOW: usize = 3;
pub const MID: usize = 4;
pub const HIGH: usize = 5;

#[derive(Debug, thiserror::Error)]
pub enum WaveformError {
    #[error("not a BCW2 container (bad magic or too short)")]
    BadMagic,
    #[error("unsupported BCW2 version {0}")]
    UnsupportedVersion(u16),
    #[error("truncated BCW2 container: need {need} bytes, have {have}")]
    Truncated { need: usize, have: usize },
    #[error("corrupt BCW2 container: {0}")]
    Corrupt(&'static str),
    #[error("block is zstd-compressed but the `zstd` feature is not enabled")]
    ZstdUnavailable,
    #[error("zstd decode failed: {0}")]
    Zstd(String),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
}

/// `hop_samples = round(sample_rate * 256 / 44100)`, at least 1.
pub fn hop_for_rate(sample_rate: u32) -> u32 {
    (((sample_rate as u64 * BASE_HOP as u64) + 22_050) / 44_100).max(1) as u32
}

/// Alias used by the published bars API.
pub type Level = Levels;

/// One level: `n` points in six planar byte vectors.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Levels {
    pub n: usize,
    /// peak_pos, peak_neg, rms, low, mid, high.
    pub planes: [Vec<u8>; PLANES],
}

impl Levels {
    pub fn zeros(n: usize) -> Self {
        Self {
            n,
            planes: std::array::from_fn(|_| vec![0; n]),
        }
    }
    pub fn empty() -> Self {
        Self::default()
    }
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }
    pub fn point(&self, i: usize) -> [u8; PLANES] {
        std::array::from_fn(|p| self.planes[p][i])
    }
    pub fn set_point(&mut self, i: usize, v: [u8; PLANES]) {
        for (p, x) in v.iter().enumerate() {
            self.planes[p][i] = *x;
        }
    }
    pub fn peak_pos(&self) -> &[u8] {
        &self.planes[PEAK_POS]
    }
    pub fn peak_neg(&self) -> &[u8] {
        &self.planes[PEAK_NEG]
    }
    pub fn rms(&self) -> &[u8] {
        &self.planes[RMS]
    }
    pub fn low(&self) -> &[u8] {
        &self.planes[LOW]
    }
    pub fn mid(&self) -> &[u8] {
        &self.planes[MID]
    }
    pub fn high(&self) -> &[u8] {
        &self.planes[HIGH]
    }
    /// Max over all peak planes (loudest point), for render-time normalisation.
    pub fn max_peak(&self) -> u8 {
        let a = self.planes[PEAK_POS].iter().copied().max().unwrap_or(0);
        let b = self.planes[PEAK_NEG].iter().copied().max().unwrap_or(0);
        a.max(b)
    }
    /// Raw planar block (6 * n bytes).
    pub fn to_raw(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.n * PLANES);
        for p in &self.planes {
            out.extend_from_slice(p);
        }
        out
    }
    pub fn from_raw(raw: &[u8], n: usize) -> Result<Self, WaveformError> {
        if raw.len() != n * PLANES {
            return Err(WaveformError::Corrupt(
                "block size does not match point count",
            ));
        }
        Ok(Self {
            n,
            planes: std::array::from_fn(|p| raw[p * n..(p + 1) * n].to_vec()),
        })
    }
    /// Overview resampling into `out_n` points: peak planes are max-pooled, the RMS and band
    /// planes use **energy-mean pooling** (`sqrt(mean(lin^2))` in the linear domain), so loud
    /// transients (kicks) do not hold every bucket at the ceiling and breakdowns stay visible.
    /// With fewer points than `out_n` the nearest point is repeated. Empty input gives zeros.
    pub fn resample_overview(&self, out_n: usize) -> Levels {
        let mut out = Levels::zeros(out_n);
        if self.n == 0 || out_n == 0 {
            return out;
        }
        for p in 0..PLANES {
            let src = &self.planes[p];
            let energy = p >= RMS;
            for j in 0..out_n {
                out.planes[p][j] = if self.n >= out_n {
                    let a = j * self.n / out_n;
                    let b = ((j + 1) * self.n / out_n).max(a + 1);
                    if energy {
                        crate::scale::energy_mean_u8(src[a..b].iter().copied())
                    } else {
                        src[a..b].iter().copied().max().unwrap_or(0)
                    }
                } else {
                    src[((2 * j + 1) * self.n / (2 * out_n)).min(self.n - 1)]
                };
            }
        }
        out
    }

    /// Max-pool every plane into `out_n` points: bucket `j` covers
    /// `[j*n/out_n, (j+1)*n/out_n)`; with fewer points than `out_n` the nearest point is
    /// repeated. An empty input gives zeros.
    pub fn resample_max(&self, out_n: usize) -> Levels {
        let mut out = Levels::zeros(out_n);
        if self.n == 0 || out_n == 0 {
            return out;
        }
        for p in 0..PLANES {
            let src = &self.planes[p];
            for j in 0..out_n {
                out.planes[p][j] = if self.n >= out_n {
                    let a = j * self.n / out_n;
                    let b = ((j + 1) * self.n / out_n).max(a + 1);
                    src[a..b].iter().copied().max().unwrap_or(0)
                } else {
                    src[((2 * j + 1) * self.n / (2 * out_n)).min(self.n - 1)]
                };
            }
        }
        out
    }
}

/// Parsed 64-byte header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub version: u16,
    pub flags: u16,
    pub sample_rate: u32,
    pub hop_samples: u32,
    pub total_samples: u64,
    pub source_hash: [u8; 16],
    pub overview_points: u32,
    pub detail_points: u32,
    pub overview_len: u32,
    pub detail_len: u32,
}

impl Header {
    pub fn has_overview(&self) -> bool {
        self.flags & FLAG_OVERVIEW != 0
    }
    pub fn has_detail(&self) -> bool {
        self.flags & FLAG_DETAIL != 0
    }
    pub fn is_raw(&self) -> bool {
        self.flags & FLAG_RAW != 0
    }
    /// Total container length implied by the header.
    pub fn total_len(&self) -> usize {
        HEADER_LEN + self.overview_len as usize + self.detail_len as usize
    }
    pub fn source_hash_hex(&self) -> String {
        hex(&self.source_hash)
    }
    fn write(&self) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[0..4].copy_from_slice(&MAGIC);
        h[4..6].copy_from_slice(&self.version.to_le_bytes());
        h[6..8].copy_from_slice(&self.flags.to_le_bytes());
        h[8..12].copy_from_slice(&self.sample_rate.to_le_bytes());
        h[12..16].copy_from_slice(&self.hop_samples.to_le_bytes());
        h[16..24].copy_from_slice(&self.total_samples.to_le_bytes());
        h[24..40].copy_from_slice(&self.source_hash);
        h[40..44].copy_from_slice(&self.overview_points.to_le_bytes());
        h[44..48].copy_from_slice(&self.detail_points.to_le_bytes());
        h[48..52].copy_from_slice(&self.overview_len.to_le_bytes());
        h[52..56].copy_from_slice(&self.detail_len.to_le_bytes());
        h
    }
}

pub fn hex(b: &[u8]) -> String {
    use std::fmt::Write;
    b.iter()
        .fold(String::with_capacity(b.len() * 2), |mut s, x| {
            let _ = write!(s, "{x:02x}");
            s
        })
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

/// Cheap header parse and validation (no decompression, body length not checked).
pub fn read_header(bytes: &[u8]) -> Result<Header, WaveformError> {
    if bytes.len() < HEADER_LEN {
        return Err(if bytes.len() >= 4 && bytes[0..4] == MAGIC {
            WaveformError::Truncated {
                need: HEADER_LEN,
                have: bytes.len(),
            }
        } else {
            WaveformError::BadMagic
        });
    }
    if bytes[0..4] != MAGIC {
        return Err(WaveformError::BadMagic);
    }
    let version = u16_at(bytes, 4);
    if version != VERSION {
        return Err(WaveformError::UnsupportedVersion(version));
    }
    let mut source_hash = [0u8; 16];
    source_hash.copy_from_slice(&bytes[24..40]);
    let h = Header {
        version,
        flags: u16_at(bytes, 6),
        sample_rate: u32_at(bytes, 8),
        hop_samples: u32_at(bytes, 12),
        total_samples: u64_at(bytes, 16),
        source_hash,
        overview_points: u32_at(bytes, 40),
        detail_points: u32_at(bytes, 44),
        overview_len: u32_at(bytes, 48),
        detail_len: u32_at(bytes, 52),
    };
    if h.sample_rate == 0 || h.hop_samples == 0 {
        return Err(WaveformError::Corrupt("zero sample rate or hop"));
    }
    if !h.has_overview() && h.overview_len != 0 || !h.has_detail() && h.detail_len != 0 {
        return Err(WaveformError::Corrupt("block length without presence flag"));
    }
    if h.has_overview() && h.overview_len == 0 && h.overview_points != 0 && h.is_raw() {
        return Err(WaveformError::Corrupt("overview flagged but empty"));
    }
    Ok(h)
}

/// Decode one block of `points` points. `raw` selects the framing.
pub fn decode_block(data: &[u8], points: usize, raw: bool) -> Result<Levels, WaveformError> {
    if raw {
        return Levels::from_raw(data, points);
    }
    #[cfg(all(feature = "zstd", not(target_arch = "wasm32")))]
    {
        let dec = zstd::bulk::decompress(data, points * PLANES)
            .map_err(|e| WaveformError::Zstd(e.to_string()))?;
        Levels::from_raw(&dec, points)
    }
    #[cfg(not(all(feature = "zstd", not(target_arch = "wasm32"))))]
    {
        let _ = (data, points);
        Err(WaveformError::ZstdUnavailable)
    }
}

/// Options for [`Waveform::to_bytes`].
#[derive(Debug, Clone)]
pub struct EncodeOpts {
    /// zstd-compress the blocks (files). `false` = raw (wire format). Without the `zstd`
    /// feature, blocks are always written raw.
    pub compress: bool,
    pub include_overview: bool,
    pub include_detail: bool,
    pub zstd_level: i32,
}

impl EncodeOpts {
    /// Both levels, zstd level 3 (on-disk file).
    pub fn file() -> Self {
        Self::default()
    }
    /// Raw blocks, both levels.
    pub fn wire() -> Self {
        Self {
            compress: false,
            ..Self::default()
        }
    }
    pub fn wire_overview() -> Self {
        Self {
            compress: false,
            include_detail: false,
            ..Self::default()
        }
    }
    pub fn wire_detail() -> Self {
        Self {
            compress: false,
            include_overview: false,
            ..Self::default()
        }
    }
    /// Overview only, compressed (what the LRU evictor leaves behind).
    pub fn file_overview_only() -> Self {
        Self {
            include_detail: false,
            ..Self::default()
        }
    }
}

impl Default for EncodeOpts {
    fn default() -> Self {
        Self {
            compress: true,
            include_overview: true,
            include_detail: true,
            zstd_level: 3,
        }
    }
}

/// A decoded waveform. A container may hold only one level (HTTP `?level=`): an absent
/// overview is an empty [`Levels`] (`overview.n == 0`, see [`Waveform::has_overview`]); an
/// absent detail is `None`. The [`WaveformBuilder`](crate::builder::WaveformBuilder) always
/// yields both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waveform {
    pub sample_rate: u32,
    pub hop_samples: u32,
    pub total_samples: u64,
    pub source_hash: [u8; 16],
    pub overview: Levels,
    pub detail: Option<Levels>,
}

impl Waveform {
    pub fn has_overview(&self) -> bool {
        !self.overview.is_empty()
    }
    pub fn has_detail(&self) -> bool {
        self.detail.is_some()
    }
    pub fn duration_ms(&self) -> i64 {
        if self.sample_rate == 0 {
            return 0;
        }
        ((self.total_samples as f64 * 1000.0) / self.sample_rate as f64).round() as i64
    }
    pub fn duration_s(&self) -> f64 {
        if self.sample_rate == 0 {
            return 0.0;
        }
        self.total_samples as f64 / self.sample_rate as f64
    }
    /// Detail points per second.
    pub fn detail_rate_hz(&self) -> f32 {
        if self.hop_samples == 0 {
            return 0.0;
        }
        self.sample_rate as f32 / self.hop_samples as f32
    }
    /// Seconds covered by one detail point.
    pub fn detail_dt_s(&self) -> f64 {
        self.hop_samples as f64 / self.sample_rate.max(1) as f64
    }
    /// Number of detail points the full detail level has (`ceil(total / hop)`).
    pub fn expected_detail_points(&self) -> u32 {
        self.total_samples.div_ceil(self.hop_samples.max(1) as u64) as u32
    }
    pub fn source_hash_hex(&self) -> String {
        hex(&self.source_hash)
    }
    /// Build the overview from the detail level (for detail-only containers).
    pub fn ensure_overview(&mut self) {
        if self.overview.is_empty()
            && let Some(d) = &self.detail
        {
            self.overview = d.resample_max(OVERVIEW_POINTS);
        }
    }
    /// DTO for `waveform_meta`; `bytes` is 0 (use [`Waveform::meta_with_bytes`] when the
    /// stored size is known).
    pub fn meta(&self, track_id: bc_types::TrackId) -> WaveformMeta {
        self.meta_with_bytes(track_id, 0)
    }
    pub fn meta_with_bytes(&self, track_id: bc_types::TrackId, bytes: i64) -> WaveformMeta {
        WaveformMeta {
            track_id,
            format_version: VERSION,
            source_hash: self.source_hash_hex(),
            sample_rate: self.sample_rate,
            duration_ms: self.duration_ms(),
            overview_points: OVERVIEW_POINTS as u32,
            detail_points: self
                .detail
                .as_ref()
                .map_or(self.expected_detail_points(), |d| d.n as u32),
            detail_rate_hz: self.detail_rate_hz(),
            bytes,
        }
    }

    fn encode_block(l: &Levels, opts: &EncodeOpts) -> (Vec<u8>, bool) {
        let raw = l.to_raw();
        #[cfg(all(feature = "zstd", not(target_arch = "wasm32")))]
        if opts.compress
            && let Ok(z) = zstd::bulk::compress(&raw, opts.zstd_level)
        {
            return (z, false);
        }
        let _ = opts;
        (raw, true)
    }

    /// Serialise to a container.
    pub fn to_bytes(&self, opts: &EncodeOpts) -> Vec<u8> {
        let ov = (opts.include_overview && self.has_overview())
            .then(|| Self::encode_block(&self.overview, opts));
        let de = if opts.include_detail {
            self.detail.as_ref()
        } else {
            None
        }
        .map(|d| Self::encode_block(d, opts));
        // Raw framing is a single header flag: if one block fell back to raw, both are raw.
        let raw = ov.as_ref().is_none_or(|b| b.1) && de.as_ref().is_none_or(|b| b.1);
        let (ov, de) = if raw {
            let r = |l: &Levels| l.to_raw();
            (
                ov.map(|_| r(&self.overview)),
                de.map(|_| r(self.detail.as_ref().expect("detail present"))),
            )
        } else {
            (ov.map(|b| b.0), de.map(|b| b.0))
        };
        let mut flags = 0;
        if ov.is_some() {
            flags |= FLAG_OVERVIEW;
        }
        if de.is_some() {
            flags |= FLAG_DETAIL;
        }
        if raw {
            flags |= FLAG_RAW;
        }
        let h = Header {
            version: VERSION,
            flags,
            sample_rate: self.sample_rate,
            hop_samples: self.hop_samples,
            total_samples: self.total_samples,
            source_hash: self.source_hash,
            overview_points: if ov.is_some() {
                self.overview.n as u32
            } else {
                OVERVIEW_POINTS as u32
            },
            detail_points: self
                .detail
                .as_ref()
                .map_or(self.expected_detail_points(), |d| d.n as u32),
            overview_len: ov.as_ref().map_or(0, |b| b.len() as u32),
            detail_len: de.as_ref().map_or(0, |b| b.len() as u32),
        };
        let mut out = Vec::with_capacity(h.total_len());
        out.extend_from_slice(&h.write());
        if let Some(b) = ov {
            out.extend_from_slice(&b);
        }
        if let Some(b) = de {
            out.extend_from_slice(&b);
        }
        out
    }

    /// Parse a container (any subset of levels; raw or zstd blocks).
    pub fn from_bytes(bytes: &[u8]) -> Result<Waveform, WaveformError> {
        let h = read_header(bytes)?;
        if bytes.len() < h.total_len() {
            return Err(WaveformError::Truncated {
                need: h.total_len(),
                have: bytes.len(),
            });
        }
        let ov_end = HEADER_LEN + h.overview_len as usize;
        let overview = if h.has_overview() {
            decode_block(
                &bytes[HEADER_LEN..ov_end],
                h.overview_points as usize,
                h.is_raw(),
            )?
        } else {
            Levels::empty()
        };
        let detail = if h.has_detail() {
            let expect = h.total_samples.div_ceil(h.hop_samples as u64);
            if h.detail_points as u64 != expect {
                return Err(WaveformError::Corrupt(
                    "detail_points disagrees with total_samples",
                ));
            }
            Some(decode_block(
                &bytes[ov_end..ov_end + h.detail_len as usize],
                h.detail_points as usize,
                h.is_raw(),
            )?)
        } else {
            None
        };
        Ok(Waveform {
            sample_rate: h.sample_rate,
            hop_samples: h.hop_samples,
            total_samples: h.total_samples,
            source_hash: h.source_hash,
            overview,
            detail,
        })
    }
}
