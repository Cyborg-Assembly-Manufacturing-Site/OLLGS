//! Reads the literal data tables in Wiktionary's Lua data modules, such as
//! `labels["obsolete"] = { aliases = {"obs"}, display = "obsolete" }`.
//!
//! Only literals are understood: strings, numbers, booleans, nil and tables.
//! A bare variable reads as nil; a statement whose value is anything else (a
//! function call, an expression) is skipped, and the number skipped is
//! reported so gaps are visible.

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Num(f64),
    Bool(bool),
    Nil,
    /// Entries in source order; positional entries have no key.
    Table(Vec<(Option<String>, Value)>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Table(t) => t.iter().find(|(k, _)| k.as_deref() == Some(key)).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn truthy(&self, key: &str) -> bool {
        !matches!(self.get(key), None | Some(Value::Nil) | Some(Value::Bool(false)))
    }

    /// The positional entries (1, 2, 3 ...), including explicit nils.
    pub fn positional(&self) -> Vec<&Value> {
        match self {
            Value::Table(t) => t.iter().filter(|(k, _)| k.is_none()).map(|(_, v)| v).collect(),
            _ => Vec::new(),
        }
    }

    /// A string, or every string in a table of strings.
    pub fn strings(&self) -> Vec<&str> {
        match self {
            Value::Str(s) => vec![s],
            Value::Table(_) => self.positional().into_iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        }
    }
}

