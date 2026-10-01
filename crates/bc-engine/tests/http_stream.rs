//! The HTTP range reader (Bandcamp streams): a local server with `Range`
//! support serves a WAV; decoding over HTTP must equal decoding the file, both
//! from the start and from a seek into the middle, and must fetch in blocks.

use bc_engine::decode::{Source, decode_range};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn wav_bytes(secs: f32) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.wav");
    let spec = hound::WavSpec { channels: 2, sample_rate: 44_100, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(&p, spec).unwrap();
    for i in 0..(44_100.0 * secs) as usize {
        let v = (((i as f32 * 0.021).sin() + (i as f32 * 0.0007).sin()) * 9000.0) as i16;
        w.write_sample(v).unwrap();
        w.write_sample(v / 2).unwrap();
    }
    w.finalize().unwrap();
    std::fs::read(&p).unwrap()
}

/// A tiny HTTP/1.1 server: `GET` with an optional `Range: bytes=a-b`, `Connection: close`.
fn serve(data: Arc<Vec<u8>>, ranges: bool) -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/track.wav", l.local_addr().unwrap());
    let (requests, bytes_sent) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (rq, bs) = (requests.clone(), bytes_sent.clone());
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let Ok(mut c) = conn else { continue };
            let (data, rq, bs) = (data.clone(), rq.clone(), bs.clone());
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 8192];
                let mut n = 0;
                loop {
                    let k = c.read(&mut buf[n..]).unwrap_or(0);
                    if k == 0 {
                        return;
                    }
                    n += k;
                    if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                rq.fetch_add(1, Ordering::Relaxed);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let range = head.lines().find_map(|l| {
                    let l = l.to_ascii_lowercase();
                    let v = l.strip_prefix("range: bytes=")?.to_string();
                    let (a, b) = v.split_once('-')?;
                    Some((a.trim().parse::<usize>().ok()?, b.trim().parse::<usize>().ok()))
                });
                let total = data.len();
                let resp = match range.filter(|_| ranges) {
                    Some((a, b)) if a < total => {
                        let b = b.unwrap_or(total - 1).min(total - 1);
                        let body = &data[a..=b];
                        bs.fetch_add(body.len(), Ordering::Relaxed);
                        let mut r = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/wav\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {a}-{b}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        r.extend_from_slice(body);
                        r
                    }
                    _ => {
                        bs.fetch_add(total, Ordering::Relaxed);
                        let mut r = format!("HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n").into_bytes();
                        r.extend_from_slice(&data);
                        r
                    }
                };
                let _ = c.write_all(&resp);
            });
        }
    });
    (url, requests, bytes_sent)
}

#[test]
fn http_decode_equals_file_decode_with_range_requests() {
    let bytes = wav_bytes(12.0); // about 2 MB: several 512 KiB blocks
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.wav");
    std::fs::write(&path, &bytes).unwrap();
    let (url, requests, sent) = serve(Arc::new(bytes.clone()), true);

    for (start, end) in [(0.0, Some(3.0)), (7.5, Some(9.0))] {
        let (a, fa) = decode_range(&Source::File(path.clone()), 48_000, start, end).unwrap();
        let (b, fb) = decode_range(&Source::Http { url: url.clone() }, 48_000, start, end).unwrap();
        assert_eq!(fa, fb, "same start frame");
        assert_eq!(a.len(), b.len(), "same length for [{start}, {end:?}]");
        let worst = a.iter().zip(&b).map(|(x, y)| (x - y).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1e-6, "streams decode identically (worst diff {worst})");
        assert!(a.len() as f64 / 2.0 > (end.unwrap() - start) * 48_000.0 * 0.99);
    }
    // it fetched in blocks (not byte by byte), and a seek did not download the whole file again
    let n = requests.load(Ordering::Relaxed);
    assert!((2..40).contains(&n), "{n} range requests");
    assert!(sent.load(Ordering::Relaxed) < bytes.len() * 4, "fetched {} bytes of a {} byte file", sent.load(Ordering::Relaxed), bytes.len());
}

#[test]
fn server_without_range_support_is_read_whole() {
    let bytes = wav_bytes(3.0);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.wav");
    std::fs::write(&path, &bytes).unwrap();
    let (url, _r, _s) = serve(Arc::new(bytes), false);
    let (a, _) = decode_range(&Source::File(path), 48_000, 0.0, Some(2.0)).unwrap();
    let (b, _) = decode_range(&Source::Http { url }, 48_000, 0.0, Some(2.0)).unwrap();
    assert_eq!(a.len(), b.len());
    assert!(a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-6));
}
