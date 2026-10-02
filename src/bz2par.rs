//! Decompresses a single-stream bzip2 file on all cores.
//!
//! A bzip2 stream is a sequence of independently compressed blocks, each
//! starting with the 48-bit marker 0x314159265359 at any bit position, and the
//! stream ends with the marker 0x177245385090 followed by a CRC combining every
//! block's CRC. We find the markers, wrap each block in a stream of its own,
//! decompress the blocks in parallel and hand them on in file order.
//!
//! A marker can also occur by chance inside compressed data. Such a false
//! boundary makes the block before it fail to decompress (it is cut short), so
//! that block is retried with the following boundaries until it succeeds, and a
//! piece that starts at a false boundary never decompresses and yields nothing.
//! Every real block is checked against its own CRC by the decoder, and the
//! combined CRC of all of them must equal the one stored at the end of the file,
//! so a wrong split cannot pass silently.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufRead, Read};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;

use bzip2::{Decompress, Status};
use memmap2::Mmap;
use sha1::{Digest, Sha1};

const BLOCK_MAGIC: u64 = 0x3141_5926_5359;
const END_MAGIC: u64 = 0x1772_4538_5090;
/// How many following boundaries a block may swallow when they turn out to be false.
const MAX_SWALLOW: usize = 8;
/// Decompressed blocks allowed in flight ahead of the reader (a few MB each).
const WINDOW: usize = 64;

/// Reads `n` (at most 57) bits starting at bit `pos`, most significant bit first.
fn bits(data: &[u8], pos: u64, n: u32) -> u64 {
    let byte = (pos / 8) as usize;
    let mut w = [0u8; 8];
    let avail = data.len().saturating_sub(byte).min(8);
    w[..avail].copy_from_slice(&data[byte..byte + avail]);
    let v = u64::from_be_bytes(w);
    (v << (pos % 8)) >> (64 - n)
}

