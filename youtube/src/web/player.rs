//! Turns YouTube's base.js into a small-footprint n/sig solver, the way yt-dlp's
//! ejs does but by slicing source text instead of building and printing an AST.
//!
//! The player is one IIFE. Its top-level expression statements that are not plain
//! assignments start the player (and need a browser), so they are dropped; the rest
//! only defines functions and tables. The URL builder is the function whose body
//! calls `x.set("alr","yes")`; it applies both the n and the sig transforms.

use super::lex::{tokenize, Kind, Token};

const SETUP: &str = r#"if (typeof globalThis.XMLHttpRequest === "undefined") globalThis.XMLHttpRequest = { prototype: {} };
if (typeof URL === "undefined") {
  globalThis.location = { hash: "", host: "www.youtube.com", hostname: "www.youtube.com",
    href: "https://www.youtube.com/watch?v=yt-dlp-wins", origin: "https://www.youtube.com", password: "",
    pathname: "/watch", port: "", protocol: "https:", search: "?v=yt-dlp-wins", username: "" };
} else globalThis.location = new URL("https://www.youtube.com/watch?v=yt-dlp-wins");
if (typeof globalThis.document === "undefined") globalThis.document = Object.create(null);
if (typeof globalThis.navigator === "undefined") globalThis.navigator = Object.create(null);
if (typeof globalThis.self === "undefined") globalThis.self = globalThis;
if (typeof globalThis.window === "undefined") globalThis.window = globalThis;
"#;

const ASSIGN_OPS: &[&str] = &[
    "=", "+=", "-=", "*=", "/=", "%=", "**=", "<<=", ">>=", ">>>=", "&=", "|=", "^=", "&&=", "||=",
    "??=",
];

const UNARY_WORDS: &[&str] = &[
    "new", "typeof", "void", "delete", "await", "yield", "function", "class", "async",
];

struct Player<'a> {
    src: &'a str,
    toks: Vec<Token>,
    /// For each bracket token, the index of its partner.
    pair: Vec<u32>,
}

impl<'a> Player<'a> {
    fn new(src: &'a str) -> Result<Self, String> {
        let toks = tokenize(src)?;
        let mut pair = vec![u32::MAX; toks.len()];
        let mut stack = Vec::new();
        for (i, t) in toks.iter().enumerate() {
            if t.kind != Kind::Punct {
                continue;
            }
            match t.text(src) {
                "(" | "[" | "{" => stack.push(i),
                ")" | "]" | "}" => {
                    let o = stack
                        .pop()
                        .ok_or_else(|| format!("unbalanced bracket at {}", t.start))?;
                    pair[o] = i as u32;
                    pair[i] = o as u32;
                }
                _ => {}
            }
        }
        if !stack.is_empty() {
            return Err("unclosed bracket".into());
        }
        Ok(Self { src, toks, pair })
    }

