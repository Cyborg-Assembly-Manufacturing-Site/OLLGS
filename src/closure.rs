//! The lookup chain: start from the licence's words, define each one, then
//! every new word in those definitions, until no new word turns up.
//!
//! Owner's rules this follows:
//! - each spelling is its own entry, defined once, in order of discovery;
//! - all English meanings, grouped by word type, labels kept;
//! - a word capitalised only because it starts a sentence, line or heading is
//!   looked up in lowercase; other capitals are kept, falling back to lowercase
//!   when there is no entry;
//! - a hyphenated or apostrophe word is defined whole if Wiktionary has it,
//!   otherwise by its parts ("owner's" -> "owner" + "'s");
//! - only things containing a letter are looked up.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::cache::Dictionary;
use crate::render::Renderer;

/// One entry's meanings as plain text: (word type, [(depth, text)]).
pub type Rendered = Vec<(String, Vec<(u8, String)>)>;

/// Renders every English entry on all cores, dropping meanings that come out
/// empty (they were only an editor's notice) and word types left with none.
pub fn render_all<'d>(dict: &'d Dictionary, r: &Renderer) -> HashMap<&'d str, Rendered> {
    let entries: Vec<_> = dict.entries.iter().collect();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    std::thread::scope(|s| {
        let handles: Vec<_> = entries
            .chunks(entries.len().div_ceil(threads).max(1))
            .map(|chunk| {
                s.spawn(move || {
                    chunk
                        .iter()
                        .filter_map(|(title, blocks)| {
                            let rendered: Rendered = blocks
                                .iter()
                                .map(|b| {
                                    let meanings = b
                                        .meanings
                                        .iter()
                                        .map(|m| (m.depth, r.render(title, &m.text)))
                                        .filter(|(_, t)| !t.is_empty())
                                        .collect::<Vec<_>>();
                                    (b.word_type.clone(), meanings)
                                })
                                .filter(|(_, m)| !m.is_empty())
                                .collect();
                            (!rendered.is_empty()).then_some((title.as_str(), rendered))
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    })
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '\'' | '’' | '-' | '.') || ('\u{0300}'..='\u{036F}').contains(&c)
}

/// Calls `f(word, sentence_start)` for each word-like token of one line of text,
/// in reading order. A token is sentence-initial when no word outside
/// parentheses has come before it since the line start or the last ". ", "! "
/// or "? ".
pub fn tokens<'l>(line: &'l str, mut f: impl FnMut(&'l str, bool)) {
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let byte = |k: usize| chars.get(k).map_or(line.len(), |&(b, _)| b);
    let next_is_space = |k: usize| chars.get(k).is_none_or(|&(_, n)| n.is_whitespace());
    let mut sentence_start = true;
    let mut paren_depth = 0usize;
    let mut k = 0;
    while k < chars.len() {
        let c = chars[k].1;
        if c.is_alphanumeric() {
            let mut e = k;
            while e < chars.len() && is_word_char(chars[e].1) {
                e += 1;
            }
            let raw = &line[byte(k)..byte(e)];
            // Trailing apostrophes and hyphens are quotation marks or dashes. A trailing
            // full stop stays, so abbreviations ("e.g.") can be tried whole.
            let word = raw.trim_end_matches(['\'', '’', '-']);
            // A word right after a backslash is a formula command (\mathcal), not a word.
            let formula = k > 0 && chars[k - 1].1 == '\\';
            if !formula && word.chars().any(char::is_alphabetic) {
                f(word, sentence_start && paren_depth == 0);
                if paren_depth == 0 {
                    sentence_start = false;
                }
            }
            if raw.ends_with('.') && next_is_space(e) {
                sentence_start = true;
            }
            k = e;
            continue;
        }
        match c {
            '(' | '[' => paren_depth += 1,
            ')' | ']' => paren_depth = paren_depth.saturating_sub(1),
            '.' | '!' | '?' if next_is_space(k + 1) => sentence_start = true,
            _ => {}
        }
        k += 1;
    }
}

const CLITICS: &[&str] = &["'s", "'ll", "'re", "'ve", "'d", "'m", "n't"];

pub struct Lookup<'a> {
    pub rendered: &'a HashMap<&'a str, Rendered>,
    pub redirects: &'a HashMap<String, String>,
}

impl<'a> Lookup<'a> {
    fn entry(&self, form: &str) -> Option<&'a str> {
        if let Some((&k, _)) = self.rendered.get_key_value(form) {
            return Some(k);
        }
        let target = self.redirects.get(form)?;
        self.rendered.get_key_value(target.as_str()).map(|(&k, _)| k)
    }

    /// The forms to try for a token, in order, following the capital-letter rule.
    fn forms(token: &str, sentence_start: bool) -> Vec<String> {
        let lower_first = {
            let mut c = token.chars();
            c.next().map(|f| f.to_lowercase().chain(c).collect::<String>()).unwrap_or_default()
        };
        let starts_upper = token.chars().next().is_some_and(char::is_uppercase);
        let mut forms = if !starts_upper {
            vec![token.to_string()]
        } else if sentence_start {
            vec![lower_first, token.to_string(), token.to_lowercase()]
        } else {
            vec![token.to_string(), lower_first, token.to_lowercase()]
        };
        // Abbreviations keep their final stop ("e.g."); other words lose it.
        let mut out = Vec::new();
        for f in forms.drain(..) {
            let f = f.replace('’', "'");
            if !out.contains(&f) {
                out.push(f.clone());
            }
            let bare = f.trim_end_matches('.').to_string();
            if !bare.is_empty() && !out.contains(&bare) {
                out.push(bare);
            }
        }
        out
    }

    /// The entry for the whole token, if Wiktionary has one.
    pub fn whole(&self, token: &str, sentence_start: bool) -> Option<&'a str> {
        Self::forms(token, sentence_start).iter().find_map(|f| self.entry(f))
    }

    /// The entry for a phrase of several tokens (owner's choice: tried only for a
    /// word with no entry of its own), longest first, up to three words.
    pub fn phrase(&self, words: &[(&str, bool)], i: usize) -> Option<&'a str> {
        for len in [3, 2] {
            for first in i.saturating_sub(len - 1)..=i {
                let Some(window) = words.get(first..first + len) else { continue };
                let joined = window.iter().map(|(w, _)| *w).collect::<Vec<_>>().join(" ");
                if let Some(t) = self.whole(&joined, window[0].1) {
                    return Some(t);
                }
            }
        }
        None
    }

    /// Resolves a token to entry titles: the whole word if Wiktionary has it,
    /// otherwise its parts. Returns the titles found and the parts with no entry.
    pub fn resolve(&self, token: &str, sentence_start: bool, found: &mut Vec<&'a str>, missing: &mut Vec<String>) {
        if let Some(title) = self.whole(token, sentence_start) {
            found.push(title);
            return;
        }
        let base = token.replace('’', "'");
        let base = base.trim_end_matches('.');
        // Apostrophe endings: "owner's" -> "owner" + "'s".
        for clitic in CLITICS {
            if let Some(stem) = base.strip_suffix(clitic) {
                if stem.chars().any(char::is_alphabetic) && !stem.ends_with('\'') {
                    self.resolve(stem, sentence_start, found, missing);
                    match self.entry(clitic) {
                        Some(t) => found.push(t),
                        None => missing.push(clitic.to_string()),
                    }
                    return;
                }
            }
        }
        // Hyphenated and dotted words: their parts.
        for sep in ['-', '.', '\''] {
            if base.contains(sep) {
                let mut first = true;
                for part in base.split(sep).filter(|p| p.chars().any(char::is_alphabetic)) {
                    self.resolve(part, sentence_start && first, found, missing);
                    first = false;
                }
                return;
            }
        }
        // Recorded as written, or in lowercase where the capital is only grammar.
        let shown = if sentence_start { Self::forms(token, true).swap_remove(0) } else { base.to_string() };
        missing.push(shown.trim_end_matches('.').to_string());
    }
}

