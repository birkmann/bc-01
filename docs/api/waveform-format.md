# `.bcw2` waveform format (workstream 3, `bc-waveform`)

Version 3 (v1 and v2 files are stale: a reader rejects them with `UnsupportedVersion`, the store treats them as a cache miss and the track is rebuilt on demand). Version 3 changed the pooling: the overview and the pyramid use **energy-mean pooling** for the RMS and band planes (see Levels). The byte layout is unchanged. All integers little-endian. One file per track at
`<cache>/waveforms/{track_id/1000}/{track_id}.bcw2`. The same container is the HTTP wire
format of `GET /api/tracks/{id}/waveform?level=overview|detail` (blocks stored raw there).

## Header (64 bytes, never compressed)

| off | size | field | notes |
| --- | --- | --- | --- |
| 0 | 4 | magic | `BCW2` |
| 4 | 2 | version | `3` |
| 6 | 2 | flags | bit0 overview present, bit1 detail present, bit2 blocks are **raw** (not zstd) |
| 8 | 4 | sample_rate | native rate of the source (Hz) |
| 12 | 4 | hop_samples | samples per detail point = `round(sample_rate * 256 / 44100)` (256 @ 44.1 kHz, 279 @ 48 kHz) so every file has ~5.805 ms / ~172.27 pts/s |
| 16 | 8 | total_samples | per channel at `sample_rate`; duration = total/sample_rate |
| 24 | 16 | source_hash | blake3 (truncated to 16 bytes) of `size ‖ first 64 KiB ‖ last 64 KiB` of the source file. Hex of it is the `ETag` base |
| 40 | 4 | overview_points | always 2048 |
| 44 | 4 | detail_points | `ceil(total_samples / hop_samples)` |
| 48 | 4 | overview_len | bytes of the overview block **as stored/transmitted** (0 if absent) |
| 52 | 4 | detail_len | likewise |
| 56 | 8 | reserved | zero |

Body: `overview block` immediately followed by `detail block`. In a file both are zstd frames
(level 3); on the wire (flag bit2) they are raw. The overview can be read without touching the
detail block. A file whose detail was evicted by the LRU cache has bit1 clear, `detail_len=0`.
`overview_points` and `detail_points` are written even for an absent level (so an overview-only
wire container still announces the detail size); the flag bits say what is present. A
detail-only container is valid (the client derives the overview by max-pooling). Decoders reject
`detail_points != ceil(total_samples / hop_samples)`, unknown versions, truncated bodies and
blocks whose decoded size is not `6 * points`. Block framing (raw vs zstd) is one flag for the whole
file; without the `zstd` feature the writer always emits raw.

## Block layout (after decompression)

`N` points, **planar**: six planes of `N` bytes in this order

1. `peak_pos` - largest positive sample in the point
2. `peak_neg` - magnitude of the most negative sample
3. `rms`      - RMS of the point
4. `low`      - RMS of the low band (< ~200 Hz)
5. `mid`      - RMS of the mid band (~200 Hz .. ~2.5 kHz)
6. `high`     - RMS of the high band (> ~2.5 kHz)

Planar layout compresses far better than interleaved; a client uploading to a texture
interleaves while building the mip pyramid.

## Value scale (absolute, not per-track)

`u8 = round(255 * clamp((dBFS + 60) / 60, 0, 1))`; `0` = silence (<= -60 dBFS), `255` = 0 dBFS.
`dBFS = 20*log10(linear)`. Both peaks are stored as magnitudes. Normalisation (the renderer's per-track p95 reference levels, `bc_waveform::bars::reference_levels`) is a
render-time computation, never stored. Bands are the Linkwitz-Riley 4th-order split of the mono sum
(L+R)/2 at 200 Hz and 2.5 kHz (low = LP200, mid = HP200 -> LP2500, high = HP2500) and use the
same mapping on their own RMS.

## Levels

* **Detail**: one point per `hop_samples`, ~172 pts/s. ~1 KB/s compressed.
* **Overview**: exactly 2048 points over the whole duration. Point `j` covers
  `[j*T/2048, (j+1)*T/2048)`; computed over the covered detail points: the peak planes are
  **max-pooled**, the `rms` and band planes use **energy-mean pooling**,
  `sqrt(mean(lin^2))` in the linear domain re-encoded to the byte scale (max-pooling them made
  every bucket the loudest 5.8 ms window, so loud masters sat at the ceiling and breakdowns
  vanished). Fewer detail points than 2048 => nearest-neighbour repeat.
* **Mip pyramid** (client/render side, `bc_waveform::mip::Pyramid`): level 0 = detail,
  level k = pool x2 of level k-1 (max for peaks, energy mean for rms and bands), until `<= 2048` points; the last level is (resampled to)
  the overview so a client holding only the overview still renders the whole track.

## HTTP

`GET /api/tracks/{id}/waveform?level=overview|detail[&v=<hash>]`

* `200 application/octet-stream`, a valid `BCW2` container containing exactly the requested
  level (other `*_len` = 0, flag bit2 set).
* `ETag: "<hex source_hash>-v<format version>-<level>"`; `If-None-Match` honoured with `304`.
* `Cache-Control: public, max-age=31536000, immutable` only when `v=<hex source_hash>` matches
  **and** `f=<format version>` (currently `3`) is present, otherwise `no-cache` (revalidate by
  ETag). The `f` parameter keeps a format bump from being shadowed by an immutable-cached old
  response; clients should send both (`?level=overview&v=<hash>&f=3`).
* Detail is computed on demand (decode ~100-200 ms) when not cached; `404` only if the track has
  no available file. `X-BCW-Duration-Ms`, `X-BCW-Sample-Rate` headers mirror the header.
