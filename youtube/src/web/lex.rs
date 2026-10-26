//! Just enough of a JavaScript lexer to split YouTube's player into statements:
//! it tells strings, templates, regexes and comments apart from code so that
//! bracket depth and statement-ending semicolons can be trusted.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Ident,
    Num,
    Str,
    /// A whole template literal, `${}` parts included.
    Template,
    Regex,
    Punct,
}

#[derive(Clone, Copy, Debug)]
pub struct Token {
    pub kind: Kind,
    pub start: u32,
    pub end: u32,
}

impl Token {
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.start as usize..self.end as usize]
    }
}

const PUNCTS: &[&str] = &[
    ">>>=", "...", "===", "!==", "**=", "<<=", ">>=", ">>>", "&&=", "||=", "??=", "=>", "==", "!=",
    "<=", ">=", "&&", "||", "??", "?.", "++", "--", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=",
    "<<", ">>", "**", "{", "}", "(", ")", "[", "]", ";", ",", "<", ">", "+", "-", "*", "/", "%",
    "&", "|", "^", "!", "~", "?", ":", "=", ".", "@", "#",
];

const REGEX_AFTER_WORD: &[&str] = &[
    "return",
    "typeof",
    "instanceof",
    "in",
    "of",
    "new",
    "delete",
    "void",
    "throw",
    "case",
    "do",
    "else",
    "yield",
    "await",
];

fn ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c == b'\\' || c >= 0x80
}

fn ident_part(c: u8) -> bool {
    ident_start(c) || c.is_ascii_digit()
}

pub fn tokenize(src: &str) -> Result<Vec<Token>, String> {
    let mut out = Vec::with_capacity(src.len() / 4);
    lex(src, 0, false, &mut out)?;
    Ok(out)
}

/// Lexes from `i`; when `nested`, stops at the `}` closing a template `${` and returns its offset.
fn lex(src: &str, mut i: usize, nested: bool, out: &mut Vec<Token>) -> Result<usize, String> {
    let b = src.as_bytes();
    let mut depth = 0usize;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // NBSP, LS and PS also count as whitespace.
        if c == 0xC2 && b.get(i + 1) == Some(&0xA0) {
            i += 2;
            continue;
        }
        if c == 0xE2 && b.get(i + 1) == Some(&0x80) && matches!(b.get(i + 2), Some(0xA8 | 0xA9)) {
            i += 3;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' && b[i] != b'\r' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let end = src[i + 2..].find("*/").ok_or("unterminated comment")?;
            i += end + 4;
            continue;
        }
        let start = i;
        let kind = if ident_start(c) {
            while i < b.len() && ident_part(b[i]) {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            Kind::Ident
        } else if c.is_ascii_digit()
            || c == b'.' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit())
        {
            let hex = c == b'0' && matches!(b.get(i + 1), Some(b'x' | b'X'));
            i += 1;
            while i < b.len() {
                let d = b[i];
                let sign = (d == b'+' || d == b'-') && !hex && matches!(b[i - 1], b'e' | b'E');
                if d.is_ascii_alphanumeric() || d == b'.' || d == b'_' || sign {
                    i += 1;
                } else {
                    break;
                }
            }
            Kind::Num
        } else if c == b'"' || c == b'\'' {
            i += 1;
            while i < b.len() && b[i] != c {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            if i >= b.len() {
                return Err(format!("unterminated string at {start}"));
            }
            i += 1;
            Kind::Str
        } else if c == b'`' {
            i = template(src, i + 1)?;
            Kind::Template
        } else if c == b'/' && regex_allowed(src, out) {
            i += 1;
            let mut class = false;
            while i < b.len() {
                match b[i] {
                    b'\\' => i += 1,
                    b'[' => class = true,
                    b']' => class = false,
                    b'/' if !class => break,
                    b'\n' => return Err(format!("unterminated regex at {start}")),
                    _ => {}
                }
                i += 1;
            }
            i += 1;
            while i < b.len() && ident_part(b[i]) {
                i += 1;
            }
            Kind::Regex
        } else {
            let p = PUNCTS
                .iter()
                .find(|p| src[i..].starts_with(**p))
                .ok_or_else(|| format!("unexpected byte {c:#x} at {i}"))?;
            // `?.` followed by a digit is a conditional, not optional chaining.
            let p = if *p == "?." && b.get(i + 2).is_some_and(|d| d.is_ascii_digit()) {
                "?"
            } else {
                p
            };
            match p {
                "{" => depth += 1,
                "}" if nested && depth == 0 => return Ok(i),
                "}" => depth = depth.saturating_sub(1),
                _ => {}
            }
            i += p.len();
            Kind::Punct
        };
        out.push(Token {
            kind,
            start: start as u32,
            end: i as u32,
        });
    }
    if nested {
        return Err("unterminated template expression".into());
    }
    Ok(i)
}

/// Skips template characters from `i` (just past the backtick) through the closing backtick.
fn template(src: &str, mut i: usize) -> Result<usize, String> {
    let b = src.as_bytes();
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'`' => return Ok(i + 1),
            b'$' if b.get(i + 1) == Some(&b'{') => {
                let mut inner = Vec::new();
                i = lex(src, i + 2, true, &mut inner)? + 1;
            }
            _ => i += 1,
        }
    }
    Err("unterminated template".into())
}

fn regex_allowed(src: &str, out: &[Token]) -> bool {
    let Some(prev) = out.last() else { return true };
    match prev.kind {
        // A keyword after `.` is just a property name: `a.delete / b` divides.
        Kind::Ident => {
            REGEX_AFTER_WORD.contains(&prev.text(src))
                && !out
                    .len()
                    .checked_sub(2)
                    .is_some_and(|k| matches!(out[k].text(src), "." | "?."))
        }
        Kind::Num | Kind::Str | Kind::Template | Kind::Regex => false,
        Kind::Punct => !matches!(prev.text(src), ")" | "]" | "}" | "++" | "--"),
    }
}
