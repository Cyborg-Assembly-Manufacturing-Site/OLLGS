//! The extracted dictionary on disk: every English entry's meanings as raw wikitext,
//! main-namespace redirects, and the template and module pages that say how
//! Wiktionary displays that wikitext, stamped with the SHA-1 of the dump it came from.
//!
//! Layout: magic, dump SHA-1 (40 hex bytes), entries, redirects, end marker.
//! Strings are a little-endian u32 length followed by UTF-8 bytes.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use crate::wikitext::{Meaning, WordTypeBlock};

const MAGIC: &[u8; 8] = b"OLLGSDC2";
const END: &[u8; 8] = b"OLLGSEND";

/// A template or module page, by full title ("Template:lb", "Module:labels/data").
pub struct WikiPage {
    pub redirect: Option<String>,
    pub text: String,
}

pub struct Dictionary {
    pub dump_sha1: String,
    pub entries: HashMap<String, Vec<WordTypeBlock>>,
    pub redirects: HashMap<String, String>,
    pub pages: HashMap<String, WikiPage>,
}

pub struct Writer {
    out: BufWriter<File>,
    entries: u64,
    redirects: Vec<(String, String)>,
}

fn put_str(out: &mut impl Write, s: &str) -> io::Result<()> {
    out.write_all(&(s.len() as u32).to_le_bytes())?;
    out.write_all(s.as_bytes())
}

impl Writer {
    pub fn create(path: &Path) -> io::Result<Self> {
        let mut out = BufWriter::with_capacity(1 << 20, File::create(path)?);
        out.write_all(MAGIC)?;
        Ok(Writer { out, entries: 0, redirects: Vec::new() })
    }

    pub fn entry(&mut self, title: &str, blocks: &[WordTypeBlock]) -> io::Result<()> {
        self.out.write_all(&[1])?;
        put_str(&mut self.out, title)?;
        self.out.write_all(&(blocks.len() as u32).to_le_bytes())?;
        for b in blocks {
            put_str(&mut self.out, &b.word_type)?;
            self.out.write_all(&(b.meanings.len() as u32).to_le_bytes())?;
            for m in &b.meanings {
                self.out.write_all(&[m.depth])?;
                put_str(&mut self.out, &m.text)?;
            }
        }
        self.entries += 1;
        Ok(())
    }

    pub fn page(&mut self, title: &str, redirect: Option<&str>, text: &str) -> io::Result<()> {
        self.out.write_all(&[3])?;
        put_str(&mut self.out, title)?;
        put_str(&mut self.out, redirect.unwrap_or(""))?;
        put_str(&mut self.out, text)
    }

    pub fn redirect(&mut self, from: String, to: String) {
        self.redirects.push((from, to));
    }

    /// Writes the redirects and the dump's SHA-1 last, so a cache from an
    /// interrupted run has no end marker and is refused when read.
    pub fn finish(mut self, dump_sha1: &str) -> io::Result<u64> {
        for (from, to) in &self.redirects {
            self.out.write_all(&[2])?;
            put_str(&mut self.out, from)?;
            put_str(&mut self.out, to)?;
        }
        self.out.write_all(&[0])?;
        put_str(&mut self.out, dump_sha1)?;
        self.out.write_all(&self.entries.to_le_bytes())?;
        self.out.write_all(END)?;
        self.out.flush()?;
        Ok(self.entries)
    }
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("dictionary cache: {what}"))
}

struct In<R> {
    r: R,
}

impl<R: Read> In<R> {
    fn bytes<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let mut b = [0u8; N];
        self.r.read_exact(&mut b)?;
        Ok(b)
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.bytes()?))
    }
    fn string(&mut self) -> io::Result<String> {
        let len = self.u32()? as usize;
        let mut v = vec![0u8; len];
        self.r.read_exact(&mut v)?;
        String::from_utf8(v).map_err(|_| bad("string is not UTF-8"))
    }
}

pub fn read(path: &Path) -> io::Result<Dictionary> {
    let mut r = In { r: BufReader::with_capacity(1 << 20, File::open(path)?) };
    if &r.bytes::<8>()? != MAGIC {
        return Err(bad("not an OLLGS dictionary cache"));
    }
    let mut entries = HashMap::new();
    let mut redirects = HashMap::new();
    let mut pages = HashMap::new();
    loop {
        match r.bytes::<1>()?[0] {
            1 => {
                let title = r.string()?;
                let n_blocks = r.u32()?;
                let mut blocks = Vec::with_capacity(n_blocks as usize);
                for _ in 0..n_blocks {
                    let word_type = r.string()?;
                    let n = r.u32()?;
                    let mut meanings = Vec::with_capacity(n as usize);
                    for _ in 0..n {
                        let depth = r.bytes::<1>()?[0];
                        meanings.push(Meaning { depth, text: r.string()? });
                    }
                    blocks.push(WordTypeBlock { word_type, meanings });
                }
                if entries.insert(title, blocks).is_some() {
                    return Err(bad("duplicate title"));
                }
            }
            2 => {
                let from = r.string()?;
                redirects.insert(from, r.string()?);
            }
            3 => {
                let title = r.string()?;
                let redirect = Some(r.string()?).filter(|t| !t.is_empty());
                pages.insert(title, WikiPage { redirect, text: r.string()? });
            }
            0 => break,
            _ => return Err(bad("corrupt record")),
        }
    }
    let dump_sha1 = r.string()?;
    let count = u64::from_le_bytes(r.bytes()?);
    if &r.bytes::<8>()? != END || count != entries.len() as u64 {
        return Err(bad("incomplete or corrupt (no end marker or wrong entry count)"));
    }
    Ok(Dictionary { dump_sha1, entries, redirects, pages })
}
