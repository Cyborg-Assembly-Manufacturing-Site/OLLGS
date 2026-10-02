mod cache;
mod dump;
mod wikitext;

use std::collections::HashMap;
use std::path::PathBuf;
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
        "usage:\n  ollgs extract        read the Wiktionary dump once and write the dictionary cache\n  ollgs show WORD...   print the cached meanings of each word\n  ollgs templates      count the wiki templates used in cached meanings\n\nFiles live in $OLLGS_DATA (default ../OLLGS-data)."
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("extract") if args.len() == 1 => extract(),
        Some("show") if args.len() > 1 => show(&args[1..]),
        Some("templates") if args.len() == 1 => templates(),
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

fn extract() -> std::io::Result<()> {
    let dir = data_dir();
    let dump_path = dir.join(DUMP_FILE);
    let out_path = dir.join(CACHE_FILE);
    let tmp_path = dir.join(format!("{CACHE_FILE}.partial"));
    let started = Instant::now();

    let mut writer = cache::Writer::create(&tmp_path)?;
    let mut odd = wikitext::Oddities::default();
    let mut english = 0u64;
    let mut meanings = 0u64;
    let mut write_err = None;

    let (stats, sha1) = dump::for_each_page(&dump_path, |page| {
        if write_err.is_some() {
            return;
        }
        if let Some(target) = page.redirect {
            writer.redirect(page.title, target);
            return;
        }
        let blocks = wikitext::english_meanings(&page.text, &mut odd);
        if blocks.is_empty() {
            return;
        }
        english += 1;
        meanings += blocks.iter().map(|b| b.meanings.len() as u64).sum::<u64>();
        if let Err(e) = writer.entry(&page.title, &blocks) {
            write_err = Some(e);
        }
    })?;
    if let Some(e) = write_err {
        return Err(e);
    }
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
