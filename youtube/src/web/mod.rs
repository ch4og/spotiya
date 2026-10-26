//! Playback as YouTube's mobile web client, with PO tokens minted locally.
//!
//! Without a GVS PO token YouTube cuts logged-out streams at 60 seconds. Only web
//! clients accept the WebPO tokens BotGuard mints, and their URLs carry the n (and
//! sometimes sig) challenges of the player JS, so both run in QuickJS (`js`). The
//! audio then comes over plain HTTPS in ranged chunks instead of SABR.

mod bytecode;
mod js;
mod lex;
mod player;
mod session;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};

const REQUEST_KEY: &str = "O43z0dpjhgX20SCx4KAo";
const WAA_API_KEY: &str = "AIzaSyDyT5W0Jh49F30Pqqtyfdf7pDLFKLJoAnw";
const MWEB_VERSION: &str = "2.20260708.05.00";
const MWEB_UA: &str = "Mozilla/5.0 (iPad; CPU OS 16_7_10 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.6 Mobile/15E148 Safari/604.1,gzip(gfe)";
/// Ranged requests keep googlevideo from throttling a single long download.
const CHUNK: u64 = 10 << 20;
/// BotGuard often rejects a VM run; a fresh challenge usually passes.
const MINT_ATTEMPTS: usize = 5;
/// Fresh sessions to try when YouTube serves a visitor SABR only.
const SESSION_ATTEMPTS: usize = 4;
const SABR_ONLY: &str = "session is SABR-only";

struct Minted {
    visitor_data: String,
    sts: Option<u64>,
    bind_to_video: bool,
    player_id: String,
    js_url: String,
    expires: Instant,
}

pub struct Track {
    pub title: String,
    pub duration_secs: u64,
}

pub struct Web {
    http: reqwest::Client,
    cache: PathBuf,
    js: js::JsWorker,
    minted: Mutex<Option<Minted>>,
    /// Serializes preparing the player solver.
    solver: Mutex<()>,
}