/// Bit positions of every occurrence of a 48-bit marker, in increasing order.
fn find_marker(data: &[u8], magic: u64, threads: usize) -> Vec<u64> {
    let chunk = data.len().div_ceil(threads).max(1 << 20);
    let mut found: Vec<u64> = thread::scope(|s| {
        let handles: Vec<_> = (0..data.len().div_ceil(chunk))
            .map(|c| {
                s.spawn(move || {
                    let lo = c * chunk;
                    // Overlap the next chunk so a marker across the seam is still seen.
                    let part = &data[lo..(lo + chunk + 8).min(data.len())];
                    let mut out = Vec::new();
                    for shift in 0..8u32 {
                        // An 8-byte window whose marker starts `shift` bits in: its
                        // bytes 1..=5 are marker bits whatever the shift.
                        let window = (magic << (16 - shift)).to_be_bytes();
                        for m in memchr::memmem::find_iter(part, &window[1..6]) {
                            let Some(first) = (lo + m).checked_sub(1) else { continue };
                            let pos = first as u64 * 8 + shift as u64;
                            if bits(data, pos, 48) == magic {
                                out.push(pos);
                            }
                        }
                    }
                    out
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    found.sort_unstable();
    found.dedup();
    found
}

/// Wraps the block bits [start, end) in a complete one-block bzip2 stream.
fn standalone(data: &[u8], level: u8, start: u64, end: u64, crc: u32) -> Vec<u8> {
    let nbits = end - start;
    let full = (nbits / 8) as usize;
    let a = (start / 8) as usize;
    let s = (start % 8) as u32;
    let mut out = Vec::with_capacity(full + 20);
    out.extend_from_slice(b"BZh");
    out.push(level);
    if s == 0 {
        out.extend_from_slice(&data[a..a + full]);
    } else {
        // Shift eight bytes at a time; byte k of the result is data[a+k] << s | data[a+k+1] >> (8-s).
        let src = &data[a..=a + full];
        out.resize(4 + full, 0);
        let dst = &mut out[4..];
        let words = full / 8;
        for k in 0..words {
            let w = u64::from_be_bytes(src[8 * k..8 * k + 8].try_into().unwrap());
            let next = src[8 * k + 8] as u64;
            dst[8 * k..8 * k + 8].copy_from_slice(&((w << s) | (next >> (8 - s))).to_be_bytes());
        }
        for k in 8 * words..full {
            dst[k] = (src[k] << s) | (src[k + 1] >> (8 - s));
        }
    }
    // Remaining bits, end marker and CRC, packed and padded to a whole byte.
    let rem = (nbits % 8) as u32;
    let mut acc: u128 = if rem == 0 { 0 } else { bits(data, start + 8 * full as u64, rem) as u128 };
    acc = (acc << 48) | END_MAGIC as u128;
    acc = (acc << 32) | crc as u128;
    let total = rem + 80;
    let padded = total.div_ceil(8) * 8;
    acc <<= padded - total;
    let bytes = acc.to_be_bytes();
    out.extend_from_slice(&bytes[16 - (padded / 8) as usize..]);
    out
}

fn decode(stream: &[u8]) -> Option<Vec<u8>> {
    let mut d = Decompress::new(false);
    let mut out = Vec::with_capacity(stream.len() * 6);
    loop {
        if out.len() == out.capacity() {
            out.reserve(out.capacity());
        }
        let before = (d.total_in(), d.total_out());
        match d.decompress_vec(&stream[d.total_in() as usize..], &mut out) {
            Ok(Status::StreamEnd) => return (d.total_in() as usize == stream.len()).then_some(out),
            Ok(_) if (d.total_in(), d.total_out()) == before && out.len() < out.capacity() => return None,
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

struct Plan {
    level: u8,
    /// Candidate block starts (bit positions), then the end marker's position.
    bounds: Vec<u64>,
    stored_crc: u32,
}

fn plan(data: &[u8], threads: usize) -> io::Result<Plan> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, format!("bzip2: {m}"));
    if data.len() < 14 || &data[..3] != b"BZh" || !(b'1'..=b'9').contains(&data[3]) {
        return Err(bad("not a bzip2 file"));
    }
    let total_bits = data.len() as u64 * 8;
    // The real end marker is the one followed only by the CRC and under a byte of padding.
    let end = find_marker(data, END_MAGIC, threads)
        .into_iter()
        .rev()
        .find(|&p| p + 80 <= total_bits && total_bits - (p + 80) < 8)
        .ok_or_else(|| bad("no end-of-stream marker at the end (multi-stream files are not supported)"))?;
    let mut bounds: Vec<u64> = find_marker(data, BLOCK_MAGIC, threads).into_iter().filter(|&p| p < end).collect();
    if bounds.first() != Some(&32) {
        return Err(bad("first block does not follow the header"));
    }
    let stored_crc = bits(data, end + 48, 32) as u32;
    bounds.push(end);
    Ok(Plan { level: data[3], bounds, stored_crc })
}

/// A decompressed block: its bytes, its CRC, and the index of the boundary it ends at.
type Piece = Option<(Vec<u8>, u32, usize)>;

/// Decompresses piece `i`, swallowing following boundaries if they prove false.
/// None means the piece starts at a false boundary.
fn piece(data: &[u8], p: &Plan, i: usize) -> Piece {
    let start = p.bounds[i];
    let crc = bits(data, start + 48, 32) as u32;
    let last = (i + 1 + MAX_SWALLOW).min(p.bounds.len() - 1);
    (i + 1..=last).find_map(|j| decode(&standalone(data, p.level, start, p.bounds[j], crc)).map(|out| (out, crc, j)))
}

/// A reader over the decompressed file, in order, that also reports the SHA-1
/// and size of the compressed file once it has been read to the end.
pub struct ParallelDecoder {
    rx: mpsc::Receiver<io::Result<Vec<u8>>>,
    cur: Vec<u8>,
    pos: usize,
    handle: Option<thread::JoinHandle<(String, u64)>>,
}

impl ParallelDecoder {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        // SAFETY: the dump is a read-only input that nothing modifies while we run.
        let map = Arc::new(unsafe { Mmap::map(&file)? });
        let threads = thread::available_parallelism().map_or(4, |n| n.get());
        let (tx, rx) = mpsc::sync_channel(WINDOW);

        let handle = thread::spawn(move || {
            let data: &[u8] = &map;
            let hasher = {
                let map = Arc::clone(&map);
                thread::spawn(move || {
                    let digest = Sha1::digest(&map[..]);
                    digest.iter().map(|b| format!("{b:02x}")).collect::<String>()
                })
            };
            let finish = |tx: &mpsc::SyncSender<_>, r: io::Result<()>| {
                if let Err(e) = r {
                    let _ = tx.send(Err(e));
                }
            };
            let p = match plan(data, threads) {
                Ok(p) => p,
                Err(e) => {
                    finish(&tx, Err(e));
                    return (hasher.join().unwrap(), data.len() as u64);
                }
            };
            let pieces = p.bounds.len() - 1;
            let next = AtomicUsize::new(0);
            let emitted = Mutex::new(0usize);
            let room = Condvar::new();
            let (rtx, rrx) = mpsc::channel::<(usize, Piece)>();

            let result = thread::scope(|s| {
                for _ in 0..threads {
                    let rtx = rtx.clone();
                    let (p, next, emitted, room) = (&p, &next, &emitted, &room);
                    s.spawn(move || loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= pieces {
                            break;
                        }
                        {
                            let mut e = emitted.lock().unwrap();
                            while i >= *e + WINDOW {
                                e = room.wait(e).unwrap();
                            }
                        }
                        if rtx.send((i, piece(data, p, i))).is_err() {
                            break;
                        }
                    });
                }
                drop(rtx);

                let bad = |m: String| Err(io::Error::new(io::ErrorKind::InvalidData, format!("bzip2: {m}")));
                let mut pending = BTreeMap::new();
                let mut want = 0;
                // Index of the boundary where the next real block must start.
                let mut expected = 0;
                let mut combined = 0u32;
                for (i, r) in rrx.iter() {
                    pending.insert(i, r);
                    while let Some(r) = pending.remove(&want) {
                        match r {
                            Some((out, crc, end)) => {
                                if want != expected {
                                    return bad(format!("block at boundary {want} overlaps the one before it"));
                                }
                                combined = combined.rotate_left(1) ^ crc;
                                expected = end;
                                if tx.send(Ok(out)).is_err() {
                                    return Ok(());
                                }
                            }
                            None if want == expected => return bad(format!("block at boundary {want} is unreadable")),
                            None => {}
                        }
                        want += 1;
                        *emitted.lock().unwrap() = want;
                        room.notify_all();
                    }
                }
                if expected != pieces {
                    return bad(format!("blocks end at boundary {expected}, not at the end marker ({pieces})"));
                }
                if combined != p.stored_crc {
                    return bad("combined CRC does not match the file".to_string());
                }
                Ok(())
            });
            finish(&tx, result);
            (hasher.join().unwrap(), data.len() as u64)
        });
        Ok(ParallelDecoder { rx, cur: Vec::new(), pos: 0, handle: Some(handle) })
    }

    /// Waits for the decoder and returns the SHA-1 (hex) and size of the compressed file.
    pub fn finish(&mut self) -> (String, u64) {
        self.handle.take().map(|h| h.join().expect("decoder thread panicked")).unwrap_or_default()
    }
}

impl Read for ParallelDecoder {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let avail = self.fill_buf()?;
        let n = avail.len().min(buf.len());
        buf[..n].copy_from_slice(&avail[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for ParallelDecoder {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        while self.pos >= self.cur.len() {
            match self.rx.recv() {
                Ok(Ok(chunk)) => {
                    self.cur = chunk;
                    self.pos = 0;
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => return Ok(&[]),
            }
        }
        Ok(&self.cur[self.pos..])
    }

    fn consume(&mut self, amt: usize) {
        self.pos += amt;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bzip2::write::BzEncoder;
    use std::io::Write;

    /// Text that compresses into many small blocks at level 1 (100 kB per block).
    fn sample() -> Vec<u8> {
        let mut x = 0x2545F4914F6CDD1Du64;
        (0..3_000_000)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                if i % 97 == 0 { b'\n' } else { b"abcdefgh  ijk"[(x % 13) as usize] }
            })
            .collect()
    }

    fn compress(raw: &[u8]) -> Vec<u8> {
        let mut enc = BzEncoder::new(Vec::new(), bzip2::Compression::fast());
        enc.write_all(raw).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn parallel_decode_matches_input() {
        let raw = sample();
        let compressed = compress(&raw);
        let path = std::env::temp_dir().join(format!("ollgs-bz2par-{}.bz2", std::process::id()));
        std::fs::write(&path, &compressed).unwrap();
        let mut dec = ParallelDecoder::open(&path).unwrap();
        let mut out = Vec::new();
        dec.read_to_end(&mut out).unwrap();
        let (sha1, size) = dec.finish();
        std::fs::remove_file(&path).unwrap();
        assert!(out == raw, "decompressed output differs ({} vs {} bytes)", out.len(), raw.len());
        assert_eq!(size, compressed.len() as u64);
        assert_eq!(sha1, Sha1::digest(&compressed).iter().map(|b| format!("{b:02x}")).collect::<String>());
        assert!(plan(&compressed, 4).unwrap().bounds.len() > 10, "sample should span many blocks");
    }

    #[test]
    fn false_boundary_is_swallowed() {
        let compressed = compress(&sample());
        let mut p = plan(&compressed, 4).unwrap();
        let real: Vec<u64> = p.bounds.clone();
        // Pretend a marker was found in the middle of the first block.
        let fake = (real[0] + real[1]) / 2;
        p.bounds.insert(1, fake);
        let (_, _, end) = piece(&compressed, &p, 0).expect("first block decodes past the false boundary");
        assert_eq!(p.bounds[end], real[1]);
        assert!(piece(&compressed, &p, 1).is_none());
        assert!(piece(&compressed, &p, 2).is_some());
    }

    #[test]
    fn reads_bits() {
        let d = [0b1010_1100, 0b0101_0011, 0xFF];
        assert_eq!(bits(&d, 0, 4), 0b1010);
        assert_eq!(bits(&d, 4, 8), 0b1100_0101);
        assert_eq!(bits(&d, 12, 4), 0b0011);
        assert_eq!(bits(&d, 16, 8), 0xFF);
        assert_eq!(bits(&d, 22, 6), 0b11_0000, "bits past the end read as zero");
    }
}
