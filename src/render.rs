//! Turns a meaning's wiki markup into the plain words Wiktionary shows.
//!
//! Templates are expanded from the template pages in the dump, the way
//! MediaWiki does it (parameters, parser functions such as `#if` and
//! `#switch`, redirects). Where a template hands over to one of Wiktionary's
//! Lua programs (`#invoke`), a Rust version of that program's display rules
//! runs instead, reading Wiktionary's own data tables (labels, inflection
//! tags) from the dump. Programs without a Rust version get a plain fallback
//! (their arguments, joined), and every use of a fallback is counted.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, RwLock};

use crate::cache::Dictionary;
use crate::lua::{self, Value};

const MAX_DEPTH: u32 = 40;

const LINKING_WORDS: &[&str] = &["of", "in", "on", "near", "and", "or", "at", "between", "within", "off", "along", "across", "north", "south", "east", "west", "northern", "southern", "eastern", "western", "central"];

/// "obl/Chernihiv" or "starostynskyi okruh:suf/Pisky" as (type, name). A type is
/// plain words, so text that merely contains a slash is not a holonym.
fn holonym_parts(raw: &str) -> Option<(&str, &str)> {
    let (ty, name) = raw.split_once('/')?;
    let ok = !ty.is_empty() && ty.len() <= 40 && ty.chars().all(|c| c.is_alphabetic() || c == ' ' || c == ':' || c == '-');
    (ok && !name.trim().is_empty()).then_some((ty.trim(), name.trim()))
}

const THE_COUNTRIES: &[&str] = &[
    "United States", "United Kingdom", "Netherlands", "Philippines", "Bahamas", "Gambia", "Czech Republic",
    "Democratic Republic of the Congo", "Republic of the Congo", "Central African Republic", "Dominican Republic",
    "United Arab Emirates", "Maldives", "Marshall Islands", "Solomon Islands", "Comoros", "Seychelles",
];

/// Request and cleanup notices for Wiktionary's editors ("This term needs a
/// definition", "needs verification"). They are not part of any meaning, so
/// they show nothing; a meaning that was only such a notice ends up empty.
const MAINTENANCE: &[&str] = &[
    "rfdef", "rfex", "rfexample", "rfv-sense", "rfc-sense", "rfd-sense", "rfm-sense", "rfclarify", "rfquote",
    "rfquote-sense", "rfquotek", "rfref", "rfdate", "rfdatek", "rfi", "rfe", "rfv", "rfd", "rfc", "rfm",
    "rft-sense", "rfscript", "tea room sense", "tea room", "attention", "attn", "rfv-quote", "rfap", "rfpron",
    "rfinfl", "rfelite", "rfgloss", "rfusex", "rfquote-lite", "rfdef-sense", "rfdatek-lite", "rfscriptex",
];

// ---------------------------------------------------------------- parsing

#[derive(Debug, Clone)]
enum Node {
    Text(String),
    /// `{{...}}`, split at top-level `|`.
    Template(Vec<Vec<Node>>),
    /// `{{{...}}}`, a template parameter.
    Arg(Vec<Vec<Node>>),
    /// `[[...]]`, kept as a link; parsed only so its `|` does not split a template.
    Link(Vec<Vec<Node>>),
    /// An extension tag such as `<ref>...</ref>` or `<nowiki>...</nowiki>`:
    /// its `|`, `=` and braces belong to it, not to the template around it.
    Sealed(String),
}

const SEALED_TAGS: &[&str] = &["ref", "nowiki", "math", "pre", "syntaxhighlight", "chem", "ce", "gallery", "score", "templatestyles"];

/// The length of a sealed tag starting at the beginning of `rest`, if there is one.
fn sealed_len(rest: &[u8]) -> Option<usize> {
    let lower: Vec<u8> = rest.iter().take(20).map(|b| b.to_ascii_lowercase()).collect();
    let name = SEALED_TAGS.iter().find(|n| {
        lower.get(1..1 + n.len()) == Some(n.as_bytes()) && matches!(lower.get(1 + n.len()), Some(b' ' | b'>' | b'/' | b'\t' | b'\n'))
    })?;
    let open_end = memchr::memchr(b'>', rest)?;
    if rest[open_end - 1] == b'/' {
        return Some(open_end + 1);
    }
    let close = format!("</{name}>");
    let hay: Vec<u8> = rest.iter().map(|b| b.to_ascii_lowercase()).collect();
    Some(memchr::memmem::find(&hay[open_end..], close.as_bytes()).map_or(rest.len(), |e| open_end + e + close.len()))
}

fn parse(s: &str) -> Vec<Node> {
    let mut i = 0;
    let (mut parts, _) = parse_until(s.as_bytes(), &mut i, None);
    parts.pop().unwrap_or_default()
}

fn push_text(out: &mut Vec<Node>, s: &str) {
    if let Some(Node::Text(t)) = out.last_mut() {
        t.push_str(s);
    } else if !s.is_empty() {
        out.push(Node::Text(s.to_string()));
    }
}

/// Parses until `closer` (or the end when None). Returns the parts split at
/// top-level `|` (only when there is a closer) and whether the closer was found.
fn parse_until(s: &[u8], i: &mut usize, closer: Option<&[u8]>) -> (Vec<Vec<Node>>, bool) {
    let text = |a: usize, b: usize| std::str::from_utf8(&s[a..b]).unwrap();
    let mut parts = vec![Vec::new()];
    let mut start = *i;
    while *i < s.len() {
        let rest = &s[*i..];
        if let Some(c) = closer {
            if rest.starts_with(c) {
                push_text(parts.last_mut().unwrap(), text(start, *i));
                *i += c.len();
                return (parts, true);
            }
            if rest[0] == b'|' {
                push_text(parts.last_mut().unwrap(), text(start, *i));
                parts.push(Vec::new());
                *i += 1;
                start = *i;
                continue;
            }
        }
        if rest[0] == b'<' {
            if let Some(len) = sealed_len(rest) {
                push_text(parts.last_mut().unwrap(), text(start, *i));
                parts.last_mut().unwrap().push(Node::Sealed(text(*i, *i + len).to_string()));
                *i += len;
                start = *i;
                continue;
            }
        }
        let opener = if rest.starts_with(b"{{{") {
            Some((3, &b"}}}"[..]))
        } else if rest.starts_with(b"{{") {
            Some((2, &b"}}"[..]))
        } else if rest.starts_with(b"[[") {
            Some((2, &b"]]"[..]))
        } else {
            None
        };
        let Some((len, close)) = opener else {
            *i += 1;
            continue;
        };
        let before = *i;
        let mut j = *i + len;
        let (inner, closed) = parse_until(s, &mut j, Some(close));
        // `{{{` that does not close as a parameter may still be `{` + a template.
        let (inner, closed, j) = if !closed && len == 3 {
            let mut k = *i + 1;
            let (inner2, closed2) = parse_until(s, &mut k, Some(b"}}"));
            if closed2 {
                // Treat the first `{` as text and the rest as a template.
                push_text(parts.last_mut().unwrap(), text(start, before + 1));
                parts.last_mut().unwrap().push(Node::Template(inner2));
                *i = k;
                start = k;
                continue;
            }
            (inner, false, j)
        } else {
            (inner, closed, j)
        };
        // A link target cannot contain brackets ("[[roof], a [[lean-to]]" is not a link to "roof], a [[lean-to").
        let bad_link = close == b"]]" && inner.first().is_some_and(|t| t.iter().any(|n| matches!(n, Node::Text(x) if x.contains(['[', ']', '\n']))));
        if !closed || bad_link {
            *i += 1;
            continue;
        }
        push_text(parts.last_mut().unwrap(), text(start, before));
        parts.last_mut().unwrap().push(match close {
            b"}}}" => Node::Arg(inner),
            b"}}" => Node::Template(inner),
            _ => Node::Link(inner),
        });
        *i = j;
        start = j;
    }
    push_text(parts.last_mut().unwrap(), text(start, s.len()));
    (parts, closer.is_none())
}