struct P<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> P<'a> {
    fn skip_ws(&mut self) {
        loop {
            while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
                self.i += 1;
            }
            if self.s[self.i..].starts_with(b"--") {
                self.i += 2;
                if let Some(level) = self.long_bracket_level() {
                    self.long_string(level);
                } else {
                    while self.i < self.s.len() && self.s[self.i] != b'\n' {
                        self.i += 1;
                    }
                }
            } else {
                return;
            }
        }
    }

    /// At `[[` or `[=*[`, returns the number of `=` signs.
    fn long_bracket_level(&self) -> Option<usize> {
        let rest = &self.s[self.i..];
        if rest.first() != Some(&b'[') {
            return None;
        }
        let eqs = rest[1..].iter().take_while(|&&b| b == b'=').count();
        (rest.get(1 + eqs) == Some(&b'[')).then_some(eqs)
    }

    fn long_string(&mut self, level: usize) -> Option<String> {
        self.i += level + 2;
        let close: Vec<u8> = [&[b']'][..], &vec![b'='; level], &[b']']].concat();
        let start = self.i;
        let end = memchr::memmem::find(&self.s[start..], &close)? + start;
        self.i = end + close.len();
        let mut body = &self.s[start..end];
        if body.first() == Some(&b'\n') {
            body = &body[1..];
        }
        String::from_utf8(body.to_vec()).ok()
    }

    fn quoted(&mut self) -> Option<String> {
        let q = self.s[self.i];
        self.i += 1;
        let mut out = Vec::new();
        while self.i < self.s.len() {
            let b = self.s[self.i];
            self.i += 1;
            match b {
                b'\\' => {
                    let e = *self.s.get(self.i)?;
                    self.i += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'\\' | b'"' | b'\'' => out.push(e),
                        b'\n' => out.push(b'\n'),
                        b'0'..=b'9' => {
                            let mut n = (e - b'0') as u32;
                            for _ in 0..2 {
                                match self.s.get(self.i) {
                                    Some(d @ b'0'..=b'9') => {
                                        n = n * 10 + (d - b'0') as u32;
                                        self.i += 1;
                                    }
                                    _ => break,
                                }
                            }
                            out.push(u8::try_from(n).ok()?);
                        }
                        _ => return None,
                    }
                }
                b'\n' => return None,
                _ if b == q => return String::from_utf8(out).ok(),
                _ => out.push(b),
            }
        }
        None
    }

    fn name(&mut self) -> Option<&'a str> {
        let start = self.i;
        while self.i < self.s.len() && (self.s[self.i].is_ascii_alphanumeric() || self.s[self.i] == b'_') {
            self.i += 1;
        }
        (self.i > start).then(|| std::str::from_utf8(&self.s[start..self.i]).unwrap())
    }

    fn value(&mut self) -> Option<Value> {
        self.skip_ws();
        let b = *self.s.get(self.i)?;
        let v = match b {
            b'"' | b'\'' => Value::Str(self.quoted()?),
            b'[' => Value::Str(self.long_string(self.long_bracket_level()?)?),
            b'{' => self.table()?,
            b'-' | b'0'..=b'9' => {
                let start = self.i;
                self.i += 1;
                while self.i < self.s.len() && (self.s[self.i].is_ascii_alphanumeric() || self.s[self.i] == b'.') {
                    self.i += 1;
                }
                Value::Num(std::str::from_utf8(&self.s[start..self.i]).ok()?.parse().ok()?)
            }
            _ => match self.name()? {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                // A variable (such as APPENDIX) cannot be resolved here; it reads as nil.
                // A call or an expression still fails the check below.
                _ => Value::Nil,
            },
        };
        // Anything after a literal other than a separator (`..`, a call) is not a literal.
        self.skip_ws();
        match self.s.get(self.i) {
            None | Some(b',' | b';' | b'}' | b']') => Some(v),
            _ if self.at_statement_end() => Some(v),
            _ => None,
        }
    }

    fn at_statement_end(&self) -> bool {
        // After skip_ws we may be at the start of the next statement.
        let rest = &self.s[self.i..];
        rest.first().is_some_and(|b| b.is_ascii_alphabetic())
    }

    fn table(&mut self) -> Option<Value> {
        self.i += 1;
        let mut entries = Vec::new();
        loop {
            self.skip_ws();
            match self.s.get(self.i)? {
                b'}' => {
                    self.i += 1;
                    return Some(Value::Table(entries));
                }
                b',' | b';' => self.i += 1,
                b'[' if self.long_bracket_level().is_none() => {
                    self.i += 1;
                    let key = match self.value()? {
                        Value::Str(s) => s,
                        Value::Num(n) => n.to_string(),
                        _ => return None,
                    };
                    self.skip_ws();
                    if self.s.get(self.i) != Some(&b']') {
                        return None;
                    }
                    self.i += 1;
                    self.skip_ws();
                    if self.s.get(self.i) != Some(&b'=') {
                        return None;
                    }
                    self.i += 1;
                    let v = self.entry_value()?;
                    entries.push((Some(key), v));
                }
                b if b.is_ascii_alphabetic() || *b == b'_' => {
                    let save = self.i;
                    let name = self.name()?;
                    self.skip_ws();
                    if self.s.get(self.i) == Some(&b'=') && self.s.get(self.i + 1) != Some(&b'=') {
                        self.i += 1;
                        let v = self.entry_value()?;
                        entries.push((Some(name.to_string()), v));
                    } else {
                        self.i = save;
                        let v = self.entry_value()?;
                        entries.push((None, v));
                    }
                }
                _ => {
                    let v = self.entry_value()?;
                    entries.push((None, v));
                }
            }
        }
    }
}

impl P<'_> {
    /// A table entry's value; an expression that is not a literal is skipped
    /// and reads as nil, so the rest of the table can still be read.
    fn entry_value(&mut self) -> Option<Value> {
        let save = self.i;
        if let Some(v) = self.value() {
            return Some(v);
        }
        self.i = save;
        let mut depth = 0usize;
        while self.i < self.s.len() {
            self.skip_ws();
            let Some(&b) = self.s.get(self.i) else { break };
            match b {
                b'"' | b'\'' => {
                    self.quoted()?;
                    continue;
                }
                b'[' if self.long_bracket_level().is_some() => {
                    self.long_string(self.long_bracket_level()?)?;
                    continue;
                }
                b'(' | b'{' | b'[' => depth += 1,
                b')' | b']' => depth = depth.checked_sub(1)?,
                b'}' if depth == 0 => return Some(Value::Nil),
                b'}' => depth -= 1,
                b',' | b';' if depth == 0 => return Some(Value::Nil),
                _ => {}
            }
            self.i += 1;
        }
        None
    }
}

