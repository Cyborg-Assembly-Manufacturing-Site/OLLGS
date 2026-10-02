mod bz2par;
mod cache;
mod closure;
mod dump;
mod lua;
mod render;
mod wikitext;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{mpsc, Mutex};
use std::process::ExitCode;
use std::time::Instant;

/// The one Wiktionary dump OLLGS reads, fixed so every run is reproducible.
const DUMP_FILE: &str = "enwiktionary-20261001-pages-articles.xml.bz2";
/// SHA-1 published by Wikimedia in enwiktionary-20261001-sha1sums.txt.
const DUMP_SHA1: &str = "f9ed426c69c6b21d026613bd0f200c2aaa1145e9";
const CACHE_FILE: &str = "dictionary.ollgs";

fn data_dir() -> PathBuf {
    std::env::var_os("OLLGS_DATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("../OLLGS-data"))
}

fn usage() -> ExitCode {
    eprintln!(
        "usage:\n  ollgs extract        read the Wiktionary dump once and write the dictionary cache\n  ollgs build          write the full licence\n  ollgs show WORD...   print the cached meanings of each word\n  ollgs templates      count the wiki templates used in cached meanings\n  ollgs render WORD... print each word's meanings as plain text\n  ollgs render-stats   render every meaning and report fallbacks\n  ollgs wiki TITLE...  print cached template or module pages, following redirects (TITLE* lists titles, ~TEXT finds templates containing TEXT)\n\nFiles live in $OLLGS_DATA (default ../OLLGS-data)."
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("extract") if args.len() == 1 => extract(),
        Some("show") if args.len() > 1 => show(&args[1..]),
        Some("templates") if args.len() == 1 => templates(),
        Some("wiki") if args.len() > 1 => wiki(&args[1..]),
        Some("build") if args.len() == 1 => build(),
        Some("render") if args.len() > 1 => render(&args[1..]),
        Some("render-text") if args.len() == 2 => {
            let dict = load();
            dict.map(|d| println!("{}", render::Renderer::new(&d).render("test", &args[1])))
        }
        Some("render-stats") if args.len() == 1 => render_stats(),
        _ => return usage(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ollgs: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Pages per batch handed to an extraction worker.
const BATCH: usize = 512;

/// What one batch of pages yields, in page order.
#[derive(Default)]
struct Batch {
    entries: Vec<(String, Vec<wikitext::WordTypeBlock>)>,
    redirects: Vec<(String, String)>,
    /// Template and module pages: (title, redirect target, text).
    pages: Vec<(String, Option<String>, String)>,
    odd: wikitext::Oddities,
}

fn extract_batch(pages: Vec<dump::Page>) -> std::io::Result<Batch> {
    let english = memchr::memmem::Finder::new(b"English");
    let mut out = Batch::default();
    for page in pages {
        if page.ns != 0 {
            let text = std::str::from_utf8(&page.escaped_text)
                .map_err(|_| std::io::Error::other(format!("page {:?} is not UTF-8", page.title)))?;
            out.pages.push((page.title, page.redirect, dump::unescape(text)));
            continue;
        }
        if let Some(target) = page.redirect {
            out.redirects.push((page.title, target));
            continue;
        }
        // Most pages have no English section; skip them before unescaping.
        if english.find(&page.escaped_text).is_none() {
            continue;
        }
        let text = std::str::from_utf8(&page.escaped_text)
            .map_err(|_| std::io::Error::other(format!("page {:?} is not UTF-8", page.title)))?;
        let blocks = wikitext::english_meanings(&dump::unescape(text), &mut out.odd);
        if !blocks.is_empty() {
            out.entries.push((page.title, blocks));
        }
    }
    Ok(out)
}

struct Written {
    writer: cache::Writer,
    english: u64,
    meanings: u64,
    pages: u64,
    odd: wikitext::Oddities,
}

fn write_batches(mut writer: cache::Writer, done: mpsc::Receiver<(usize, std::io::Result<Batch>)>) -> std::io::Result<Written> {
    let mut pending = BTreeMap::new();
    let mut next = 0;
    let (mut english, mut meanings, mut pages) = (0, 0, 0);
    let mut odd = wikitext::Oddities::default();
    for (i, batch) in done {
        pending.insert(i, batch);
        while let Some(batch) = pending.remove(&next) {
            let batch = batch?;
            for (title, blocks) in &batch.entries {
                writer.entry(title, blocks)?;
                english += 1;
                meanings += blocks.iter().map(|b| b.meanings.len() as u64).sum::<u64>();
            }
            for (title, redirect, text) in &batch.pages {
                writer.page(title, redirect.as_deref(), text)?;
                pages += 1;
            }
            for (from, to) in batch.redirects {
                writer.redirect(from, to);
            }
            odd.skipped_headings_with_meanings.extend(batch.odd.skipped_headings_with_meanings);
            odd.orphan_meaning_lines += batch.odd.orphan_meaning_lines;
            next += 1;
        }
    }
    Ok(Written { writer, english, meanings, pages, odd })
}

fn extract() -> std::io::Result<()> {
    let dir = data_dir();
    let dump_path = dir.join(DUMP_FILE);
    let out_path = dir.join(CACHE_FILE);
    let tmp_path = dir.join(format!("{CACHE_FILE}.partial"));
    let started = Instant::now();

    let writer = cache::Writer::create(&tmp_path)?;
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());

    // The reader hands pages out in numbered batches; workers extract meanings on
    // all cores; the writer puts batches back in dump order, so the cache is the
    // same on every run.
    let (work_tx, work_rx) = mpsc::sync_channel::<(usize, Vec<dump::Page>)>(threads * 2);
    let work_rx = Mutex::new(work_rx);
    let (done_tx, done_rx) = mpsc::channel::<(usize, std::io::Result<Batch>)>();

    let (read, written) = std::thread::scope(|s| {
        for _ in 0..threads {
            let (work_rx, done_tx) = (&work_rx, done_tx.clone());
            s.spawn(move || {
                // After the writer stops (on an error), keep draining so the reader never blocks.
                let mut writer_alive = true;
                loop {
                    let next = work_rx.lock().unwrap().recv();
                    let Ok((i, pages)) = next else { break };
                    if writer_alive && done_tx.send((i, extract_batch(pages))).is_err() {
                        writer_alive = false;
                    }
                }
            });
        }
        drop(done_tx);
        let writer = s.spawn(move || write_batches(writer, done_rx));

        let mut batch = Vec::with_capacity(BATCH);
        let mut n = 0;
        let read = dump::for_each_page(&dump_path, |page| {
            batch.push(page);
            if batch.len() == BATCH {
                let full = std::mem::replace(&mut batch, Vec::with_capacity(BATCH));
                let _ = work_tx.send((n, full));
                n += 1;
            }
        });
        if !batch.is_empty() {
            let _ = work_tx.send((n, batch));
        }
        drop(work_tx);
        (read, writer.join().expect("writer thread panicked"))
    });
    let (stats, sha1) = read?;
    let Written { writer, english, meanings, pages, odd } = written?;
    if sha1 != DUMP_SHA1 {
        std::fs::remove_file(&tmp_path)?;
        return Err(std::io::Error::other(format!(
            "{} has SHA-1 {sha1}, expected {DUMP_SHA1}; refusing to use it",
            dump_path.display()
        )));
    }
    let entries = writer.finish(&sha1)?;
    std::fs::rename(&tmp_path, &out_path)?;

    let mut skipped: HashMap<&str, u64> = HashMap::new();
    for h in &odd.skipped_headings_with_meanings {
        *skipped.entry(h).or_default() += 1;
    }
    let mut skipped: Vec<_> = skipped.into_iter().collect();
    skipped.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));

    println!("dump SHA-1        {sha1} (matches Wikimedia's)");
    println!("compressed bytes  {}", stats.compressed_bytes);
    println!("decompressed      {}", stats.decompressed_bytes);
    println!("pages             {}", stats.pages);
    println!("main-space pages  {}", stats.main_pages);
    println!("English entries   {english} (written {entries})");
    println!("meaning lines     {meanings}");
    println!("template/module   {pages} pages");
    println!("orphan lines      {}", odd.orphan_meaning_lines);
    println!("cache             {} ({} bytes)", out_path.display(), std::fs::metadata(&out_path)?.len());
    println!("time              {:.1} s", started.elapsed().as_secs_f64());
    println!("non-word-type headings with meaning-shaped lines (pages):");
    for (name, n) in skipped.iter().take(40) {
        println!("  {n:>8}  {name}");
    }
    Ok(())
}

fn load() -> std::io::Result<cache::Dictionary> {
    let dict = cache::read(&data_dir().join(CACHE_FILE))?;
    if dict.dump_sha1 != DUMP_SHA1 {
        return Err(std::io::Error::other("cache was built from a different dump; run `ollgs extract`"));
    }
    Ok(dict)
}

fn templates() -> std::io::Result<()> {
    let dict = load()?;
    let mut counts: HashMap<String, u64> = HashMap::new();
    let (mut lines, mut with_template) = (0u64, 0u64);
    for blocks in dict.entries.values() {
        for m in blocks.iter().flat_map(|b| &b.meanings) {
            lines += 1;
            let mut any = false;
            let mut rest = m.text.as_str();
            while let Some(i) = rest.find("{{") {
                rest = &rest[i + 2..];
                let end = rest.find(['|', '}']).unwrap_or(rest.len());
                *counts.entry(rest[..end].trim().to_string()).or_default() += 1;
                any = true;
            }
            with_template += any as u64;
        }
    }
    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let total: u64 = counts.iter().map(|c| c.1).sum();
    println!("meaning lines {lines}, with templates {with_template}, template uses {total}, distinct {}", counts.len());
    let mut cum = 0;
    for (i, (name, n)) in counts.iter().enumerate() {
        cum += n;
        println!("{:>5} {n:>9} {:>6.2}%  {name}", i + 1, 100.0 * cum as f64 / total as f64);
    }
    Ok(())
}

/// The licence OLLGS starts from, relative to the repository.
const LICENSE_FILE: &str = "input/sss-license.txt";
const DEFINITIONS_MARKER: &str = "[definitions]";

/// The credit Wiktionary's licence (CC BY-SA 4.0) requires, placed after the
/// credit line and before the definitions (owner's choice). Its words are not looked up.
fn wiktionary_credit() -> String {
    format!(
        "The definitions below are taken from English Wiktionary (https://en.wiktionary.org/), written by \
         Wiktionary's contributors, as published in the database dump of 1 October 2026 ({DUMP_FILE}, SHA-1 \
         {DUMP_SHA1}). OLLGS changed them: it turned Wiktionary's markup into plain text, kept only English \
         meanings and their sub-meanings, and left out examples, quotations and notices for Wiktionary's editors. \
         The definitions, and this licence text with them, are shared under the Creative Commons \
         Attribution-ShareAlike 4.0 International licence (CC BY-SA 4.0): \
         https://creativecommons.org/licenses/by-sa/4.0/"
    )
}

fn write_entry(out: &mut impl std::io::Write, title: &str, rendered: &closure::Rendered) -> std::io::Result<()> {
    writeln!(out, "\"{title}\":")?;
    for (word_type, meanings) in rendered {
        writeln!(out, "{word_type}")?;
        let mut counters: Vec<usize> = Vec::new();
        for (depth, text) in meanings {
            let d = (*depth as usize).max(1);
            counters.truncate(d);
            while counters.len() < d {
                counters.push(0);
            }
            counters[d - 1] += 1;
            writeln!(out, "{}{}. {text}", "   ".repeat(d - 1), counters[d - 1])?;
        }
    }
    writeln!(out)
}

fn build() -> std::io::Result<()> {
    let started = Instant::now();
    let licence = std::fs::read_to_string(LICENSE_FILE)?;
    let lines: Vec<&str> = licence.lines().collect();
    let (name, credit) = (lines.first().copied().unwrap_or(""), lines.iter().find(|l| l.starts_with("Credit:")).copied().unwrap_or(""));
    // Words come from everything except the name line, the credit line and the marker.
    let start_lines: Vec<String> = lines
        .iter()
        .skip(1)
        .filter(|l| !l.starts_with("Credit:") && l.trim() != DEFINITIONS_MARKER && !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect();

    let dict = load()?;
    let r = render::Renderer::new(&dict);
    let rendered = closure::render_all(&dict, &r);
    let rendered_at = started.elapsed();
    let lookup = closure::Lookup { rendered: &rendered, redirects: &dict.redirects };
    let c = closure::close(&lookup, &start_lines);
    let closed_at = started.elapsed();

    let out_dir = data_dir().join("output");
    std::fs::create_dir_all(&out_dir)?;
    let out_path = out_dir.join("sss-license.txt");
    let mut out = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&out_path)?);
    use std::io::Write;
    let body_start = lines.iter().position(|l| l.trim() == "Definitions").unwrap_or(lines.len());
    writeln!(out, "{name}\n\n{credit}\n\n{}\n", wiktionary_credit())?;
    for l in &lines[body_start..] {
        if l.trim() == DEFINITIONS_MARKER {
            for item in &c.order {
                match item {
                    closure::Item::Entry(title) => write_entry(&mut out, title, &rendered[title])?,
                    closure::Item::NoEntry(word) => writeln!(out, "\"{word}\":\n{}\n", closure::NO_ENTRY_NOTE)?,
                }
            }
        } else {
            writeln!(out, "{l}")?;
        }
    }
    out.flush()?;
    drop(out);

    let size = std::fs::metadata(&out_path)?.len();
    let entries: Vec<&str> = c.order.iter().filter_map(|i| match i { closure::Item::Entry(t) => Some(*t), _ => None }).collect();
    let words = entries.len();
    let meanings: usize = entries.iter().map(|t| rendered[t].iter().map(|(_, m)| m.len()).sum::<usize>()).sum();
    println!("rendered {} entries in {:.1} s; chain closed in {:.1} s", rendered.len(), rendered_at.as_secs_f64(), (closed_at - rendered_at).as_secs_f64());
    println!("defined words     {words} of {} English entries ({:.1}%)", rendered.len(), 100.0 * words as f64 / rendered.len() as f64);
    println!("meanings          {meanings}");
    println!("first 40 in order {:?}", &entries[..entries.len().min(40)]);
    println!("words with no entry: {} different, met {} times", c.missing.len(), c.missing.iter().map(|m| m.1).sum::<u64>());
    let mut top = c.missing.clone();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    println!("  most frequent: {:?}", &top[..top.len().min(40)]);
    println!("  first met:     {:?}", &c.missing[..c.missing.len().min(25)]);
    println!("output            {} ({:.1} MB), total {:.1} s", out_path.display(), size as f64 / 1e6, started.elapsed().as_secs_f64());
    Ok(())
}