/// Splits a template part at its first top-level `=` into (name, value).
fn split_named(part: &[Node]) -> Option<(Vec<Node>, Vec<Node>)> {
    for (n, node) in part.iter().enumerate() {
        if let Node::Text(t) = node {
            if let Some(eq) = t.find('=') {
                let mut name = part[..n].to_vec();
                push_text(&mut name, &t[..eq]);
                let mut value = Vec::new();
                push_text(&mut value, &t[eq + 1..]);
                value.extend_from_slice(&part[n + 1..]);
                return Some((name, value));
            }
        }
    }
    None
}

/// What MediaWiki keeps of a page when it is transcluded.
fn transcluded(src: &str) -> String {
    let src = crate::wikitext::strip_comments(src);
    let mut s = src.as_ref();
    let only: String;
    if s.contains("<onlyinclude>") {
        let mut acc = String::new();
        let mut rest = s;
        while let Some(a) = rest.find("<onlyinclude>") {
            rest = &rest[a + "<onlyinclude>".len()..];
            let b = rest.find("</onlyinclude>").unwrap_or(rest.len());
            acc.push_str(&rest[..b]);
            rest = &rest[b..];
        }
        only = acc;
        s = &only;
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(a) = rest.find("<noinclude>") {
        out.push_str(&rest[..a]);
        rest = &rest[a..];
        match rest.find("</noinclude>") {
            Some(b) => rest = &rest[b + "</noinclude>".len()..],
            None => rest = "",
        }
    }
    out.push_str(rest);
    out.replace("<includeonly>", "").replace("</includeonly>", "").replace("<noinclude/>", "").replace("<noinclude />", "")
}

// ---------------------------------------------------------------- data

#[derive(Debug, Clone, Default)]
struct Label {
    display: Option<String>,
    omit_pre_comma: bool,
    omit_post_comma: bool,
    omit_pre_space: bool,
    omit_post_space: bool,
}

#[derive(Debug, Clone)]
struct Tag {
    display: String,
}

#[derive(Default)]
pub struct Stats {
    /// Lua programs with no Rust version, by "module|function", and uses.
    pub fallback_invokes: BTreeMap<String, u64>,
    /// The first entry seen using each fallback, for checking by hand.
    pub fallback_samples: BTreeMap<String, String>,
    /// Templates that have no page in the dump.
    pub missing_templates: BTreeMap<String, u64>,
    pub depth_exceeded: u64,
    /// Editor notices left out, by template.
    pub maintenance: BTreeMap<String, u64>,
    /// Data-table statements that were not plain literals, by module.
    pub skipped_data: BTreeMap<String, usize>,
}

pub struct Renderer<'a> {
    dict: &'a Dictionary,
    labels: HashMap<String, Label>,
    label_alias: HashMap<String, String>,
    tags: HashMap<String, Tag>,
    tag_shortcuts: HashMap<String, Vec<String>>,
    /// Language and etymology-language codes to their names ("fr" -> "French").
    languages: HashMap<String, String>,
    /// Place-type shorthands to their names ("ucomm" -> "unincorporated community").
    placetype_aliases: HashMap<String, String>,
    parsed: RwLock<HashMap<String, Option<Arc<Vec<Node>>>>>,
    pub stats: Mutex<Stats>,
}

type Args = HashMap<String, String>;

struct Frame<'f> {
    args: Args,
    title: &'f str,
}

fn arg<'x>(a: &'x Args, k: &str) -> Option<&'x str> {
    a.get(k).map(String::as_str).filter(|v| !v.trim().is_empty())
}

fn positional(a: &Args, from: usize) -> Vec<&str> {
    (from..).map_while(|n| a.get(&n.to_string()).map(String::as_str)).filter(|v| !v.trim().is_empty()).collect()
}

fn ucfirst(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

fn lcfirst(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_lowercase().chain(c).collect()).unwrap_or_default()
}

fn article(next: &str) -> &'static str {
    match next.trim_start_matches(['[', '\'']).chars().next().map(|c| c.to_ascii_lowercase()) {
        Some('a' | 'e' | 'i' | 'o' | 'u') => "An",
        _ => "A",
    }
}

/// Joins terms as Wiktionary does: "a", "a or b", "a, b or c".
fn join_terms(terms: &[String], conj: &str) -> String {
    match terms {
        [] => String::new(),
        [a] => a.clone(),
        [init @ .., last] => format!("{} {conj} {last}", init.join(", ")),
    }
}

impl<'a> Renderer<'a> {
    pub fn new(dict: &'a Dictionary) -> Self {
        let mut r = Renderer {
            dict,
            labels: HashMap::new(),
            label_alias: HashMap::new(),
            tags: HashMap::new(),
            tag_shortcuts: HashMap::new(),
            languages: HashMap::new(),
            placetype_aliases: HashMap::new(),
            parsed: RwLock::new(HashMap::new()),
            stats: Mutex::new(Stats::default()),
        };
        r.load_labels();
        r.load_tags();
        r.load_languages();
        if let Some(Value::Table(t)) = r.module_source("place/placetypes").and_then(|src| lua::assigned_table(src, "export.placetype_aliases")) {
            r.placetype_aliases = t.into_iter().filter_map(|(k, v)| Some((k?, v.as_str()?.to_string()))).collect();
        }
        r
    }

