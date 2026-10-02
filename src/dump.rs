//! Streams pages out of a MediaWiki XML export compressed with bzip2.
//!
//! The export puts each of `<title>`, `<ns>`, `<redirect>` and the opening
//! `<text>` tag on a line of its own, and escapes `<`, `>`, `&` and `"` inside
//! values, so a line scanner is enough and much cheaper than a full XML parser.
//! Anything that breaks that layout is reported as an error, not skipped.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::sync::mpsc;
use std::thread;

use bzip2::read::MultiBzDecoder;
use sha1::{Digest, Sha1};

/// One main-namespace page. Pages in other namespaces are counted but never copied.
pub struct Page {
    pub title: String,
    pub redirect: Option<String>,
    pub text: String,
}

#[derive(Default, Debug)]
pub struct DumpStats {
    pub pages: u64,
    pub main_pages: u64,
    pub compressed_bytes: u64,
    pub decompressed_bytes: u64,
}

const CHUNK: usize = 4 << 20;

/// Reads the file, hashes the compressed bytes with SHA-1 and decompresses on a
/// separate thread, so hashing and decompression overlap with page scanning.
struct Decompressor {
    rx: mpsc::Receiver<io::Result<Vec<u8>>>,
    cur: Vec<u8>,
    pos: usize,
    handle: Option<thread::JoinHandle<(String, u64)>>,
}

struct HashingReader<R> {
    inner: R,
    hasher: Sha1,
    bytes: u64,
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
}

impl Decompressor {
    fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let (tx, rx) = mpsc::sync_channel(8);
        let handle = thread::spawn(move || {
            let hashing = HashingReader { inner: BufReader::with_capacity(1 << 20, file), hasher: Sha1::new(), bytes: 0 };
            let mut dec = MultiBzDecoder::new(hashing);
            loop {
                let mut chunk = vec![0u8; CHUNK];
                let mut filled = 0;
                while filled < CHUNK {
                    match dec.read(&mut chunk[filled..]) {
                        Ok(0) => break,
                        Ok(n) => filled += n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            return (String::new(), 0);
                        }
                    }
                }
                chunk.truncate(filled);
                let done = filled < CHUNK;
                if filled > 0 && tx.send(Ok(chunk)).is_err() {
                    return (String::new(), 0);
                }
                if done {
                    break;
                }
            }
            // Drain anything after the last stream so the hash covers the whole file.
            let mut hashing = dec.into_inner();
            let _ = io::copy(&mut hashing, &mut io::sink());
            let digest = hashing.hasher.finalize();
            let hex = digest.iter().map(|b| format!("{b:02x}")).collect();
            (hex, hashing.bytes)
        });
        Ok(Decompressor { rx, cur: Vec::new(), pos: 0, handle: Some(handle) })
    }

    /// Waits for the reading thread and returns the SHA-1 of the compressed file and its size.
    fn finish(&mut self) -> (String, u64) {
        self.handle.take().map(|h| h.join().expect("decompression thread panicked")).unwrap_or_default()
    }
}