fn render(words: &[String]) -> std::io::Result<()> {
    let dict = load()?;
    let r = render::Renderer::new(&dict);
    for w in words {
        println!("\"{w}\":");
        match dict.entries.get(w) {
            Some(blocks) => {
                for b in blocks {
                    println!("{}", b.word_type);
                    for m in &b.meanings {
                        println!("{}{}", "  ".repeat(m.depth as usize), r.render(w, &m.text));
                    }
                }
            }
            None => println!("  (no English entry)"),
        }
        println!();
    }
    Ok(())
}

fn render_stats() -> std::io::Result<()> {
    let started = Instant::now();
    let dict = load()?;
    let loaded = started.elapsed();
    let r = render::Renderer::new(&dict);
    let entries: Vec<(&String, &Vec<wikitext::WordTypeBlock>)> = dict.entries.iter().collect();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let render_start = Instant::now();
    let results: Vec<(u64, u64, u64, Vec<(String, String)>)> = std::thread::scope(|s| {
        let handles: Vec<_> = entries
            .chunks(entries.len().div_ceil(threads))
            .map(|chunk| {
                let r = &r;
                s.spawn(move || {
                    let (mut lines, mut empty, mut leftover) = (0u64, 0u64, 0u64);
                    let mut samples = Vec::new();
                    for (title, blocks) in chunk {
                        for m in blocks.iter().flat_map(|b| &b.meanings) {
                            lines += 1;
                            let text = r.render(title, &m.text);
                            if text.is_empty() {
                                empty += 1;
                            } else if text.contains("{{") || text.contains("}}") || text.contains("[[") || text.contains("]]") {
                                leftover += 1;
                                if samples.len() < 5 {
                                    samples.push((title.to_string(), text));
                                }
                            }
                        }
                    }
                    (lines, empty, leftover, samples)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let render_time = render_start.elapsed();
    let lines: u64 = results.iter().map(|r| r.0).sum();
    let empty: u64 = results.iter().map(|r| r.1).sum();
    let leftover: u64 = results.iter().map(|r| r.2).sum();
    let stats = r.stats.lock().unwrap();
    println!("load {:.1} s, render {:.1} s on {threads} threads", loaded.as_secs_f64(), render_time.as_secs_f64());
    println!("meaning lines {lines}, rendered empty {empty}, with leftover markup {leftover}, depth limit hit {}", stats.depth_exceeded);
    for (title, text) in results.iter().flat_map(|r| &r.3).take(10) {
        println!("  leftover in {title:?}: {text}");
    }
    let top = |m: &std::collections::BTreeMap<String, u64>, what: &str| {
        let mut v: Vec<_> = m.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let total: u64 = v.iter().map(|x| *x.1).sum();
        println!("{what}: {} kinds, {total} uses", v.len());
        for (k, n) in v.iter().take(40) {
            let sample = stats.fallback_samples.get(k.as_str()).map(|t| format!("  e.g. {t:?}")).unwrap_or_default();
            println!("  {n:>8}  {k}{sample}");
        }
    };
    top(&stats.fallback_invokes, "Lua programs shown with the plain fallback");
    top(&stats.missing_templates, "templates with no page in the dump");
    top(&stats.maintenance, "editor notices left out");
    println!("data-table statements skipped (not plain literals): {:?}", stats.skipped_data);
    Ok(())
}

fn wiki(titles: &[String]) -> std::io::Result<()> {
    let dict = load()?;
    for t in titles {
        if let Some(needle) = t.strip_prefix('~') {
            let mut found: Vec<&String> = dict.pages.iter().filter(|(k, p)| k.starts_with("Template:") && p.text.contains(needle)).map(|(k, _)| k).collect();
            found.sort();
            println!("{} templates contain {needle:?}: {:?}", found.len(), found.iter().take(30).collect::<Vec<_>>());
            continue;
        }
        if let Some(prefix) = t.strip_suffix('*') {
            let mut found: Vec<&String> = dict.pages.keys().filter(|k| k.starts_with(prefix)).collect();
            found.sort();
            for k in found {
                println!("{k}");
            }
            continue;
        }
        let mut title = t.clone();
        for _ in 0..5 {
            match dict.pages.get(&title) {
                Some(p) => match &p.redirect {
                    Some(to) => {
                        println!("== {title} -> {to}");
                        title = to.clone();
                    }
                    None => {
                        println!("== {title}\n{}\n", p.text);
                        break;
                    }
                },
                None => {
                    println!("== {title}: not in the cache\n");
                    break;
                }
            }
        }
    }
    Ok(())
}

fn show(words: &[String]) -> std::io::Result<()> {
    let dict = load()?;
    for w in words {
        println!("\"{w}\":");
        match (dict.entries.get(w), dict.redirects.get(w)) {
            (Some(blocks), _) => {
                for b in blocks {
                    println!("{}", b.word_type);
                    for m in &b.meanings {
                        println!("{}{}", "  ".repeat(m.depth as usize), m.text);
                    }
                }
            }
            (None, Some(to)) => println!("  (redirects to {to})"),
            (None, None) => println!("  (no English entry)"),
        }
        println!();
    }
    Ok(())
}