impl Web {
    /// `cache` holds the prepared player solver between restarts.
    pub fn new(http: reqwest::Client, cache: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            http,
            cache,
            js: js::JsWorker::spawn(),
            minted: Mutex::new(None),
            solver: Mutex::new(()),
        })
    }

    /// Loads a BotGuard VM and trades its snapshot for an integrity token.
    async fn mint_session(&self) -> Result<Minted, String> {
        let mut last = String::new();
        for attempt in 1..=MINT_ATTEMPTS {
            let page = session::fetch(&self.http).await?;
            let interpreter = self
                .http
                .get(&page.interpreter_url)
                .header("user-agent", session::BROWSER_UA)
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .map_err(|e| format!("interpreter: {e}"))?
                .text()
                .await
                .map_err(|e| format!("interpreter: {e}"))?;
            let snapshot = self
                .js
                .bg_start(
                    interpreter,
                    page.program.clone(),
                    page.global_name.clone(),
                    page.ytcfg.to_string(),
                )
                .await?;
            let it: Value = self
                .http
                .post("https://jnn-pa.googleapis.com/$rpc/google.internal.waa.v1.Waa/GenerateIT")
                .header("content-type", "application/json+protobuf")
                .header("x-goog-api-key", WAA_API_KEY)
                .header("x-user-agent", "grpc-web-javascript/0.1")
                .header("user-agent", session::BROWSER_UA)
                .body(json!([REQUEST_KEY, snapshot]).to_string())
                .send()
                .await
                .map_err(|e| format!("GenerateIT: {e}"))?
                .json()
                .await
                .map_err(|e| format!("GenerateIT: {e}"))?;
            let Some(token) = it[0].as_str() else {
                last = "BotGuard snapshot rejected".into();
                tracing::info!("[web] attempt {attempt}: {last}");
                continue;
            };
            self.js.bg_minter(token.to_owned()).await?;
            let ttl = it[1].as_u64().unwrap_or(21600).min(6 * 3600);
            let player_id = page
                .js_url
                .split("/s/player/")
                .nth(1)
                .and_then(|r| r.split('/').next())
                .ok_or("odd player URL")?
                .to_owned();
            tracing::info!(
                "[web] integrity token after {attempt} attempt(s), ttl {ttl}s, player {player_id}, bind to {}",
                if page.bind_to_video { "video" } else { "visitor" }
            );
            return Ok(Minted {
                visitor_data: page.visitor_data,
                sts: page.sts,
                bind_to_video: page.bind_to_video,
                player_id,
                js_url: page.js_url,
                // Refresh well before YouTube stops honoring the token.
                expires: Instant::now() + Duration::from_secs(ttl.saturating_sub(600)),
            });
        }
        Err(last)
    }

    /// A GVS PO token for `video_id` plus the session it belongs to.
    async fn token(
        &self,
        video_id: &str,
    ) -> Result<(String, String, Option<u64>, String, String), String> {
        let mut minted = self.minted.lock().await;
        if minted.as_ref().is_none_or(|m| Instant::now() > m.expires) {
            *minted = Some(self.mint_session().await?);
        }
        let m = minted.as_ref().expect("minted");
        let binding = if m.bind_to_video {
            video_id
        } else {
            &m.visitor_data
        };
        let pot = match self.js.bg_mint(binding.to_owned()).await {
            Ok(t) => t,
            Err(e) => {
                *minted = None;
                return Err(format!("mint: {e}"));
            }
        };
        Ok((
            pot,
            m.visitor_data.clone(),
            m.sts,
            m.player_id.clone(),
            m.js_url.clone(),
        ))
    }

    /// Path of the preprocessed solver for `player_id`, preparing it on first use.
    async fn solver(&self, player_id: &str, js_url: &str) -> Result<PathBuf, String> {
        let _guard = self.solver.lock().await;
        let dir = &self.cache;
        let path = dir.join(format!("player-{player_id}.js"));
        if path.exists() {
            return Ok(path);
        }
        let t = Instant::now();
        let base = self
            .http
            .get(format!("https://www.youtube.com{js_url}"))
            .header("user-agent", session::BROWSER_UA)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| format!("player JS: {e}"))?
            .text()
            .await
            .map_err(|e| format!("player JS: {e}"))?;
        let code = tokio::task::spawn_blocking(move || player::preprocess(&base))
            .await
            .map_err(|e| e.to_string())??;
        tracing::info!(
            "[web] player {player_id} prepared in {:?} ({} bytes)",
            t.elapsed(),
            code.len()
        );
        // Older players are useless once YouTube moves on.
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().starts_with("player-") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        std::fs::write(&path, code).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(path)
    }

    /// Resolves `video_id` to a playable audio URL.
    async fn resolve(&self, video_id: &str) -> Result<(String, String, u64, u64), String> {
        let (pot, visitor_data, sts, player_id, js_url) = self.token(video_id).await?;
        let mut body = json!({
            "context": { "client": {
                "clientName": "MWEB", "clientVersion": MWEB_VERSION, "userAgent": MWEB_UA,
                "hl": "en", "gl": "US", "visitorData": visitor_data,
            }},
            "videoId": video_id,
            "playbackContext": { "contentPlaybackContext": { "html5Preference": "HTML5_PREF_WANTS" } },
            "contentCheckOk": true,
            "racyCheckOk": true,
        });
        if let Some(sts) = sts {
            body["playbackContext"]["contentPlaybackContext"]["signatureTimestamp"] = json!(sts);
        }
        let player: Value = self
            .http
            .post("https://www.youtube.com/youtubei/v1/player?prettyPrint=false")
            .header("user-agent", MWEB_UA)
            .header("x-youtube-client-name", "2")
            .header("x-youtube-client-version", MWEB_VERSION)
            .header("x-goog-visitor-id", &visitor_data)
            .header("origin", "https://www.youtube.com")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("player: {e}"))?
            .json()
            .await
            .map_err(|e| format!("player: {e}"))?;
        let status = player["playabilityStatus"]["status"]
            .as_str()
            .unwrap_or("?");
        if status != "OK" {
            return Err(format!(
                "{status}: {}",
                player["playabilityStatus"]["reason"].as_str().unwrap_or("")
            ));
        }
        let title = player["videoDetails"]["title"]
            .as_str()
            .unwrap_or(video_id)
            .to_owned();
        let duration = player["videoDetails"]["lengthSeconds"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let formats = player["streamingData"]["adaptiveFormats"]
            .as_array()
            .ok_or("no adaptive formats")?;
        if formats
            .iter()
            .all(|f| f["url"].is_null() && f["signatureCipher"].is_null())
        {
            // This visitor landed in the SABR-only experiment; another session may not have.
            *self.minted.lock().await = None;
            return Err(SABR_ONLY.into());
        }
        // The local player decodes WebM/Opus; prefer the best Opus stream.
        let format = formats
            .iter()
            .filter(|f| {
                f["mimeType"]
                    .as_str()
                    .is_some_and(|m| m.starts_with("audio/webm"))
            })
            .max_by_key(|f| f["bitrate"].as_u64().unwrap_or(0))
            .ok_or("no Opus audio format")?;
        let length: u64 = format["contentLength"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let (mut url, sig) = match format["url"].as_str() {
            Some(u) => (u.to_owned(), None),
            None => {
                let cipher = format["signatureCipher"]
                    .as_str()
                    .ok_or("format has no URL")?;
                let q: Vec<(String, String)> =
                    serde_urlencoded::from_str(cipher).map_err(|e| e.to_string())?;
                let get = |k: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
                let url = get("url").ok_or("cipher without url")?;
                let s = get("s").ok_or("cipher without s")?;
                (
                    url,
                    Some((get("sp").unwrap_or_else(|| "signature".into()), s)),
                )
            }
        };
        let n = query_param(&url, "n");
        if n.is_some() || sig.is_some() {
            let solver = self.solver(&player_id, &js_url).await?;
            let solved = self
                .js
                .solve(
                    player_id,
                    solver,
                    n.iter().cloned().collect(),
                    sig.iter().map(|(_, s)| s.clone()).collect(),
                )
                .await?;
            if let Some(n) = solved.n.first() {
                url = set_query_param(&url, "n", n);
            }
            if let (Some((sp, _)), Some(s)) = (&sig, solved.sig.first()) {
                url = set_query_param(&url, sp, s);
            }
        }
        url = set_query_param(&url, "pot", &pot);
        Ok((url, title, duration, length))
    }
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    serde_urlencoded::from_str::<Vec<(String, String)>>(q)
        .ok()?
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

fn set_query_param(url: &str, key: &str, value: &str) -> String {
    let (base, q) = url.split_once('?').unwrap_or((url, ""));
    let mut pairs: Vec<(String, String)> = serde_urlencoded::from_str(q).unwrap_or_default();
    pairs.retain(|(k, _)| k != key);
    pairs.push((key.to_owned(), value.to_owned()));
    format!(
        "{base}?{}",
        serde_urlencoded::to_string(pairs).unwrap_or_default()
    )
}

impl Web {
    /// Resolves `video_id` and starts downloading its WebM/Opus audio into a channel.
    pub async fn open(&self, video_id: &str) -> Result<(Track, mpsc::Receiver<Bytes>), String> {
        let mut attempt = 1;
        let (url, title, duration, length) = loop {
            match self.resolve(video_id).await {
                Err(e) if e == SABR_ONLY && attempt < SESSION_ATTEMPTS => {
                    tracing::info!("[web] {video_id}: {e}, trying a new session");
                    attempt += 1;
                }
                r => break r?,
            }
        };
        tracing::info!("[web] {video_id} \"{title}\" [{duration}s], {length} bytes");
        let (tx, rx) = mpsc::channel(32);
        let http = self.http.clone();
        let id = video_id.to_owned();
        tokio::spawn(async move {
            let mut pos = 0u64;
            while length == 0 || pos < length {
                let end = if length == 0 {
                    pos + CHUNK - 1
                } else {
                    (pos + CHUNK).min(length) - 1
                };
                let resp = match http
                    .get(set_query_param(&url, "range", &format!("{pos}-{end}")))
                    .header("user-agent", MWEB_UA)
                    .header("origin", "https://www.youtube.com")
                    .send()
                    .await
                    .and_then(|r| r.error_for_status())
                {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("[web] {id} chunk at {pos} failed: {e}");
                        return;
                    }
                };
                let mut body = resp.bytes_stream();
                let mut got = 0u64;
                while let Some(chunk) = body.next().await {
                    let Ok(chunk) = chunk else {
                        tracing::warn!("[web] {id} download broke at {}", pos + got);
                        return;
                    };
                    got += chunk.len() as u64;
                    if tx.send(chunk).await.is_err() {
                        return; // Listener went away.
                    }
                }
                if got == 0 {
                    break;
                }
                pos += got;
            }
            tracing::info!("[web] {id} downloaded {pos} bytes");
        });
        Ok((
            Track {
                title,
                duration_secs: duration,
            },
            rx,
        ))
    }
}