impl Read for Decompressor {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let avail = self.fill_buf()?;
        let n = avail.len().min(buf.len());
        buf[..n].copy_from_slice(&avail[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for Decompressor {
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

fn bad(line_no: u64, what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("dump line {line_no}: {what}"))
}

fn between<'a>(line: &'a str, open: &str, close: &str) -> Option<&'a str> {
    line.strip_prefix(open)?.strip_suffix(close)
}

/// Calls `f` for every main-namespace page, in dump order. Returns the stats and
/// the SHA-1 (hex) of the compressed file as actually read.
pub fn for_each_page(path: &Path, mut f: impl FnMut(Page)) -> io::Result<(DumpStats, String)> {
    let mut src = Decompressor::open(path)?;
    let mut stats = DumpStats::default();
    let mut line = String::new();
    let mut line_no = 0u64;

    let mut in_page = false;
    let mut title: Option<String> = None;
    let mut ns: Option<i64> = None;
    let mut redirect: Option<String> = None;
    let mut text: Option<String> = None;
    let mut in_text = false;

    loop {
        line.clear();
        let n = src.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        line_no += 1;
        stats.decompressed_bytes += n as u64;

        if in_text {
            let keep = ns == Some(0);
            match line.find("</text>") {
                Some(end) => {
                    if keep {
                        text.get_or_insert_with(String::new).push_str(&line[..end]);
                    }
                    in_text = false;
                }
                None => {
                    if keep {
                        text.get_or_insert_with(String::new).push_str(&line);
                    }
                }
            }
            continue;
        }

        let t = line.trim();
        if t == "<page>" {
            if in_page {
                return Err(bad(line_no, "<page> inside a page"));
            }
            in_page = true;
            title = None;
            ns = None;
            redirect = None;
            text = None;
        } else if !in_page {
            continue;
        } else if let Some(v) = between(t, "<title>", "</title>") {
            title = Some(unescape(v));
        } else if let Some(v) = between(t, "<ns>", "</ns>") {
            ns = Some(v.parse().map_err(|_| bad(line_no, "unreadable <ns>"))?);
        } else if let Some(v) = between(t, "<redirect title=\"", "\" />") {
            redirect = Some(unescape(v));
        } else if t.starts_with("<text") {
            let keep = ns == Some(0);
            let open_end = t.find('>').ok_or_else(|| bad(line_no, "unclosed <text> tag"))?;
            if t[..=open_end].ends_with("/>") {
                // Empty or deleted text.
                if keep {
                    text = Some(String::new());
                }
                continue;
            }
            // Content starts after the opening tag in the untrimmed line.
            let tag_at = line.find("<text").unwrap();
            let rest = &line[tag_at + line[tag_at..].find('>').unwrap() + 1..];
            match rest.find("</text>") {
                Some(end) => {
                    if keep {
                        text = Some(rest[..end].to_string());
                    }
                }
                None => {
                    if keep {
                        text = Some(rest.to_string());
                    }
                    in_text = true;
                }
            }
        } else if t == "</page>" {
            in_page = false;
            stats.pages += 1;
            if ns == Some(0) {
                stats.main_pages += 1;
                let title = title.take().ok_or_else(|| bad(line_no, "page without <title>"))?;
                let raw = text.take().unwrap_or_default();
                f(Page { title, redirect: redirect.take(), text: unescape(&raw) });
            }
        }
    }
    if in_page || in_text {
        return Err(bad(line_no, "dump ended inside a page"));
    }
    let (sha1, bytes) = src.finish();
    stats.compressed_bytes = bytes;
    Ok((stats, sha1))
}

/// Undoes the XML escaping MediaWiki applies: the five named entities and numeric references.
pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let semi = rest.as_bytes()[..rest.len().min(12)].iter().position(|&b| b == b';');
        let decoded = semi.and_then(|j| {
            let name = &rest[1..j];
            let c = match name {
                "lt" => Some('<'),
                "gt" => Some('>'),
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => {
                    let num = name.strip_prefix('#')?;
                    let code = match num.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                        None => num.parse().ok()?,
                    };
                    char::from_u32(code)
                }
            }?;
            Some((c, j + 1))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_entities() {
        assert_eq!(unescape("a &lt;b&gt; &amp;amp; &quot;c&quot; &#039;d&#x27; &bogus; & e"), "a <b> &amp; \"c\" 'd' &bogus; & e");
        assert_eq!(unescape("&ēēēēēē; &amp;ē"), "&ēēēēēē; &ē");
    }

    #[test]
    fn reads_main_namespace_pages_and_hash() {
        use bzip2::write::BzEncoder;
        use std::io::Write;

        let xml = "<mediawiki>\n  <page>\n    <title>Wiktionary:About</title>\n    <ns>4</ns>\n    <revision>\n      <text bytes=\"5\" xml:space=\"preserve\">==English==\n# &lt;no&gt;</text>\n    </revision>\n  </page>\n  <page>\n    <title>caf&#233; &amp; co</title>\n    <ns>0</ns>\n    <revision>\n      <text bytes=\"30\" xml:space=\"preserve\">==English==\n===Noun===\n# A &lt;b&gt;place&lt;/b&gt;.\n</text>\n    </revision>\n  </page>\n  <page>\n    <title>colour</title>\n    <ns>0</ns>\n    <redirect title=\"color\" />\n    <revision>\n      <text bytes=\"20\" xml:space=\"preserve\">#REDIRECT [[color]]</text>\n    </revision>\n  </page>\n  <page>\n    <title>empty</title>\n    <ns>0</ns>\n    <revision>\n      <text bytes=\"0\" />\n    </revision>\n  </page>\n</mediawiki>\n";
        let mut enc = BzEncoder::new(Vec::new(), bzip2::Compression::best());
        enc.write_all(xml.as_bytes()).unwrap();
        let compressed = enc.finish().unwrap();
        let path = std::env::temp_dir().join(format!("ollgs-test-{}.xml.bz2", std::process::id()));
        std::fs::write(&path, &compressed).unwrap();

        let mut pages = Vec::new();
        let (stats, sha1) = for_each_page(&path, |p| pages.push((p.title, p.redirect, p.text))).unwrap();
        std::fs::remove_file(&path).unwrap();

        let expected_sha1: String = Sha1::digest(&compressed).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(sha1, expected_sha1);
        assert_eq!(stats.compressed_bytes, compressed.len() as u64);
        assert_eq!((stats.pages, stats.main_pages), (4, 3));
        assert_eq!(
            pages,
            vec![
                ("café & co".to_string(), None, "==English==\n===Noun===\n# A <b>place</b>.\n".to_string()),
                ("colour".to_string(), Some("color".to_string()), "#REDIRECT [[color]]".to_string()),
                ("empty".to_string(), None, String::new()),
            ]
        );
    }
}