/// What a word with no entry says instead of a definition (owner's choice: list
/// it, saying so). Its own words are defined like any other text in the licence.
pub const NO_ENTRY_NOTE: &str = "(no entry in English Wiktionary)";

/// One word of the licence, in order of discovery.
#[derive(Debug, PartialEq, Eq)]
pub enum Item<'a> {
    Entry(&'a str),
    NoEntry(String),
}

/// The result of the lookup chain.
pub struct Closure<'a> {
    /// Every word, in order of discovery.
    pub order: Vec<Item<'a>>,
    /// Words with no entry, in order of discovery, with how often each was met.
    pub missing: Vec<(String, u64)>,
}

/// Runs the chain from the given lines of starting text.
pub fn close<'a>(lookup: &Lookup<'a>, start: &[String]) -> Closure<'a> {
    struct State<'a> {
        seen: HashSet<&'a str>,
        order: Vec<Item<'a>>,
        queue: VecDeque<&'a str>,
        missing_index: HashMap<String, usize>,
        missing: Vec<(String, u64)>,
        found: Vec<&'a str>,
        not_found: Vec<String>,
    }
    fn take_line<'a>(lookup: &Lookup<'a>, st: &mut State<'a>, line: &str) {
        let mut words: Vec<(&str, bool)> = Vec::new();
        tokens(line, |w, start| words.push((w, start)));
        for (i, &(w, start)) in words.iter().enumerate() {
            st.found.clear();
            st.not_found.clear();
            match lookup.whole(w, start).or_else(|| lookup.phrase(&words, i)) {
                Some(t) => st.found.push(t),
                None => lookup.resolve(w, start, &mut st.found, &mut st.not_found),
            }
            for &t in &st.found {
                if st.seen.insert(t) {
                    st.order.push(Item::Entry(t));
                    st.queue.push_back(t);
                }
            }
            for m in st.not_found.drain(..) {
                match st.missing_index.get(&m) {
                    Some(&i) => st.missing[i].1 += 1,
                    None => {
                        st.missing_index.insert(m.clone(), st.missing.len());
                        st.order.push(Item::NoEntry(m.clone()));
                        st.missing.push((m, 1));
                    }
                }
            }
        }
    }
    let mut st = State {
        seen: HashSet::new(),
        order: Vec::new(),
        queue: VecDeque::new(),
        missing_index: HashMap::new(),
        missing: Vec::new(),
        found: Vec::new(),
        not_found: Vec::new(),
    };
    let mut note_read = false;
    let mut read = |st: &mut State<'a>, line: &str| {
        take_line(lookup, st, line);
        // The note appears in the licence once the first word without an entry does.
        if !note_read && !st.missing.is_empty() {
            note_read = true;
            take_line(lookup, st, NO_ENTRY_NOTE);
        }
    };
    for line in start {
        read(&mut st, line);
    }
    while let Some(title) = st.queue.pop_front() {
        for (word_type, meanings) in &lookup.rendered[title] {
            read(&mut st, word_type);
            for (_, text) in meanings {
                read(&mut st, text);
            }
        }
    }
    Closure { order: st.order, missing: st.missing }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<(String, bool)> {
        let mut v = Vec::new();
        tokens(s, |w, st| v.push((w.to_string(), st)));
        v
    }

    #[test]
    fn splits_words_and_marks_sentence_starts() {
        assert_eq!(
            toks("1. This license applies: in open-source work. People's (US) e.g. it, 10th 1948 & %."),
            [
                ("This", true),
                ("license", false),
                ("applies", false),
                ("in", false),
                ("open-source", false),
                ("work.", false),
                ("People's", true),
                ("US", false),
                ("e.g.", false),
                ("it", true),
                ("10th", false),
            ]
            .map(|(w, s)| (w.to_string(), s))
        );
        assert_eq!(toks("f = \\mathcal{F}"), [("f", true), ("F", false)].map(|(w, s)| (w.to_string(), s)));
        assert_eq!(toks("(transitive) To employ 'quoted' words—dash"), [("transitive", false), ("To", true), ("employ", false), ("quoted", false), ("words", false), ("dash", false)].map(|(w, s)| (w.to_string(), s)));
    }

    fn rendered(words: &[(&'static str, &'static str)]) -> HashMap<&'static str, Rendered> {
        words.iter().map(|(w, def)| (*w, vec![("Noun".to_string(), vec![(1u8, def.to_string())])])).collect()
    }

    #[test]
    fn resolves_by_the_owners_rules() {
        let r = rendered(&[("this", ""), ("This", ""), ("English", ""), ("english", ""), ("about", ""), ("open-source", ""), ("owner", ""), ("'s", ""), ("e.g.", ""), ("work", ""), ("closed", ""), ("source", "")]);
        let redirects = HashMap::new();
        let l = Lookup { rendered: &r, redirects: &redirects };
        let res = |w: &str, st: bool| {
            let (mut f, mut m) = (Vec::new(), Vec::new());
            l.resolve(w, st, &mut f, &mut m);
            (f, m)
        };
        assert_eq!(res("This", true).0, ["this"]);
        assert_eq!(res("English", false).0, ["English"]);
        assert_eq!(res("About", false).0, ["about"]);
        assert_eq!(res("open-source", false).0, ["open-source"]);
        assert_eq!(res("closed-source", false).0, ["closed", "source"]);
        assert_eq!(res("owner's", false).0, ["owner", "'s"]);
        assert_eq!(res("e.g.", false).0, ["e.g."]);
        assert_eq!(res("work.", false).0, ["work"]);
        assert_eq!(res("Soclets", false), (vec![], vec!["Soclets".to_string()]));
    }

    #[test]
    fn tries_phrases_only_for_words_without_an_entry() {
        let r = rendered(&[("in", ""), ("Costa Rica", ""), ("vice", ""), ("vice versa", ""), ("and", ""), ("San Luis Obispo", ""), ("Luis Obispo", "")]);
        let redirects = HashMap::new();
        let l = Lookup { rendered: &r, redirects: &redirects };
        let c = close(&l, &["in Costa Rica and vice versa in San Luis Obispo".to_string()]);
        let entries: Vec<&str> = c.order.iter().filter_map(|i| match i { Item::Entry(t) => Some(*t), _ => None }).collect();
        assert_eq!(entries, ["in", "Costa Rica", "and", "vice", "vice versa", "San Luis Obispo"]);
    }

    #[test]
    fn closes_in_discovery_order() {
        let r = rendered(&[("a", "b c"), ("b", "c d a"), ("c", "a"), ("d", "e"), ("e", "a b")]);
        let redirects = HashMap::new();
        let l = Lookup { rendered: &r, redirects: &redirects };
        let c = close(&l, &["a x".to_string()]);
        let e = |t| Item::Entry(t);
        let n = |t: &str| Item::NoEntry(t.to_string());
        // "x" has no entry, so the note's words ("no", "entry", ...) follow it; none of them has an entry here.
        assert_eq!(
            c.order,
            [e("a"), n("x"), n("no"), n("entry"), n("in"), n("English"), n("Wiktionary"), n("noun"), e("b"), e("c"), e("d"), e("e")]
        );
        assert_eq!(c.missing.iter().find(|m| m.0 == "noun").unwrap().1, 5);
    }
}