    fn text(&self, i: usize) -> &'a str {
        self.toks.get(i).map_or("", |t| t.text(self.src))
    }

    fn is(&self, i: usize, s: &str) -> bool {
        self.toks
            .get(i)
            .is_some_and(|t| t.kind == Kind::Punct || t.kind == Kind::Ident)
            && self.text(i) == s
    }

    fn is_open(&self, i: usize) -> bool {
        self.toks[i].kind == Kind::Punct && matches!(self.text(i), "(" | "[" | "{")
    }

    /// Index just past the statement starting at `i`, which must end before `limit`.
    fn statement_end(&self, i: usize, limit: usize) -> usize {
        let mut word = if self.toks[i].kind == Kind::Ident {
            self.text(i)
        } else {
            ""
        };
        if word == "async" && self.is(i + 1, "function") {
            word = "function";
        }
        match word {
            "var" | "let" | "const" => return self.expression_end(i, limit),
            "function" | "class" => {
                let mut j = i;
                while j < limit && !self.is(j, "{") {
                    j += if self.is_open(j) {
                        self.pair[j] as usize + 1 - j
                    } else {
                        1
                    };
                }
                return (self.pair[j] as usize + 1).min(limit);
            }
            "if" => {
                let mut j = self.statement_end(self.pair[i + 1] as usize + 1, limit);
                if self.is(j, "else") {
                    j = self.statement_end(j + 1, limit);
                }
                return j;
            }
            "for" | "while" | "with" => {
                return self.statement_end(self.pair[i + 1] as usize + 1, limit)
            }
            "do" => {
                let j = self.statement_end(i + 1, limit);
                let j = self.pair[j + 1] as usize + 1;
                return if self.is(j, ";") { j + 1 } else { j };
            }
            "switch" => return self.pair[self.pair[i + 1] as usize + 1] as usize + 1,
            "try" => {
                let mut j = self.pair[i + 1] as usize + 1;
                if self.is(j, "catch") {
                    j += 1;
                    if self.is(j, "(") {
                        j = self.pair[j] as usize + 1;
                    }
                    j = self.pair[j] as usize + 1;
                }
                if self.is(j, "finally") {
                    j = self.pair[j + 1] as usize + 1;
                }
                return j;
            }
            _ => {}
        }
        if self.is(i, "{") {
            return self.pair[i] as usize + 1;
        }
        if self.is(i, ";") {
            return i + 1;
        }
        if self.toks[i].kind == Kind::Ident && self.is(i + 1, ":") {
            return self.statement_end(i + 2, limit);
        }
        self.expression_end(i, limit)
    }

    /// Index past the `;` ending an expression (or declaration) at depth 0.
    fn expression_end(&self, mut j: usize, limit: usize) -> usize {
        while j < limit {
            if self.is_open(j) {
                j = self.pair[j] as usize + 1;
            } else if self.is(j, ";") {
                return j + 1;
            } else {
                j += 1;
            }
        }
        limit
    }

    /// Whether the expression statement `[i, end)` is a single assignment, as ejs keeps.
    fn is_assignment(&self, i: usize, end: usize) -> bool {
        let end = if self.is(end - 1, ";") { end - 1 } else { end };
        let mut j = i;
        let mut seen_assign = false;
        while j < end {
            let t = self.toks[j];
            if self.is_open(j) {
                j = self.pair[j] as usize + 1;
                continue;
            }
            let s = self.text(j);
            match t.kind {
                Kind::Punct if s == "." || s == "?." => {}
                Kind::Punct if s == "," => return false,
                Kind::Punct if !seen_assign && ASSIGN_OPS.contains(&s) => {
                    if j == i {
                        return false;
                    }
                    seen_assign = true;
                }
                Kind::Punct if !seen_assign => return false,
                Kind::Ident
                    if !seen_assign
                        && UNARY_WORDS.contains(&s)
                        && !(j > i && matches!(self.text(j - 1), "." | "?.")) =>
                {
                    return false;
                }
                _ => {}
            }
            j += 1;
        }
        seen_assign
    }

    /// Whether the function body `{ ... }` opening at `open` has a top-level `x.y("alr","yes")`.
    fn builds_url(&self, open: usize) -> bool {
        let close = self.pair[open] as usize;
        let mut j = open + 1;
        while j < close {
            let end = self.statement_end(j, close);
            if self.is_alr_call(j, end) {
                return true;
            }
            j = end.max(j + 1);
        }
        false
    }

    fn is_alr_call(&self, i: usize, end: usize) -> bool {
        let lit = |k: usize, v: &str| {
            self.toks.get(k).is_some_and(|t| t.kind == Kind::Str) && {
                let s = self.text(k);
                &s[1..s.len() - 1] == v
            }
        };
        if self.toks[i].kind != Kind::Ident {
            return false;
        }
        let call =
            if self.is(i + 1, ".") && self.toks.get(i + 2).is_some_and(|t| t.kind == Kind::Ident) {
                i + 3
            } else if self.is(i + 1, "[") {
                self.pair[i + 1] as usize + 1
            } else {
                return false;
            };
        self.is(call, "(")
            && lit(call + 1, "alr")
            && self.is(call + 2, ",")
            && lit(call + 3, "yes")
            && self.pair[call] as usize == call + 4
            && (call + 5 == end || self.is(call + 5, ";") && call + 6 == end)
    }

    /// If `[i, end)` is `NAME = function(..){..}` (or `var NAME = function..`), returns `NAME`'s text
    /// for each such function that builds URLs.
    fn url_builders(&self, i: usize, end: usize, out: &mut Vec<String>) {
        if self.is(i, "function")
            && self.toks.get(i + 1).is_some_and(|t| t.kind == Kind::Ident)
            && self.is(i + 2, "(")
        {
            let open = self.pair[i + 2] as usize + 1;
            if self.is(open, "{") && self.builds_url(open) {
                out.push(self.text(i + 1).to_owned());
            }
            return;
        }
        let end = if self.is(end - 1, ";") { end - 1 } else { end };
        let decl =
            matches!(self.text(i), "var" | "let" | "const") && self.toks[i].kind == Kind::Ident;
        let mut j = if decl { i + 1 } else { i };
        loop {
            // Target: an identifier or member chain up to `=`.
            let name_start = j;
            while j < end && !self.is(j, "=") && !self.is(j, ",") {
                j += if self.is_open(j) {
                    self.pair[j] as usize + 1 - j
                } else {
                    1
                };
            }
            if j >= end || !self.is(j, "=") {
                return;
            }
            let name_end = j;
            j += 1;
            let mut k = j;
            if self.is(k, "function") {
                k += 1;
                if self.toks.get(k).is_some_and(|t| t.kind == Kind::Ident) {
                    k += 1;
                }
                if self.is(k, "(") && self.is(self.pair[k] as usize + 1, "{") {
                    let open = self.pair[k] as usize + 1;
                    let after = self.pair[open] as usize + 1;
                    if (after == end || decl && self.is(after, ",")) && self.builds_url(open) {
                        let s = self.toks[name_start].start as usize;
                        let e = self.toks[name_end - 1].end as usize;
                        out.push(self.src[s..e].to_owned());
                    }
                }
            }
            if !decl {
                return;
            }
            // Next declarator.
            while j < end && !self.is(j, ",") {
                j += if self.is_open(j) {
                    self.pair[j] as usize + 1 - j
                } else {
                    1
                };
            }
            if j >= end {
                return;
            }
            j += 1;
        }
    }
}