    /// A place type's name; shorthands are expanded word by word, so "rural ucomm"
    /// reads "rural unincorporated community".
    fn placetype(&self, t: &str) -> String {
        if let Some(full) = self.placetype_aliases.get(t) {
            return full.clone();
        }
        t.split(' ')
            .map(|w| match w {
                // Module:place's own text for this one, which is not in the alias table.
                "caplc" => "capital and largest city".to_string(),
                _ => self.placetype_aliases.get(w).cloned().unwrap_or_else(|| w.to_string()),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn load_languages(&mut self) {
        let mut modules: Vec<&String> = self
            .dict
            .pages
            .keys()
            .filter(|t| {
                (t.starts_with("Module:languages/data/") || t.starts_with("Module:etymology languages/data") || t.starts_with("Module:families/data"))
                    && !t.ends_with("/extra")
                    && !t.ends_with("/documentation")
            })
            .collect();
        modules.sort();
        for title in modules {
            let src = &self.dict.pages[title].text;
            let (entries, skipped) = lua::assignments(src, "m");
            if entries.is_empty() {
                continue;
            }
            self.stats.get_mut().unwrap().skipped_data.insert(title.clone(), skipped);
            for (code, v) in entries {
                if let Some(name) = v.positional().first().and_then(|n| n.as_str()) {
                    self.languages.entry(code).or_insert_with(|| name.to_string());
                }
            }
        }
    }

    fn module_source(&self, name: &str) -> Option<&'a str> {
        self.dict.pages.get(&format!("Module:{name}")).map(|p| p.text.as_str())
    }

    fn load_labels(&mut self) {
        // Module:labels looks in the language's own data first, then the shared data.
        for module in ["labels/data/lang/en", "labels/data", "labels/data/qualifiers", "labels/data/regional", "labels/data/topical"] {
            let Some(src) = self.module_source(module) else { continue };
            let (entries, skipped) = lua::assignments(src, "labels");
            self.stats.get_mut().unwrap().skipped_data.insert(format!("Module:{module}"), skipped);
            for (name, v) in entries {
                if self.labels.contains_key(&name) || self.label_alias.contains_key(&name) {
                    continue;
                }
                if let Value::Str(target) = &v {
                    self.label_alias.insert(name, target.clone());
                    continue;
                }
                for alias in v.get("aliases").map(Value::strings).unwrap_or_default() {
                    self.label_alias.entry(alias.to_string()).or_insert_with(|| name.clone());
                }
                let label = Label {
                    display: v.get("display").and_then(Value::as_str).map(str::to_string),
                    omit_pre_comma: v.truthy("omit_preComma"),
                    omit_post_comma: v.truthy("omit_postComma"),
                    omit_pre_space: v.truthy("omit_preSpace"),
                    omit_post_space: v.truthy("omit_postSpace"),
                };
                self.labels.insert(name, label);
            }
        }
    }

    fn load_tags(&mut self) {
        for module in ["form of/lang-data/en", "form of/data/1", "form of/data/2"] {
            let Some(src) = self.module_source(module) else { continue };
            let (tags, skipped_t) = lua::assignments(src, "tags");
            let (shortcuts, skipped_s) = lua::assignments(src, "shortcuts");
            self.stats.get_mut().unwrap().skipped_data.insert(format!("Module:{module}"), skipped_t + skipped_s);
            for (name, v) in tags {
                let pos = v.positional();
                for short in pos.get(2).map(|s| s.strings()).unwrap_or_default() {
                    self.tag_shortcuts.entry(short.to_string()).or_insert_with(|| vec![name.clone()]);
                }
                let display = v.get("display").and_then(Value::as_str).unwrap_or(&name).to_string();
                self.tags.entry(name).or_insert(Tag { display });
            }
            for (name, v) in shortcuts {
                let expansion: Vec<String> = v.strings().into_iter().map(str::to_string).collect();
                if !expansion.is_empty() {
                    self.tag_shortcuts.entry(name).or_insert(expansion);
                }
            }
        }
    }

    fn template_body(&self, name: &str) -> Option<Arc<Vec<Node>>> {
        if let Some(hit) = self.parsed.read().unwrap().get(name) {
            return hit.clone();
        }
        let mut title = name.to_string();
        let mut found = None;
        for _ in 0..8 {
            match self.dict.pages.get(&title) {
                Some(p) => match &p.redirect {
                    Some(to) => title = to.clone(),
                    None => {
                        found = Some(Arc::new(parse(&transcluded(&p.text))));
                        break;
                    }
                },
                None => break,
            }
        }
        self.parsed.write().unwrap().insert(name.to_string(), found.clone());
        found
    }

    fn page_exists(&self, title: &str) -> bool {
        let t = title.trim();
        self.dict.entries.contains_key(t) || self.dict.redirects.contains_key(t) || self.dict.pages.contains_key(t)
    }

    // ------------------------------------------------------------ expansion

    fn expand(&self, nodes: &[Node], frame: &Frame, depth: u32) -> String {
        let mut out = String::new();
        for node in nodes {
            match node {
                Node::Text(t) => out.push_str(t),
                Node::Arg(parts) => {
                    let name = self.expand(&parts[0], frame, depth);
                    match frame.args.get(name.trim()) {
                        Some(v) => out.push_str(v),
                        None if parts.len() > 1 => out.push_str(&self.expand(&parts[1], frame, depth)),
                        None => {
                            out.push_str("{{{");
                            out.push_str(&name);
                            out.push_str("}}}");
                        }
                    }
                }
                Node::Link(parts) => {
                    out.push_str("[[");
                    for (n, p) in parts.iter().enumerate() {
                        if n > 0 {
                            out.push('|');
                        }
                        out.push_str(&self.expand(p, frame, depth));
                    }
                    out.push_str("]]");
                }
                Node::Template(parts) => out.push_str(&self.template(parts, frame, depth)),
                Node::Sealed(t) => out.push_str(t),
            }
        }
        out
    }

    fn call_args(&self, parts: &[Vec<Node>], frame: &Frame, depth: u32) -> Args {
        let mut args = Args::new();
        let mut n = 0;
        for part in parts {
            match split_named(part) {
                Some((k, v)) => {
                    let k = self.expand(&k, frame, depth).trim().to_string();
                    args.insert(k, self.expand(&v, frame, depth).trim().to_string());
                }
                None => {
                    n += 1;
                    args.insert(n.to_string(), self.expand(part, frame, depth));
                }
            }
        }
        args
    }

    fn template(&self, parts: &[Vec<Node>], frame: &Frame, depth: u32) -> String {
        if depth > MAX_DEPTH {
            self.stats.lock().unwrap().depth_exceeded += 1;
            return String::new();
        }
        let head = self.expand(&parts[0], frame, depth);
        let mut head = head.trim();
        for prefix in ["safesubst:", "subst:", "SAFESUBST:", "SUBST:"] {
            head = head.strip_prefix(prefix).unwrap_or(head).trim_start();
        }
        let rest = &parts[1..];
        let lazy = |n: usize| rest.get(n).map(|p| self.expand(p, frame, depth).trim().to_string()).unwrap_or_default();

        if let Some(func) = head.strip_prefix('#') {
            let (fname, first) = func.split_once(':').unwrap_or((func, ""));
            let first = first.trim();
            return match fname.trim().to_ascii_lowercase().as_str() {
                "if" => {
                    if first.is_empty() { lazy(1) } else { lazy(0) }
                }
                "ifeq" => {
                    let b = lazy(0);
                    let eq = match (first.parse::<f64>(), b.parse::<f64>()) {
                        (Ok(x), Ok(y)) => x == y,
                        _ => first == b,
                    };
                    if eq { lazy(1) } else { lazy(2) }
                }
                "ifexist" => {
                    if self.page_exists(first) { lazy(0) } else { lazy(1) }
                }
                "switch" => {
                    let mut matched = false;
                    let mut default = None;
                    for (n, part) in rest.iter().enumerate() {
                        match split_named(part) {
                            Some((k, v)) => {
                                let k = self.expand(&k, frame, depth);
                                let k = k.trim();
                                if matched || k == first {
                                    return self.expand(&v, frame, depth).trim().to_string();
                                }
                                if k == "#default" {
                                    default = Some(v);
                                }
                            }
                            None => {
                                let k = self.expand(part, frame, depth);
                                if n + 1 == rest.len() {
                                    return k.trim().to_string();
                                }
                                if k.trim() == first {
                                    matched = true;
                                }
                            }
                        }
                    }
                    default.map(|v| self.expand(&v, frame, depth).trim().to_string()).unwrap_or_default()
                }
                "invoke" => {
                    let function = lazy(0);
                    let iargs = self.call_args(&rest[1.min(rest.len())..], frame, depth);
                    self.invoke(first, &function, &iargs, frame, depth)
                }
                "tag" => {
                    if first.eq_ignore_ascii_case("ref") { String::new() } else { lazy(0) }
                }
                _ => String::new(),
            };
        }
        if let Some((fname, value)) = head.split_once(':') {
            let value = value.trim();
            match fname {
                "lc" => return value.to_lowercase(),
                "uc" => return value.to_uppercase(),
                "lcfirst" => return lcfirst(value),
                "ucfirst" => return ucfirst(value),
                "urlencode" | "anchorencode" | "ns" | "padleft" | "padright" | "plural" | "formatnum" => return value.to_string(),
                _ => {}
            }
        }
        let lower_head = head.to_ascii_lowercase();
        if ["fullurl:", "fullurle:", "localurl:", "localurle:", "canonicalurl:", "filepath:"].iter().any(|p| lower_head.starts_with(p)) {
            return String::new();
        }
        match head {
            // Date words read the dump's date, so every run gives the same text.
            "CURRENTYEAR" | "LOCALYEAR" | "REVISIONYEAR" => return "2026".to_string(),
            "CURRENTMONTH" | "LOCALMONTH" | "REVISIONMONTH" => return "10".to_string(),
            "CURRENTMONTHNAME" | "LOCALMONTHNAME" => return "October".to_string(),
            "CURRENTDAY" | "LOCALDAY" | "REVISIONDAY" => return "1".to_string(),
            "REVISIONUSER" => return String::new(),
            "!" => return "|".to_string(),
            "=" => return "=".to_string(),
            "PAGENAME" | "FULLPAGENAME" | "SUBPAGENAME" | "BASEPAGENAME" | "PAGENAMEE" => return frame.title.to_string(),
            "NAMESPACE" => return String::new(),
            _ => {}
        }

        let name = head.replace('_', " ");
        let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
        if MAINTENANCE.contains(&name.as_str()) {
            *self.stats.lock().unwrap().maintenance.entry(name).or_default() += 1;
            return String::new();
        }
        let full = match name.split_once(':') {
            Some((ns, _)) if ["Template", "Module", "Wiktionary", "Appendix"].contains(&ns) => name.clone(),
            // Wiktionary titles keep their first letter as written.
            _ => format!("Template:{name}"),
        };
        let Some(body) = self.template_body(&full) else {
            *self.stats.lock().unwrap().missing_templates.entry(full).or_default() += 1;
            return String::new();
        };
        let child = Frame { args: self.call_args(rest, frame, depth), title: frame.title };
        self.expand(&body, &child, depth + 1)
    }

    // ------------------------------------------------------------ Lua programs

    fn fallback(&self, key: &str, pargs: &Args, title: &str) -> String {
        let mut stats = self.stats.lock().unwrap();
        *stats.fallback_invokes.entry(key.to_string()).or_default() += 1;
        stats.fallback_samples.entry(key.to_string()).or_insert_with(|| title.to_string());
        drop(stats);
        positional(pargs, 2).join(", ")
    }

    fn invoke(&self, module: &str, function: &str, iargs: &Args, parent: &Frame, depth: u32) -> String {
        let p = &parent.args;
        let key = format!("{module}|{function}");
        match key.as_str() {
            "labels/templates|show" => {
                let shown = self.show_labels(&positional(p, 2));
                if shown.contains("{{") { self.expand(&parse(&shown), parent, depth + 1) } else { shown }
            }
            "qualifier/templates|qualifier_t" => self.show_qualifiers(&positional(p, 1)),
            "labels/templates/show_from|show_from" => match arg(p, "from") {
                Some(from) => {
                    let shown = from.split(',').map(|l| self.label_text(l.trim())).collect::<Vec<_>>().join(" and ");
                    if shown.contains("{{") { self.expand(&parse(&shown), parent, depth + 1) } else { shown }
                }
                None => arg(iargs, "default").unwrap_or("").to_string(),
            },
            "form of/templates|form_of_t" => {
                let text = arg(iargs, "1").unwrap_or("form of").to_string();
                self.form_of(&text, iargs, p, true)
            }
            "form of/templates|tagged_form_of_t" => {
                let tags = positional(iargs, 1).iter().map(|t| t.to_string()).collect::<Vec<_>>();
                let text = format!("{} of", self.tag_text(&tags));
                self.form_of(&text, iargs, p, true)
            }
            "form of/templates|inflection_of_t" => {
                let term_param = self.term_param(iargs, p);
                let tags: Vec<String> = positional(p, term_param + 2).iter().map(|t| t.to_string()).collect();
                let text = format!("{} of", self.tag_sets_text(&tags));
                self.form_of(&text, iargs, p, false)
            }
            "links/templates|l_term_t" | "links/templates|ll" => self.link_term(p, 2),
            "links/templates|def_t" => {
                let text = arg(p, "1").unwrap_or("").to_string();
                if arg(iargs, "face") == Some("gloss") { format!("({text})") } else { text }
            }
            "anchors/templates|senseid_t" | "anchors/templates|etymid_t" | "utilities/templates|categorize" | "checkparams|error" => String::new(),
            "debug/templates|track" | "checkparams|warn" | "request-forum|rfv" | "request-forum|rfd" | "attention|show"
            | "quote|call_template" | "quote|cite_t" | "quote|call_quote_template" | "check isxn|check_isbn"
            | "interproject|wikipedia_box" | "anchors/templates|anchor_t" => String::new(),
            "string/templates|find" => {
                let (src, target) = (iargs.get("1").map(String::as_str).unwrap_or(""), iargs.get("2").map(String::as_str).unwrap_or(""));
                let plain = arg(iargs, "4").is_some() || !target.contains(['^', '$', '(', ')', '%', '.', '[', ']', '*', '+', '?']) || target == "-";
                if !plain {
                    return self.fallback("string/templates|find (pattern)", p, parent.title);
                }
                let start = arg(iargs, "3").and_then(|n| n.parse::<usize>().ok()).unwrap_or(1).max(1) - 1;
                let byte_start = src.char_indices().nth(start).map(|(b, _)| b).unwrap_or(src.len());
                match src[byte_start..].find(target) {
                    Some(b) if arg(iargs, "5").is_some() => (src[..byte_start + b].chars().count() + target.chars().count()).to_string(),
                    Some(b) => (src[..byte_start + b].chars().count() + 1).to_string(),
                    None => String::new(),
                }
            }
            "string/templates|replace" => {
                let src = iargs.get("1").map(String::as_str).unwrap_or("");
                let (from, to) = (iargs.get("2").map(String::as_str).unwrap_or(""), iargs.get("3").map(String::as_str).unwrap_or(""));
                let plain = iargs.get("plain").is_none_or(|v| v != "false") && !from.contains(['%', '[', '(', '.', '*', '+', '-', '?', '^', '$']);
                if from.is_empty() || !plain {
                    return self.fallback("string/templates|replace (pattern)", p, parent.title);
                }
                src.replace(from, to)
            }
            "string/templates|len" => iargs.get("1").map(|s| s.chars().count()).unwrap_or(0).to_string(),
            "links/templates|cap_t" => format!("{}{}", ucfirst(arg(p, "1").unwrap_or("")), p.get("2").map(String::as_str).unwrap_or("")),
            "pages/templates|pagename_t" => parent.title.to_string(),
            "languages/templates|getByCodeAllowEtym" | "languages/templates|getByCode" | "languages/templates|getCanonicalName" => {
                let code = arg(iargs, "1").unwrap_or("");
                match (self.languages.get(code), arg(iargs, "2")) {
                    (Some(_), Some("getCode")) => code.to_string(),
                    (Some(name), _) => name.clone(),
                    (None, _) => String::new(),
                }
            }
            "IPA/templates|IPAchar" => positional(p, 1).join(", "),
            "script utilities|lang_t" => arg(p, "2").unwrap_or("").to_string(),
            "chemical formula|chem" => positional(p, 1).concat(),
            "demonym/templates|demonym_noun" => {
                self.count_simplified("demonym/templates|demonym_noun (simplified)");
                format!("A native or inhabitant of {}", self.link_term(p, 2))
            }
            "demonym/templates|demonym_adj" => {
                self.count_simplified("demonym/templates|demonym_adj (simplified)");
                format!("Of, from or relating to {}", self.link_term(p, 2))
            }
            "glossary|link" => arg(p, "2").or(arg(p, "1")).unwrap_or("").to_string(),
            "taxlink|taxlink" => arg(p, "3").or(arg(p, "1")).unwrap_or("").to_string(),
            "taxlink|taxfmt" => arg(p, "2").or(arg(p, "1")).unwrap_or("").to_string(),
            "names|surname" => self.surname(iargs, p),
            "names|given_name" => self.given_name(p),
            "place|show" => self.place(p, parent.title),
            "definition/templates|and_lit_t" => {
                let terms: Vec<String> = positional(p, 2).iter().map(|t| t.to_string()).collect();
                if terms.is_empty() {
                    "Used other than figuratively or idiomatically".to_string()
                } else {
                    format!("Used other than figuratively or idiomatically: see {}", join_terms(&terms, "and"))
                }
            }
            _ => self.fallback(&key, p, parent.title),
        }
    }

    fn count_simplified(&self, key: &str) {
        *self.stats.lock().unwrap().fallback_invokes.entry(key.to_string()).or_default() += 1;
    }

    fn label_text(&self, label: &str) -> String {
        let canonical = self.label_alias.get(label).map(String::as_str).unwrap_or(label);
        match self.labels.get(canonical).and_then(|l| l.display.clone()) {
            Some(d) => d,
            None => canonical.to_string(),
        }
    }

    /// Module:labels show_labels: "(a, b)", with connector labels such as "_",
    /// "or" and "chiefly" suppressing the comma around them.
    fn show_labels(&self, labels: &[&str]) -> String {
        let mut out = String::from("(");
        let (mut omit_post_comma, mut omit_post_space) = (true, true);
        for &raw in labels {
            let canonical = self.label_alias.get(raw).map(String::as_str).unwrap_or(raw);
            let data = self.labels.get(canonical).cloned().unwrap_or_default();
            let text = data.display.clone().unwrap_or_else(|| canonical.to_string());
            let omit_comma = omit_post_comma || data.omit_pre_comma;
            let omit_space = omit_post_space || data.omit_pre_space;
            omit_post_comma = data.omit_post_comma;
            omit_post_space = data.omit_post_space;
            if text.is_empty() && raw != "_" {
                continue;
            }
            if !omit_comma {
                out.push(',');
            }
            if !omit_space {
                out.push(' ');
            }
            out.push_str(&text);
        }
        out.push(')');
        out
    }

    fn show_qualifiers(&self, qualifiers: &[&str]) -> String {
        if qualifiers.is_empty() {
            return String::new();
        }
        format!("({})", qualifiers.join(", "))
    }

    fn term_param(&self, iargs: &Args, p: &Args) -> usize {
        arg(iargs, "term_param").and_then(|t| t.parse().ok()).unwrap_or(if arg(iargs, "lang").is_some() || arg(p, "lang").is_some() { 1 } else { 2 })
    }

    /// The words for a list of inflection tags, expanding shortcuts ("p" -> plural).
    fn tag_text(&self, tags: &[String]) -> String {
        let mut words = Vec::new();
        for t in tags {
            let expanded: Vec<String> = match self.tag_shortcuts.get(t.as_str()) {
                Some(e) => e.clone(),
                None => vec![t.clone()],
            };
            for e in expanded {
                if e.contains("//") {
                    let alts: Vec<String> = e.split("//").map(|x| self.tag_text(&[x.to_string()])).collect();
                    words.push(alts.join("/"));
                    continue;
                }
                let name = match self.tag_shortcuts.get(e.as_str()) {
                    Some(inner) if inner.len() == 1 && inner[0] != e => inner[0].clone(),
                    Some(inner) if inner.len() > 1 => {
                        words.push(self.tag_text(inner));
                        continue;
                    }
                    _ => e.clone(),
                };
                let display = self.tags.get(&name).map(|t| t.display.clone()).unwrap_or(name);
                words.push(display);
            }
        }
        words.join(" ")
    }

    /// Tag sets separated by ";" are joined with "and".
    fn tag_sets_text(&self, tags: &[String]) -> String {
        let sets: Vec<String> = tags
            .split(|t| t == ";")
            .filter(|s| !s.is_empty())
            .map(|s| self.tag_text(s))
            .collect();
        join_terms(&sets, "and")
    }

    /// A linked term as Wiktionary shows it: the term (or its display form),
    /// then a gloss in curly quotes.
    fn term_with_gloss(&self, term: &str, alt: Option<&str>, gloss: Option<&str>) -> String {
        let shown = alt.unwrap_or(term);
        let shown = match shown.split_once('#') {
            Some((base, _)) if !base.is_empty() && !shown.contains("[[") => base,
            _ => shown,
        };
        let shown = shown.trim_start_matches(['*', '^']);
        match gloss {
            Some(g) => format!("{shown} (“{g}”)"),
            None => shown.to_string(),
        }
    }

    /// Splits a term parameter into terms, removing inline modifiers like `<t:gloss>`.
    fn terms(&self, value: &str) -> Vec<(String, Option<String>)> {
        let mut out = Vec::new();
        let mut depth = 0;
        let mut cur = String::new();
        for c in value.chars() {
            match c {
                '<' => depth += 1,
                '>' if depth > 0 => depth -= 1,
                ',' if depth == 0 && value.contains('<') => {
                    out.push(std::mem::take(&mut cur));
                    continue;
                }
                _ => {}
            }
            cur.push(c);
        }
        out.push(cur);
        out.into_iter()
            .map(|t| {
                let mut gloss = None;
                let mut base = String::new();
                let mut rest = t.as_str();
                while let Some(a) = rest.find('<') {
                    base.push_str(&rest[..a]);
                    let b = rest[a..].find('>').map(|b| a + b).unwrap_or(rest.len() - 1);
                    let modifier = &rest[a + 1..b];
                    if let Some(g) = modifier.strip_prefix("t:").or(modifier.strip_prefix("gloss:")) {
                        gloss = Some(g.to_string());
                    }
                    rest = &rest[(b + 1).min(rest.len())..];
                }
                base.push_str(rest);
                (base.trim().to_string(), gloss)
            })
            .filter(|(t, _)| !t.is_empty())
            .collect()
    }

    fn link_term(&self, p: &Args, term_param: usize) -> String {
        let term = arg(p, &term_param.to_string()).unwrap_or("");
        let alt = arg(p, &(term_param + 1).to_string()).or(arg(p, "alt"));
        let gloss = arg(p, &(term_param + 2).to_string()).or(arg(p, "t")).or(arg(p, "gloss"));
        let mut s = self.term_with_gloss(term, alt, gloss);
        if let Some(lit) = arg(p, "lit") {
            s.push_str(&format!(" (literally, “{lit}”)"));
        }
        s
    }

    /// Module:form of: "<text> <term>", optionally capitalised and with a final dot.
    fn form_of(&self, text: &str, iargs: &Args, p: &Args, numbered_gloss: bool) -> String {
        let term_param = self.term_param(iargs, p);
        let raw = arg(p, &term_param.to_string()).unwrap_or("");
        let alt = arg(p, &(term_param + 1).to_string()).or(arg(p, "alt"));
        // inflection_of_t uses the numbered slots after the term for tags, not a gloss.
        let numbered_gloss = if numbered_gloss { arg(p, &(term_param + 2).to_string()) } else { None };
        let gloss = arg(p, "t").or(arg(p, "gloss")).or(numbered_gloss);
        let terms = self.terms(raw);
        let shown: Vec<String> = if terms.len() > 1 {
            terms.iter().map(|(t, g)| self.term_with_gloss(t, None, g.as_deref())).collect()
        } else {
            let (t, g) = terms.into_iter().next().unwrap_or_default();
            vec![self.term_with_gloss(&t, alt, g.as_deref().or(gloss))]
        };
        let conj = arg(iargs, "conj").unwrap_or("and");
        let mut out = String::new();
        let cap = arg(iargs, "withcap").is_some() || arg(p, "cap").is_some();
        out.push_str(&if cap { ucfirst(text) } else { text.to_string() });
        out.push(' ');
        out.push_str(&join_terms(&shown, conj));
        if let Some(addl) = arg(p, "addl") {
            match addl.chars().next() {
                Some(';' | ':') => out.push_str(addl),
                Some('_') => {
                    out.push(' ');
                    out.push_str(&addl[1..]);
                }
                _ => {
                    out.push_str(", ");
                    out.push_str(addl);
                }
            }
        }
        if arg(p, "nodot").is_none() {
            if let Some(dot) = arg(p, "dot") {
                out.push_str(dot);
            } else if arg(iargs, "withdot").is_some() {
                out.push('.');
            }
        }
        out
    }

    fn from_text(&self, p: &Args) -> String {
        let froms: Vec<String> = ["from", "from2", "from3"].iter().filter_map(|k| arg(p, k)).map(|f| f.replace(':', " ").to_string()).collect();
        if froms.is_empty() { String::new() } else { format!(" from {}", join_terms(&froms, "or")) }
    }

    /// Module:names surname: "A surname", "An occupational surname from Old English".
    fn surname(&self, _iargs: &Args, p: &Args) -> String {
        let adj = arg(p, "2").map(|a| format!("{a} ")).unwrap_or_default();
        let noun = format!("{adj}surname");
        let a = if arg(p, "A").is_some() { arg(p, "A").unwrap().to_string() } else { article(&noun).to_string() };
        let mut s = format!("{a} {noun}{}", self.from_text(p));
        if let Some(eq) = arg(p, "eq") {
            s.push_str(&format!(", equivalent to {eq}"));
        }
        s
    }

    /// Module:names given_name: "A male given name from Hebrew".
    fn given_name(&self, p: &Args) -> String {
        let gender = arg(p, "2").unwrap_or("");
        let noun = if gender.is_empty() || gender == "unknown" { "given name".to_string() } else { format!("{} given name", gender.replace('/', " or ")) };
        let dim = if arg(p, "dim").is_some() || arg(p, "diminutive").is_some() { "diminutive of the " } else { "" };
        let noun = if dim.is_empty() { noun } else { format!("{dim}{noun}") };
        let a = arg(p, "A").map(str::to_string).unwrap_or_else(|| article(&noun).to_string());
        let mut s = format!("{a} {noun}{}", self.from_text(p));
        if let Some(eq) = arg(p, "eq") {
            s.push_str(&format!(", equivalent to {eq}"));
        }
        s
    }

    /// Module:place, simplified: "A village in Nizhyn Raion, Chernihiv Oblast,
    /// Ukraine, founded in 1720". Arguments are place types, then holonyms
    /// ("type/Name"), linking words ("of", ";") and free text, in order. The
    /// full module knows more types, articles and wording; this covers the
    /// common shapes and is counted as simplified.
    fn place(&self, p: &Args, title: &str) -> String {
        let all = positional(p, 2);
        // British counties, parishes and districts are named without "County" and the like.
        let british = all.iter().any(|h| match holonym_parts(h) {
            Some((ty, name)) => matches!((self.placetype(ty).as_str(), name), ("constituent country", "England" | "Scotland" | "Wales" | "Northern Ireland") | ("country", "UK" | "United Kingdom")),
            None => false,
        });
        let Some(first) = all.first() else { return self.fallback("place|show", p, title) };
        let mut s;
        let mut after_holonym;
        let mut need_in;
        if first.contains("<<") {
            // Inline style: "A coastal fishing <<village>> in the <<dist/Western Area>>".
            s = String::new();
            let mut rest = *first;
            while let Some(a) = rest.find("<<") {
                s.push_str(&rest[..a]);
                let b = rest[a..].find(">>").map_or(rest.len(), |b| a + b);
                let token = &rest[a + 2..b];
                match holonym_parts(token) {
                    Some((ty, name)) => s.push_str(&self.holonym(ty, name, british)),
                    None => s.push_str(&self.placetype(token.split(':').next().unwrap_or(token))),
                }
                rest = &rest[(b + 2).min(rest.len())..];
            }
            s.push_str(rest);
            after_holonym = true;
            need_in = false;
        } else {
            let types: Vec<String> = first.split('/').map(|t| self.placetype(t.trim().split(':').next().unwrap_or(""))).filter(|t| !t.is_empty()).collect();
            let kind = match types.as_slice() {
                [] => String::new(),
                [one] => one.clone(),
                [one, rest @ ..] => format!("{one}, the {}", rest.join(" and ")),
            };
            s = format!("{} {kind}", article(&kind));
            after_holonym = false;
            need_in = true;
            if types.len() > 1 {
                // "A village, the administrative centre of ..."
                s.push_str(" of");
                need_in = false;
            }
        }
        for raw in &all[1..] {
            let raw = raw.trim();
            let first_word = raw.split_whitespace().next().unwrap_or("");
            match holonym_parts(raw) {
                Some((ty, name)) => {
                    let mut name = self.holonym(ty, name, british);
                    if after_holonym {
                        s.push_str(", ");
                    } else if need_in {
                        s.push_str(" in ");
                        // Directly after "in", some country names take "the".
                        if THE_COUNTRIES.contains(&name.as_str()) {
                            name = format!("the {name}");
                        }
                    } else {
                        s.push(' ');
                    }
                    s.push_str(&name);
                    after_holonym = true;
                    need_in = false;
                }
                None if raw == ";" => {
                    s.push(';');
                    after_holonym = false;
                    need_in = false;
                }
                // A linking phrase ("of", "in southeast") leads straight into the next holonym.
                None if LINKING_WORDS.contains(&first_word) => {
                    s.push(' ');
                    s.push_str(raw);
                    after_holonym = false;
                    need_in = false;
                }
                None => {
                    s.push_str(if after_holonym { ", " } else { " " });
                    s.push_str(raw);
                    after_holonym = true;
                    need_in = false;
                }
            }
        }
        if let Some(t) = arg(p, "t").or(arg(p, "t1")) {
            s.push_str(&format!(" (“{t}”)"));
        }
        // Module:place's extra information, each as its own sentence ("Capital: Austin").
        for (key, text) in [
            ("modern", "Modern"), ("now", "Now"), ("full", "In full"), ("short", "Short form"), ("abbr", "Abbreviation"),
            ("former", "Formerly"), ("official", "Official name"), ("capital", "Capital"), ("largest city", "Largest city"),
            ("caplc", "Capital and largest city"), ("seat", "Seat"), ("shire town", "Shire town"),
            ("headquarters", "Headquarters"), ("center", "Administrative center"), ("centre", "Administrative centre"),
        ] {
            if let Some(v) = arg(p, key) {
                let values: Vec<String> = v.split(',').map(|x| x.trim().split_once('/').map_or(x.trim(), |(_, n)| n).to_string()).collect();
                s.push_str(&format!(". {text}: {}", join_terms(&values, "and")));
            }
        }
        self.count_simplified("place|show (simplified)");
        s
    }

    /// A holonym as shown in running text: "obl/Chernihiv" -> "Chernihiv Oblast".
    fn holonym(&self, ty: &str, name: &str, british: bool) -> String {
        let name = name.trim_start_matches(':');
        let (ty, suffix) = match ty.split_once(':') {
            Some((t, "suf")) => (t, true),
            Some((t, _)) => (t, false),
            None => (ty, false),
        };
        let full_ty = self.placetype(ty);
        let ty = full_ty.as_str();
        let shown = match (ty, name) {
            ("country", "USA" | "US" | "United States of America") => "United States".to_string(),
            ("country", "UK") => "United Kingdom".to_string(),
            _ => name.to_string(),
        };
        let fixed = match ty {
            "oblast" => Some("Oblast"),
            "raion" => Some("Raion"),
            "county" | "parish" | "district" if british => None,
            "county" => Some("County"),
            "parish" => Some("Parish"),
            "township" => Some("Township"),
            "urban hromada" | "uhrom" => Some("urban hromada"),
            "settlement hromada" | "shrom" => Some("settlement hromada"),
            "rural hromada" | "rhrom" => Some("rural hromada"),
            "krai" => Some("Krai"),
            "governorate" => Some("Governorate"),
            "municipality" if suffix => Some("Municipality"),
            _ => None,
        };
        match (fixed, suffix) {
            (Some(f), _) if !shown.ends_with(f) => format!("{shown} {f}"),
            (None, true) => format!("{shown} {ty}"),
            _ => shown,
        }
    }

    // ------------------------------------------------------------ entry point

    /// The plain text of one meaning line of the entry `title`.
    pub fn render(&self, title: &str, wikitext: &str) -> String {
        let frame = Frame { args: Args::new(), title };
        let expanded = self.expand(&parse(wikitext), &frame, 0);
        plain(&expanded)
    }
}

// ---------------------------------------------------------------- plain text

fn html_entity(name: &str) -> Option<char> {
    Some(match name {
        "nbsp" | "#160" | "#32" | "ensp" | "emsp" | "thinsp" => ' ',
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" | "#39" => '\'',
        "ndash" => '–',
        "mdash" => '—',
        "deg" => '°',
        "times" => '×',
        "hellip" => '…',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "middot" => '·',
        "minus" => '−',
        "frac12" => '½',
        "zwj" | "zwnj" | "shy" | "lrm" | "rlm" => '\u{200B}',
        _ => {
            let num = name.strip_prefix('#')?;
            let code = match num.strip_prefix(['x', 'X']) {
                Some(h) => u32::from_str_radix(h, 16).ok()?,
                None => num.parse().ok()?,
            };
            char::from_u32(code)?
        }
    })
}

/// Removes HTML tags (dropping `<ref>` contents), resolves links and entities,
/// removes bold/italic quote marks and tidies spaces.
fn plain(s: &str) -> String {
    // Tags.
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(a) = rest.find('<') {
        out.push_str(&rest[..a]);
        rest = &rest[a..];
        let Some(b) = rest.find('>') else {
            out.push_str(rest);
            rest = "";
            break;
        };
        let tag = &rest[1..b];
        let name: String = tag.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
        if name.is_empty() {
            out.push('<');
            rest = &rest[1..];
            continue;
        }
        rest = &rest[b + 1..];
        if name == "ref" && !tag.ends_with('/') && !tag.starts_with('/') {
            rest = match rest.find("</ref>") {
                Some(e) => &rest[e + "</ref>".len()..],
                None => "",
            };
        } else if name == "br" {
            out.push(' ');
        } else if (name == "sup" || name == "sub") && tag.starts_with('/') && rest.starts_with(|c: char| c.is_alphanumeric()) {
            // Raised or lowered text reads as separate from a letter right after it ("14<sup>th</sup>c." -> "14th c.").
            out.push(' ');
        }
    }
    out.push_str(rest);

    // Links.
    let mut s2 = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(a) = rest.find("[[") {
        s2.push_str(&rest[..a]);
        let Some(b) = rest[a..].find("]]") else {
            s2.push_str(&rest[a..]);
            rest = "";
            break;
        };
        let inner = &rest[a + 2..a + b];
        if inner.split('|').next().is_some_and(|t| t.contains(['[', ']', '\n'])) {
            s2.push_str("[[");
            rest = &rest[a + 2..];
            continue;
        }
        rest = &rest[a + b + 2..];
        let (target, label) = match inner.split_once('|') {
            Some((t, l)) => (t, Some(l)),
            None => (inner, None),
        };
        let t = target.trim();
        let lower = t.to_ascii_lowercase();
        if !t.starts_with(':') && ["category:", "file:", "image:"].iter().any(|p| lower.starts_with(p)) {
            continue;
        }
        let shown = match label {
            Some(l) => l.to_string(),
            None => {
                let t = t.trim_start_matches(':');
                // Interwiki prefixes such as w: are not shown.
                match t.split_once(':') {
                    Some((prefix, page)) if ["w", "wikipedia", "s", "wikisource", "species", "c", "commons", "en", "q"].contains(&prefix.to_ascii_lowercase().as_str()) => page.to_string(),
                    _ => t.to_string(),
                }
            }
        };
        s2.push_str(&shown);
    }
    s2.push_str(rest);

    // External links: [http://x label] -> label.
    let mut s3 = String::with_capacity(s2.len());
    let mut rest = s2.as_str();
    while let Some(a) = rest.find("[http") {
        s3.push_str(&rest[..a]);
        match rest[a..].find(']') {
            Some(b) => {
                let inner = &rest[a + 1..a + b];
                if let Some((_, label)) = inner.split_once(' ') {
                    s3.push_str(label);
                }
                rest = &rest[a + b + 1..];
            }
            None => {
                s3.push_str(&rest[a..]);
                rest = "";
            }
        }
    }
    s3.push_str(rest);

    // Bold and italic marks, entities, spaces.
    let s4 = s3.replace("'''''", "").replace("'''", "").replace("''", "");
    let mut s5 = String::with_capacity(s4.len());
    let mut rest = s4.as_str();
    while let Some(a) = rest.find('&') {
        s5.push_str(&rest[..a]);
        let semi = rest[a..].char_indices().take(12).find(|&(_, c)| c == ';').map(|(i, _)| a + i);
        match semi.and_then(|e| html_entity(&rest[a + 1..e]).map(|c| (c, e))) {
            Some((c, e)) => {
                if c != '\u{200B}' {
                    s5.push(c);
                }
                rest = &rest[e + 1..];
            }
            None => {
                s5.push('&');
                rest = &rest[a + 1..];
            }
        }
    }
    s5.push_str(rest);
    let joined = s5.split_whitespace().collect::<Vec<_>>().join(" ");
    joined.replace(" ,", ",").replace("( ", "(").replace(" )", ")").replace("()", "").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::WikiPage;

    fn dict(pages: &[(&str, &str)]) -> Dictionary {
        Dictionary {
            dump_sha1: String::new(),
            entries: HashMap::new(),
            redirects: HashMap::new(),
            pages: pages
                .iter()
                .map(|(t, body)| {
                    let redirect = body.strip_prefix("#REDIRECT ").map(str::to_string);
                    (t.to_string(), WikiPage { redirect, text: body.to_string() })
                })
                .collect(),
        }
    }

    fn sample() -> Dictionary {
        dict(&[
            ("Template:lb", "#REDIRECT Template:label"),
            ("Template:label", "{{#invoke:labels/templates|show}}<!--\n--><noinclude>{{documentation}}</noinclude>"),
            ("Template:plural of", "{{ {{#if:{{{lang|}}}|check deprecated lang param usage|no deprecated lang param usage}}|lang={{{lang|}}}|<!--\n-->{{#invoke:form of/templates|tagged_form_of_t|p}}<!--\n-->}}"),
            ("Template:no deprecated lang param usage", "{{{1}}}"),
            ("Template:infl of", "#REDIRECT Template:inflection of"),
            ("Template:inflection of", "{{#invoke:form of/templates|inflection_of_t}}"),
            ("Template:synonym of", "{{#invoke:form of/templates|form_of_t|synonym of|withencap=1|conj=or}}"),
            ("Template:w", "[[w:{{safesubst:<noinclude/>#if:{{{lang|}}}|{{{lang}}}:}}{{{1|}}}|{{safesubst:<noinclude/>#if:{{{2|}}}|{{{2}}}|{{{1|}}}}}]]<noinclude>{{documentation}}</noinclude>"),
            ("Template:defdate", "<span class=\"defdate\"><nowiki>[</nowiki>{{{1}}}{{#if:{{{2|}}}|–{{{2}}}}}<nowiki>]</nowiki></span>"),
            ("Template:senseid", "<includeonly><onlyinclude>{{safesubst:<noinclude/>#invoke:anchors/templates|senseid_t}}</onlyinclude></includeonly>"),
            ("Template:,", "{{#invoke:checkparams|error}}<span class=\"serial-comma\">,</span>{{#ifeq:{{{1|}}}|and|<span class=\"serial-and\"> and</span>}}"),
            ("Template:sw", "{{#switch:{{{1}}}|a|b=AB|c=C|#default=D}}"),
            ("Template:mystery", "{{#invoke:mystery|run}}"),
            ("Template:place", "<includeonly>{{#invoke:place|show}}</includeonly>"),
            ("Module:place/placetypes", "export.placetype_aliases = {\n\t[\"c\"] = \"country\",\n\t[\"co\"] = \"county\",\n\t[\"s\"] = \"state\",\n\t[\"cc\"] = \"constituent country\",\n\t[\"ucomm\"] = \"unincorporated community\",\n\t[\"twp\"] = \"township\",\n\t[\"raion\"] = \"raion\",\n\t[\"obl\"] = \"oblast\",\n}\n"),
            ("Module:labels/data", "labels[\"transitive\"] = {\n\taliases = {\"trans\"},\n}\nlabels[\"obsolete\"] = {\n\tdisplay = \"[[obsolete]]\",\n}\n"),
            ("Module:labels/data/qualifiers", "labels[\"_\"] = {\n\tdisplay = \"\",\n\tomit_preComma = true,\n\tomit_postComma = true,\n}\nlabels[\"chiefly\"] = {\n\tomit_postComma = true,\n}\n"),
            ("Module:form of/data/1", "tags[\"plural\"] = {\n\t\"number\",\n\t\"plural number\",\n\t{\"p\", \"pl\"},\n}\ntags[\"simple past\"] = {\n\t\"tense\",\n\tnil,\n\t\"spast\",\n}\ntags[\"past\"] = {\n\t\"tense\",\n}\ntags[\"participle\"] = {\n\t\"non-finite\",\n\tnil,\n\t\"part\",\n}\n"),
            ("Module:form of/lang-data/en", "shortcuts[\"ed-form\"] = {\"spast\", \"and\", \"past\", \"part\"}\n"),
        ])
    }

    #[test]
    fn renders_common_templates() {
        let d = sample();
        let r = Renderer::new(&d);
        let cases = [
            ("{{plural of|en|leaf}}", "plural of leaf"),
            ("{{infl of|en|use||ed-form}}", "simple past and past participle of use"),
            ("{{lb|en|trans|_|chiefly|obsolete}} To [[employ]]; to [[apply#Verb|apply]].", "(transitive chiefly obsolete) To employ; to apply."),
            ("{{synonym of|en|foo<t:a gloss>,bar}}", "synonym of foo (“a gloss”) or bar"),
            ("A [[w:Thing|thing]] named {{w|Jim Smith}}.", "A thing named Jim Smith."),
            ("{{senseid|en|x}}Text {{defdate|from 10th c.}}", "Text [from 10th c.]"),
            ("one{{,|and}} two", "one, and two"),
            ("{{sw|a}} {{sw|c}} {{sw|z}}", "AB C D"),
            ("'''Bold''' and ''italic''<ref>A source.</ref>&nbsp;here", "Bold and italic here"),
            ("[[Category:Hidden]]Seen", "Seen"),
            ("{{defdate|14th c.<ref name=\"E\"/>}}", "[14th c.]"),
            ("{{place|en|village|raion/Nizhyn|obl/Chernihiv|c/Ukraine|founded in 1720}}.", "A village in Nizhyn Raion, Chernihiv Oblast, Ukraine, founded in 1720."),
            ("{{place|en|city|s/Texas|c/USA}}", "A city in Texas, United States"),
            ("{{place|en|country|cont/North America}}; {{place|en|town|c/USA}}", "A country in North America; A town in the United States"),
            ("{{place|en|former silrada|of|raion/Lubny|c/Ukraine|;|centre: [[x|Pisky]]}}", "A former silrada of Lubny Raion, Ukraine; centre: Pisky"),
            ("{{place|en|village/administrative centre|raion/Nizhyn|c/Ukraine|founded <abbr>a.</abbr> 1720}}", "A village, the administrative centre of Nizhyn Raion, Ukraine, founded a. 1720"),
            ("{{place|en|maritime county|in southeast|cc/England}}", "A maritime county in southeast England"),
            ("{{place|en|river|co/Cumbria|cc/England}}", "A river in Cumbria, England"),
            ("{{place|en|town|twp/Republican|co/Jefferson|s/Indiana}}", "A town in Republican Township, Jefferson County, Indiana"),
            ("{{place|en|ucomm|co/Elmore|s/Alabama}}", "An unincorporated community in Elmore County, Alabama"),
            ("from 14<sup>th</sup>c.", "from 14th c."),
            ("{{place|en|rural ucomm|co/Republic|s/Kansas}}", "A rural unincorporated community in Republic County, Kansas"),
            ("{{place|en|state|c/USA|capital=Austin}}.", "A state in the United States. Capital: Austin."),
            ("{{place|en|A coastal fishing <<village>> in the <<dist/Western Area>>, <<c/Sierra Leone>>}}", "A coastal fishing village in the Western Area, Sierra Leone"),
            ("A [[slope|sloping]] [[roof], a [[lean-to]]", "A sloping [[roof], a lean-to"),
        ];
        for (input, want) in cases {
            assert_eq!(r.render("page", input), want, "input: {input}");
        }
    }

    #[test]
    fn counts_fallbacks_and_missing_templates() {
        let d = sample();
        let r = Renderer::new(&d);
        assert_eq!(r.render("page", "{{mystery|en|a|b}} {{nonexistent|x}}"), "a, b");
        let stats = r.stats.lock().unwrap();
        assert_eq!(stats.fallback_invokes.get("mystery|run"), Some(&1));
        assert_eq!(stats.missing_templates.get("Template:nonexistent"), Some(&1));
    }

    #[test]
    fn parses_nested_braces() {
        let nodes = parse("a {{t|{{{1|x}}}|[[l|m]]}} {{{2}}} {{broken");
        assert!(matches!(&nodes[1], Node::Template(parts) if parts.len() == 3));
        assert!(matches!(&nodes[3], Node::Arg(_)));
        assert!(matches!(nodes.last(), Some(Node::Text(t)) if t.ends_with("{{broken")));
    }
}
