//! Streams pages out of a MediaWiki XML export compressed with bzip2.
//!
//! The export puts each of `<title>`, `<ns>`, `<redirect>` and the opening
//! `<text>` tag on a line of its own, and escapes `<`, `>`, `&` and `"` inside
//! values, so a line scanner is enough and much cheaper than a full XML parser.
//! Anything that breaks that layout is reported as an error, not skipped.

use std::io::{self, BufRead};
use std::path::Path;

use crate::bz2par::ParallelDecoder;

/// One main-namespace page, its text still XML-escaped so the caller can
/// unescape it on another thread.
pub struct Page {
    pub title: String,
    pub redirect: Option<String>,
    pub escaped_text: Vec<u8>,
}

#[derive(Default, Debug)]
pub struct DumpStats {
    pub pages: u64,
    pub main_pages: u64,
    pub compressed_bytes: u64,
    pub decompressed_bytes: u64,
}

fn bad(line_no: u64, what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("dump line {line_no}: {what}"))
}

fn between<'a>(line: &'a [u8], open: &[u8], close: &[u8]) -> Option<&'a [u8]> {
    line.strip_prefix(open)?.strip_suffix(close)
}

fn text(bytes: &[u8], line_no: u64) -> io::Result<String> {
    std::str::from_utf8(bytes).map(unescape).map_err(|_| bad(line_no, "not UTF-8"))
}

/// Calls `f` for every main-namespace page, in dump order. Returns the stats and
/// the SHA-1 (hex) of the compressed file as actually read.
pub fn for_each_page(path: &Path, mut f: impl FnMut(Page)) -> io::Result<(DumpStats, String)> {
    let mut src = ParallelDecoder::open(path)?;
    let mut stats = DumpStats::default();
    let close_text = memchr::memmem::Finder::new(b"</text>");
    let mut line = Vec::with_capacity(1 << 16);
    let mut line_no = 0u64;

    let mut in_page = false;
    let mut title: Option<Vec<u8>> = None;
    let mut ns: Option<i64> = None;
    let mut redirect: Option<Vec<u8>> = None;
    let mut body: Vec<u8> = Vec::new();
    let mut in_text = false;

    loop {
        line.clear();
        let n = src.read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        line_no += 1;
        stats.decompressed_bytes += n as u64;

        if in_text {
            let end = close_text.find(&line);
            if ns == Some(0) {
                body.extend_from_slice(&line[..end.unwrap_or(line.len())]);
            }
            in_text = end.is_none();
            continue;
        }

        let t = line.trim_ascii();
        if t == b"<page>" {
            if in_page {
                return Err(bad(line_no, "<page> inside a page"));
            }
            in_page = true;
            title = None;
            ns = None;
            redirect = None;
            body.clear();
        } else if !in_page {
            continue;
        } else if let Some(v) = between(t, b"<title>", b"</title>") {
            title = Some(v.to_vec());
        } else if let Some(v) = between(t, b"<ns>", b"</ns>") {
            let v = std::str::from_utf8(v).ok().and_then(|v| v.parse().ok());
            ns = Some(v.ok_or_else(|| bad(line_no, "unreadable <ns>"))?);
        } else if let Some(v) = between(t, b"<redirect title=\"", b"\" />") {
            redirect = Some(v.to_vec());
        } else if t.starts_with(b"<text") {
            let open_end = memchr::memchr(b'>', &line).ok_or_else(|| bad(line_no, "unclosed <text> tag"))?;
            if line[..open_end].ends_with(b"/") {
                continue; // empty or deleted text
            }
            let rest = &line[open_end + 1..];
            let end = close_text.find(rest);
            if ns == Some(0) {
                body.extend_from_slice(&rest[..end.unwrap_or(rest.len())]);
            }
            in_text = end.is_none();
        } else if t == b"</page>" {
            in_page = false;
            stats.pages += 1;
            if ns == Some(0) {
                stats.main_pages += 1;
                let title = title.take().ok_or_else(|| bad(line_no, "page without <title>"))?;
                f(Page {
                    title: text(&title, line_no)?,
                    redirect: redirect.take().map(|r| text(&r, line_no)).transpose()?,
                    escaped_text: std::mem::take(&mut body),
                });
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
        use sha1::{Digest, Sha1};
        use std::io::Write;

        let xml = "<mediawiki>\n  <page>\n    <title>Wiktionary:About</title>\n    <ns>4</ns>\n    <revision>\n      <text bytes=\"5\" xml:space=\"preserve\">==English==\n# &lt;no&gt;</text>\n    </revision>\n  </page>\n  <page>\n    <title>caf&#233; &amp; co</title>\n    <ns>0</ns>\n    <revision>\n      <text bytes=\"30\" xml:space=\"preserve\">==English==\n===Noun===\n# A &lt;b&gt;place&lt;/b&gt;.\n</text>\n    </revision>\n  </page>\n  <page>\n    <title>colour</title>\n    <ns>0</ns>\n    <redirect title=\"color\" />\n    <revision>\n      <text bytes=\"20\" xml:space=\"preserve\">#REDIRECT [[color]]</text>\n    </revision>\n  </page>\n  <page>\n    <title>empty</title>\n    <ns>0</ns>\n    <revision>\n      <text bytes=\"0\" />\n    </revision>\n  </page>\n</mediawiki>\n";
        let mut enc = BzEncoder::new(Vec::new(), bzip2::Compression::best());
        enc.write_all(xml.as_bytes()).unwrap();
        let compressed = enc.finish().unwrap();
        let path = std::env::temp_dir().join(format!("ollgs-test-{}.xml.bz2", std::process::id()));
        std::fs::write(&path, &compressed).unwrap();

        let mut pages = Vec::new();
        let (stats, sha1) = for_each_page(&path, |p| pages.push((p.title, p.redirect, unescape(std::str::from_utf8(&p.escaped_text).unwrap())))).unwrap();
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