/// Builds the solver script for a fresh, non-strict context with an `_result`
/// object. Its `n(challenge)` and `sig(challenge)` functions decode URL parameters.
pub fn preprocess(src: &str) -> Result<String, String> {
    let p = Player::new(src)?;
    // `var _yt_player={};(function(g){ ... })(_yt_player);`
    let func = (0..p.toks.len())
        .find(|&i| p.is(i, "function") && i > 0 && p.is(i - 1, "("))
        .ok_or("player IIFE not found")?;
    let params = func + 1;
    if !p.is(params, "(") {
        return Err("unexpected IIFE shape".into());
    }
    let open = p.pair[params] as usize + 1;
    if !p.is(open, "{") {
        return Err("unexpected IIFE body".into());
    }
    let close = p.pair[open] as usize;

    let mut out = String::with_capacity(src.len());
    out.push_str(SETUP);
    out.push_str(&src[..p.toks[open].end as usize]);
    out.push('\n');
    let mut builders = Vec::new();
    let mut i = open + 1;
    // ejs drops the leading `var window=this;` so `window` is the global.
    if p.is(i, "var") && p.is(i + 1, "window") && p.is(i + 2, "=") && p.is(i + 3, "this") {
        i = p.statement_end(i, close);
    }
    while i < close {
        let end = p.statement_end(i, close);
        let t = p.toks[i];
        let async_fn = p.is(i, "async") && p.is(i + 1, "function");
        let expression = !async_fn
            && !(t.kind == Kind::Ident
                && matches!(
                    p.text(i),
                    "var"
                        | "let"
                        | "const"
                        | "function"
                        | "class"
                        | "if"
                        | "for"
                        | "while"
                        | "do"
                        | "try"
                        | "switch"
                        | "with"
                ))
            && !p.is(i, "{")
            && !p.is(i, ";")
            && !(t.kind == Kind::Ident && p.is(i + 1, ":"));
        let literal = t.kind == Kind::Str && (end == i + 1 || end == i + 2 && p.is(i + 1, ";"));
        let keep = !expression || literal || p.is_assignment(i, end);
        if keep {
            p.url_builders(i, end, &mut builders);
            let s = t.start as usize;
            let e = p.toks[end - 1].end as usize;
            out.push_str(&src[s..e]);
            if !matches!(&src[e - 1..e], ";" | "}") {
                out.push(';');
            }
            out.push('\n');
        }
        i = end.max(i + 1);
    }
    if builders.is_empty() {
        return Err("URL builder function not found".into());
    }
    for kind in ["n", "sig"] {
        out.push_str(&format!(
            "_result.{kind} = {};\n",
            multi_try(&builders, kind)
        ));
    }
    out.push_str(&src[p.toks[close].start as usize..]);
    Ok(out)
}

fn multi_try(builders: &[String], kind: &str) -> String {
    let solvers: Vec<String> = builders
        .iter()
        .map(|name| {
            format!(
                r#"({kind}) => (({{sig, n}}) => {{
  const url = ({name})("https://youtube.com/watch?v=yt-dlp-wins", "s", sig ? encodeURIComponent(sig) : undefined);
  url.set("n", n);
  const proto = Object.getPrototypeOf(url);
  const keys = Object.keys(proto).concat(Object.getOwnPropertyNames(proto));
  for (const key of keys) {{
    if (!["constructor", "set", "get", "clone"].includes(key)) {{ url[key](); break; }}
  }}
  const s = url.get("s");
  return {{ sig: s ? decodeURIComponent(s) : null, n: url.get("n") ?? null }};
}})({{{kind}}}).{kind}"#
            )
        })
        .collect();
    format!(
        r#"(_input) => {{
  const _results = new Set();
  const errors = [];
  for (const _generator of [{}]) {{
    try {{ _results.add(_generator(_input)); }} catch (e) {{ errors.push(e); }}
  }}
  if (!_results.size) throw `no solutions: ${{errors.join(", ")}}`;
  if (_results.size !== 1) throw `invalid solutions: ${{[..._results].map(x => JSON.stringify(x)).join(", ")}}`;
  return _results.values().next().value;
}}"#,
        solvers.join(",\n")
    )
}