/// Every `table_name["key"] = <literal>` statement, in source order, and the
/// number of such statements whose value was not a literal.
pub fn assignments(src: &str, table_name: &str) -> (Vec<(String, Value)>, usize) {
    let s = src.as_bytes();
    let pat = format!("{table_name}[");
    let mut out = Vec::new();
    let mut skipped = 0;
    for start in memchr::memmem::find_iter(s, pat.as_bytes()) {
        // Only statements at the start of a line.
        if start > 0 && s[start - 1] != b'\n' {
            continue;
        }
        let mut p = P { s, i: start + pat.len() };
        let parsed = (|| {
            let key = match p.value()? {
                Value::Str(k) => k,
                _ => return None,
            };
            p.skip_ws();
            if p.s.get(p.i) != Some(&b']') {
                return None;
            }
            p.i += 1;
            p.skip_ws();
            if p.s.get(p.i) != Some(&b'=') || p.s.get(p.i + 1) == Some(&b'=') {
                return None;
            }
            p.i += 1;
            Some((key, p.value()?))
        })();
        match parsed {
            Some(kv) => out.push(kv),
            None => skipped += 1,
        }
    }
    (out, skipped)
}

/// The table literal assigned by `name = { ... }` (e.g. `export.placetype_aliases`).
pub fn assigned_table(src: &str, name: &str) -> Option<Value> {
    let s = src.as_bytes();
    let pat = format!("{name} =");
    let start = memchr::memmem::find_iter(s, pat.as_bytes()).find(|&i| i == 0 || s[i - 1] == b'\n')?;
    let mut p = P { s, i: start + pat.len() };
    p.skip_ws();
    if p.s.get(p.i) != Some(&b'{') {
        return None;
    }
    p.table()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_label_and_tag_tables() {
        let src = r#"local labels = {}
-- a comment with labels["fake"] = 1
labels["intransitive"] = {
	aliases = {"not transitive", "intr"}, -- trailing comment
	glossary = true,
	display = "[[intransitive]]",
}

labels["_"] = {
	display = "",
	omit_preComma = true,
}
labels["computed"] = make("x")
labels["joined"] = APPENDIX .. "x"
tags["plural"] = {
	"number",
	APPENDIX,
	{"p", "pl"},
	146786,
}
m["xx"] = {
	"Example",
	123,
	sort_key = s["xx-sortkey"],
	from = {"æ" .. c.x},
	"after",
}
tags["third-person (-th)"] = {
	"person",
	nil,
	"3-th",
	display = [[third-person]],
}
shortcuts["ed-form"] = {"spast", "and", "past", "part"}
labels["quote"] = 'it\'s "fine"'
"#;
        let (labels, skipped) = assignments(src, "labels");
        assert_eq!(skipped, 2);
        let names: Vec<&str> = labels.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["intransitive", "_", "quote"]);
        assert_eq!(labels[0].1.get("aliases").unwrap().strings(), ["not transitive", "intr"]);
        assert_eq!(labels[0].1.get("display").unwrap().as_str(), Some("[[intransitive]]"));
        assert!(labels[1].1.truthy("omit_preComma"));
        assert!(!labels[1].1.truthy("omit_postComma"));
        assert_eq!(labels[2].1.as_str(), Some("it's \"fine\""));

        let (tags, _) = assignments(src, "tags");
        assert_eq!(tags[0].1.positional()[1], &Value::Nil);
        assert_eq!(tags[0].1.positional()[2].strings(), ["p", "pl"]);
        assert_eq!(tags[1].1.positional()[1], &Value::Nil);
        assert_eq!(tags[1].1.get("display").unwrap().as_str(), Some("third-person"));
        let (m, _) = assignments(src, "m");
        let pos = m[0].1.positional();
        assert_eq!(pos[0].as_str(), Some("Example"));
        assert_eq!(pos[2].as_str(), Some("after"));
        assert_eq!(m[0].1.get("sort_key"), Some(&Value::Nil));
        let aliases = assigned_table("x\nexport.placetype_aliases = {\n\t[\"c\"] = \"country\",\n\t[\"co\"] = \"county\",\n}\n", "export.placetype_aliases").unwrap();
        assert_eq!(aliases.get("co").and_then(Value::as_str), Some("county"));
        let (shortcuts, _) = assignments(src, "shortcuts");
        assert_eq!(shortcuts[0].1.strings(), ["spast", "and", "past", "part"]);
    }
}
