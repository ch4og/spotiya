//! What the YouTube homepage hands a logged-out browser: its config (visitor data,
//! player version, experiments) and the BotGuard challenge used to mint PO tokens.

use serde_json::Value;

pub const BROWSER_UA: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36(KHTML, like Gecko)";

pub struct Homepage {
    /// The page's `ytcfg`; BotGuard reads `yt.config_` from it.
    pub ytcfg: Value,
    pub visitor_data: String,
    /// `/s/player/<id>/.../base.js`
    pub js_url: String,
    pub sts: Option<u64>,
    /// GVS tokens are bound to the video ID instead of visitor data under this experiment.
    pub bind_to_video: bool,
    pub program: String,
    pub global_name: String,
    pub interpreter_url: String,
}

pub async fn fetch(http: &reqwest::Client) -> Result<Homepage, String> {
    let html = http
        .get("https://www.youtube.com/")
        .header("user-agent", BROWSER_UA)
        .header("accept-language", "en-US,en;q=0.7")
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("homepage: {e}"))?
        .text()
        .await
        .map_err(|e| format!("homepage: {e}"))?;
    parse(&html)
}

pub fn parse(html: &str) -> Result<Homepage, String> {
    let ytcfg = ytcfg(html).ok_or("homepage: no ytcfg")?;
    let challenge = challenge(html).ok_or("homepage: no BotGuard challenge")?;
    let str_at = |v: &Value, k: &str| v[k].as_str().map(str::to_owned);
    let visitor_data = str_at(&ytcfg, "VISITOR_DATA")
        .or_else(|| str_at(&ytcfg["INNERTUBE_CONTEXT"]["client"], "visitorData"))
        .ok_or("homepage: no visitor data")?;
    let js_url = str_at(&ytcfg, "PLAYER_JS_URL")
        .or_else(|| {
            ytcfg["WEB_PLAYER_CONTEXT_CONFIGS"]
                .as_object()?
                .values()
                .find_map(|c| str_at(c, "jsUrl"))
        })
        .ok_or("homepage: no player URL")?;
    let sts = ytcfg["STS"].as_u64();
    let bind_to_video = ytcfg["WEB_PLAYER_CONTEXT_CONFIGS"]
        .as_object()
        .is_some_and(|configs| {
            configs.values().any(|c| {
                c["serializedExperimentFlags"]
                    .as_str()
                    .is_some_and(|flags| {
                        flags
                            .split('&')
                            .any(|kv| kv == "html5_generate_content_po_token=true")
                    })
            })
        });
    let program = str_at(&challenge, "program").ok_or("challenge: no program")?;
    let global_name = str_at(&challenge, "globalName").ok_or("challenge: no global name")?;
    let interpreter_url = challenge["interpreterUrl"]
        ["privateDoNotAccessOrElseTrustedResourceUrlWrappedValue"]
        .as_str()
        .ok_or("challenge: no interpreter URL")?;
    let interpreter_url = if interpreter_url.starts_with("//") {
        format!("https:{interpreter_url}")
    } else {
        interpreter_url.to_owned()
    };
    Ok(Homepage {
        ytcfg,
        visitor_data,
        js_url,
        sts,
        bind_to_video,
        program,
        global_name,
        interpreter_url,
    })
}

/// Merges every `ytcfg.set({...})` object literal that is plain JSON.
fn ytcfg(html: &str) -> Option<Value> {
    let mut merged = serde_json::Map::new();
    for (i, _) in html.match_indices("ytcfg.set({") {
        let rest = &html[i + "ytcfg.set(".len()..];
        if let Some(Ok(Value::Object(o))) = serde_json::Deserializer::from_str(rest)
            .into_iter::<Value>()
            .next()
        {
            merged.extend(o);
        }
    }
    (!merged.is_empty()).then_some(Value::Object(merged))
}

/// `window.ytAtN({'R': '<JS string holding JSON>', ...})` → its `bgChallenge`.
fn challenge(html: &str) -> Option<Value> {
    let start = html.find("window.ytAtN({'R': '")? + "window.ytAtN({'R': '".len();
    let json = js_string(&html[start..])?;
    let v: Value = serde_json::from_str(&json).ok()?;
    let c = v.get("bgChallenge")?;
    Some(c.clone())
}

/// Decodes a single-quoted JS string body up to its closing quote.
fn js_string(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = s.chars();
    loop {
        match chars.next()? {
            '\'' => return Some(out),
            '\\' => match chars.next()? {
                'x' => {
                    let h: String = chars.by_ref().take(2).collect();
                    out.push(char::from(u8::from_str_radix(&h, 16).ok()?));
                }
                'u' => {
                    let h: String = chars.by_ref().take(4).collect();
                    let mut code = u32::from_str_radix(&h, 16).ok()?;
                    // Surrogate pairs arrive as two escapes.
                    if (0xD800..0xDC00).contains(&code) {
                        let rest: String = chars.by_ref().take(6).collect();
                        let low = u32::from_str_radix(rest.strip_prefix("\\u")?, 16).ok()?;
                        code = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                    }
                    out.push(char::from_u32(code)?);
                }
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                'v' => out.push('\u{b}'),
                '0' => out.push('\0'),
                c => out.push(c),
            },
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_saved_homepage() {
        let Ok(path) = std::env::var("YT_HOMEPAGE") else {
            return;
        };
        let html = std::fs::read_to_string(path).unwrap();
        let h = super::parse(&html).unwrap();
        eprintln!(
            "visitor {}… js {} sts {:?} bind {} program {} B global {} interp {}",
            &h.visitor_data[..12],
            h.js_url,
            h.sts,
            h.bind_to_video,
            h.program.len(),
            h.global_name,
            h.interpreter_url
        );
        assert!(h.js_url.contains("/s/player/"));
    }
}
