//! Pulls the meanings out of a page's English section, still as raw wikitext.
//!
//! Owner's decisions this follows: every English meaning counts; only meanings
//! and their sub-meanings count (no example sentences, quotations, etymology,
//! pronunciation, synonyms or translations); meanings are grouped by word type.

/// Headings Wiktionary uses for word types (Wiktionary:Entry layout, "Part of speech").
pub const WORD_TYPES: &[&str] = &[
    "Adjective", "Adverb", "Ambiposition", "Article", "Circumposition", "Classifier",
    "Conjunction", "Contraction", "Counter", "Determiner", "Ideophone", "Interjection",
    "Noun", "Numeral", "Participle", "Particle", "Postposition", "Preposition", "Pronoun",
    "Proper noun", "Verb",
    "Circumfix", "Combining form", "Infix", "Interfix", "Prefix", "Root", "Suffix",
    "Diacritical mark", "Letter", "Ligature", "Number", "Punctuation mark", "Syllable", "Symbol",
    "Phrase", "Proverb", "Prepositional phrase",
    "Han character", "Hanzi", "Kanji", "Hanja", "Romanization",
];

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Meaning {
    /// 1 for a meaning, 2 for a sub-meaning, and so on.
    pub depth: u8,
    pub text: String,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct WordTypeBlock {
    pub word_type: String,
    pub meanings: Vec<Meaning>,
}

/// What the extractor saw besides meanings, for checking the extraction against the data.
#[derive(Default, Debug)]
pub struct Oddities {
    /// Headings that had meaning-shaped lines under them but are not word types.
    pub skipped_headings_with_meanings: Vec<String>,
    /// Meaning-shaped lines under no heading at all.
    pub orphan_meaning_lines: usize,
}

/// Removes `<!-- ... -->` comments; an unclosed comment runs to the end, as in MediaWiki.
pub fn strip_comments(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains("<!--") {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("<!--") {
        out.push_str(&rest[..i]);
        match rest[i + 4..].find("-->") {
            Some(j) => rest = &rest[i + 4 + j + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    std::borrow::Cow::Owned(out)
}

/// Returns (level, name) if the line is a heading such as `===Noun===`.
pub fn heading(line: &str) -> Option<(usize, &str)> {
    let t = line.trim_end();
    if !t.starts_with('=') {
        return None;
    }
    let lead = t.bytes().take_while(|&b| b == b'=').count();
    let trail = t.bytes().rev().take_while(|&b| b == b'=').count();
    if lead == t.len() {
        return None;
    }
    let level = lead.min(trail);
    if level == 0 {
        return None;
    }
    Some((level, t[level..t.len() - level].trim()))
}

/// The lines of the `==English==` section, if the page has one.
pub fn english_lines(text: &str) -> Option<Vec<&str>> {
    let mut lines = text.lines();
    loop {
        let l = lines.next()?;
        if heading(l) == Some((2, "English")) {
            break;
        }
    }
    Some(lines.take_while(|l| !matches!(heading(l), Some((lvl, _)) if lvl <= 2)).collect())
}

fn brace_balance(s: &str) -> i64 {
    s.matches("{{").count() as i64 - s.matches("}}").count() as i64
}

/// Splits a line starting with `#` into (depth, rest) when it is a meaning line,
/// i.e. not an example (`#:`), quotation (`#*`) or their continuations.
fn meaning_line(line: &str) -> Option<(u8, &str)> {
    let depth = line.bytes().take_while(|&b| b == b'#').count();
    if depth == 0 {
        return None;
    }
    let rest = &line[depth..];
    if rest.starts_with([':', '*']) {
        return None;
    }
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    Some((depth.min(u8::MAX as usize) as u8, rest))
}

/// Extracts the English meanings of one page, grouped by word-type heading, in page order.
pub fn english_meanings(page_text: &str, odd: &mut Oddities) -> Vec<WordTypeBlock> {
    let text = strip_comments(page_text);
    let Some(lines) = english_lines(&text) else { return Vec::new() };

    let mut blocks: Vec<WordTypeBlock> = Vec::new();
    // None before any heading; Some(None) under a heading that is not a word type.
    let mut current: Option<Option<String>> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if let Some((_, name)) = heading(line) {
            current = Some(WORD_TYPES.contains(&name).then(|| name.to_string()));
            continue;
        }
        let Some((depth, first)) = meaning_line(line) else { continue };
        // A template left open continues on the following lines.
        let mut text = first.to_string();
        let mut balance = brace_balance(&text);
        while balance > 0 && i < lines.len() && heading(lines[i]).is_none() && !lines[i].starts_with('#') {
            text.push(' ');
            text.push_str(lines[i].trim());
            balance = brace_balance(&text);
            i += 1;
        }
        match &current {
            Some(Some(word_type)) => {
                if blocks.last().is_none_or(|b| &b.word_type != word_type) {
                    blocks.push(WordTypeBlock { word_type: word_type.clone(), meanings: Vec::new() });
                }
                blocks.last_mut().unwrap().meanings.push(Meaning { depth, text });
            }
            Some(None) => {
                let (_, name) = lines[..i].iter().rev().find_map(|l| heading(l)).unwrap();
                if odd.skipped_headings_with_meanings.last().map(String::as_str) != Some(name) {
                    odd.skipped_headings_with_meanings.push(name.to_string());
                }
            }
            None => odd.orphan_meaning_lines += 1,
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = "\
{{also|Used}}
==English==
===Etymology===
From {{m|en|use}}.

===Verb===
{{head|en|verb form}}

# {{infl of|en|use||ed-form}}
#: {{ux|en|I used it.}}
#* {{quote-book|en|year=1948
|title=Something}}
## A sub-meaning.
#*: continuation of a quote

====Synonyms====
* {{l|en|employed}}

===Adjective===
{{en-adj}}

# That has previously been owned by {{lb|en|informal
}} someone else. <!-- # not a meaning -->
<!--
# also not a meaning
-->
#

===Noun===
# A third meaning.

==French==
===Verb===
# {{inflection of|fr|user}}
";

    #[test]
    fn extracts_english_meanings_by_word_type() {
        let mut odd = Oddities::default();
        let blocks = english_meanings(PAGE, &mut odd);
        let got: Vec<(&str, Vec<(u8, &str)>)> = blocks
            .iter()
            .map(|b| (b.word_type.as_str(), b.meanings.iter().map(|m| (m.depth, m.text.as_str())).collect()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("Verb", vec![(1, "{{infl of|en|use||ed-form}}"), (2, "A sub-meaning.")]),
                ("Adjective", vec![(1, "That has previously been owned by {{lb|en|informal }} someone else.")]),
                ("Noun", vec![(1, "A third meaning.")]),
            ]
        );
        assert!(odd.skipped_headings_with_meanings.is_empty());
        assert_eq!(odd.orphan_meaning_lines, 0);
    }

    #[test]
    fn page_without_english_has_no_meanings() {
        let mut odd = Oddities::default();
        assert!(english_meanings("==French==\n===Noun===\n# chat\n", &mut odd).is_empty());
    }

    #[test]
    fn records_meaning_lines_under_other_headings() {
        let mut odd = Oddities::default();
        let blocks = english_meanings("==English==\n===Usage notes===\n# a numbered note\n# another\n", &mut odd);
        assert!(blocks.is_empty());
        assert_eq!(odd.skipped_headings_with_meanings, vec!["Usage notes".to_string()]);
    }

    #[test]
    fn headings() {
        assert_eq!(heading("===Proper noun=== "), Some((3, "Proper noun")));
        assert_eq!(heading("== English =="), Some((2, "English")));
        assert_eq!(heading("===="), None);
        assert_eq!(heading("# x"), None);
    }
}
