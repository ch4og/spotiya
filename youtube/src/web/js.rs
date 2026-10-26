//! A thread owning QuickJS for the two jobs that need YouTube's own JavaScript:
//! running BotGuard to mint PO tokens, and the player's n/sig transforms.
//!
//! QuickJS runtimes are not `Send`, so callers post jobs over a channel. The
//! BotGuard runtime lives as long as its integrity token. The much larger player
//! runtime exists only while transforming a URL and is released before playback.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use rquickjs::{Context, Runtime};
use tokio::sync::oneshot;

const BG_JS: &str = include_str!("js/bg.js");
const SHIM_JS: &str = include_str!("js/shim.js");
const DOM_JSON: &str = include_str!("js/dom.json");

const JOB_TIMEOUT: Duration = Duration::from_secs(60);

type Reply<T> = oneshot::Sender<Result<T, String>>;

enum Job {
    /// Fresh BotGuard VM from a challenge; replies with the snapshot for GenerateIT.
    BgStart {
        interpreter: String,
        program: String,
        global_name: String,
        ytcfg: String,
        reply: Reply<String>,
    },
    /// Hands the integrity token to the VM, which then yields a minter.
    BgMinter {
        integrity_token: String,
        reply: Reply<()>,
    },
    BgMint {
        binding: String,
        reply: Reply<String>,
    },
    /// Decodes n and sig challenges with a preprocessed player (see `player::preprocess`).
    Solve {
        player_id: String,
        solver: std::path::PathBuf,
        n: Vec<String>,
        sig: Vec<String>,
        reply: Reply<Solved>,
    },
}

#[derive(Debug, Default)]
pub struct Solved {
    pub n: Vec<String>,
    pub sig: Vec<String>,
}

#[derive(Clone)]
pub struct JsWorker {
    jobs: mpsc::Sender<Job>,
}

impl JsWorker {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("ytjs".into())
            // QuickJS recursion over deeply nested player code needs more than the default.
            .stack_size(4 << 20)
            .spawn(move || run(rx))
            .expect("spawn JS thread");
        Self { jobs: tx }
    }

    async fn call<T>(&self, make: impl FnOnce(Reply<T>) -> Job) -> Result<T, String> {
        let (tx, rx) = oneshot::channel();
        self.jobs
            .send(make(tx))
            .map_err(|_| "JS thread gone".to_owned())?;
        rx.await
            .map_err(|_| "JS thread dropped the job".to_owned())?
    }

    pub async fn bg_start(
        &self,
        interpreter: String,
        program: String,
        global_name: String,
        ytcfg: String,
    ) -> Result<String, String> {
        self.call(|reply| Job::BgStart {
            interpreter,
            program,
            global_name,
            ytcfg,
            reply,
        })
        .await
    }

    pub async fn bg_minter(&self, integrity_token: String) -> Result<(), String> {
        self.call(|reply| Job::BgMinter {
            integrity_token,
            reply,
        })
        .await
    }

    pub async fn bg_mint(&self, binding: String) -> Result<String, String> {
        self.call(|reply| Job::BgMint { binding, reply }).await
    }

    pub async fn solve(
        &self,
        player_id: String,
        solver: std::path::PathBuf,
        n: Vec<String>,
        sig: Vec<String>,
    ) -> Result<Solved, String> {
        self.call(|reply| Job::Solve {
            player_id,
            solver,
            n,
            sig,
            reply,
        })
        .await
    }
}

struct Engine {
    rt: Runtime,
    ctx: Context,
}

impl Engine {
    fn new(memory_limit: usize) -> Result<Self, String> {
        let rt = Runtime::new().map_err(|e| e.to_string())?;
        rt.set_memory_limit(memory_limit);
        rt.set_max_stack_size(3 << 20);
        let ctx = Context::full(&rt).map_err(|e| e.to_string())?;
        Ok(Self { rt, ctx })
    }

    fn eval(&self, code: &str) -> Result<(), String> {
        self.ctx
            .with(|ctx| ctx.eval::<(), _>(code).map_err(|e| describe(&ctx, e)))
    }

    /// Calls a global function with string arguments.
    fn call(&self, name: &str, args: &[&str]) -> Result<(), String> {
        self.ctx.with(|ctx| {
            let f: rquickjs::Function = ctx.globals().get(name).map_err(|e| describe(&ctx, e))?;
            let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
            f.call::<_, ()>((rquickjs::function::Rest(args),))
                .map_err(|e| describe(&ctx, e))
        })
    }

    /// Pumps promise jobs and timers until the glue's `__out` settles.
    fn settle(&self) -> Result<String, String> {
        let deadline = Instant::now() + JOB_TIMEOUT;
        loop {
            while self
                .rt
                .execute_pending_job()
                .map_err(|_| "JS job failed".to_owned())?
            {}
            let (done, value, error, next) = self.ctx.with(|ctx| {
                let out: rquickjs::Object =
                    ctx.globals().get("__out").map_err(|e| describe(&ctx, e))?;
                let done: bool = out.get("done").unwrap_or(false);
                let value: Option<String> = out
                    .get::<_, rquickjs::Coerced<String>>("value")
                    .ok()
                    .map(|c| c.0);
                let error: Option<String> = out.get("error").ok().flatten();
                let run: rquickjs::Function = ctx
                    .globals()
                    .get("__runTimers")
                    .map_err(|e| describe(&ctx, e))?;
                let next: i32 = if done { -1 } else { run.call(()).unwrap_or(-1) };
                Ok::<_, String>((done, value, error, next))
            })?;
            if done {
                return match error {
                    Some(e) => Err(e),
                    None => Ok(value.unwrap_or_default()),
                };
            }
            if Instant::now() > deadline {
                return Err("JS timed out".into());
            }
            if next != 0 {
                let wait = if next < 0 { 5 } else { next.min(50) as u64 };
                std::thread::sleep(Duration::from_millis(wait));
            }
        }
    }
}

pub(super) fn describe(ctx: &rquickjs::Ctx<'_>, e: rquickjs::Error) -> String {
    if matches!(e, rquickjs::Error::Exception) {
        let ex = ctx.catch();
        if let Some(obj) = ex.as_object() {
            let msg: String = obj.get("message").unwrap_or_default();
            let stack: String = obj.get("stack").unwrap_or_default();
            return format!("{msg}\n{stack}");
        }
        return format!("{ex:?}");
    }
    e.to_string()
}

fn bg_engine(
    interpreter: &str,
    program: &str,
    global_name: &str,
    ytcfg: &str,
) -> Result<(Engine, String), String> {
    let e = Engine::new(64 << 20)?;
    e.eval(BG_JS)?;
    e.eval(&format!("globalThis.DOM_TABLE = {DOM_JSON};"))?;
    e.eval(SHIM_JS)?;
    e.call("__bgStart", &[interpreter, program, global_name, ytcfg])?;
    let snapshot = e.settle()?;
    Ok((e, snapshot))
}

fn solver_engine(source: &std::path::Path) -> Result<Engine, String> {
    let e = Engine::new(96 << 20)?;
    e.eval("var __solver = { n: null, sig: null }; var _result = __solver;")?;
    e.ctx.with(|ctx| super::bytecode::eval(&ctx, source))?;
    Ok(e)
}

fn solve(e: &Engine, kind: &str, inputs: &[String]) -> Result<Vec<String>, String> {
    e.ctx.with(|ctx| {
        let solver: rquickjs::Object = ctx
            .globals()
            .get("__solver")
            .map_err(|err| describe(&ctx, err))?;
        let f: rquickjs::Function = solver.get(kind).map_err(|err| describe(&ctx, err))?;
        inputs
            .iter()
            .map(|x| {
                f.call::<_, String>((x.as_str(),))
                    .map_err(|err| format!("{kind}: {}", describe(&ctx, err)))
            })
            .collect()
    })
}

/// Parsing the player leaves tens of MB free inside malloc; hand them back to the system.
pub(super) fn trim() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        unsafe { malloc_trim(0) };
    }
}

fn run(rx: mpsc::Receiver<Job>) {
    let mut bg: Option<Engine> = None;
    loop {
        let job = match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(job) => job,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(e) = &bg {
                    e.rt.run_gc();
                }
                trim();
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        match job {
            Job::BgStart {
                interpreter,
                program,
                global_name,
                ytcfg,
                reply,
            } => {
                bg = None;
                let t = Instant::now();
                let r =
                    bg_engine(&interpreter, &program, &global_name, &ytcfg).map(|(e, snapshot)| {
                        tracing::info!(
                            "[js] BotGuard snapshot in {:?}, {} KB",
                            t.elapsed(),
                            e.rt.memory_usage().memory_used_size / 1024
                        );
                        bg = Some(e);
                        snapshot
                    });
                drop((interpreter, program, global_name, ytcfg));
                trim();
                let _ = reply.send(r);
            }
            Job::BgMinter {
                integrity_token,
                reply,
            } => {
                let r = match &bg {
                    Some(e) => e
                        .call("__bgMinter", &[&integrity_token])
                        .and_then(|_| e.settle())
                        .map(|_| ()),
                    None => Err("no BotGuard VM".into()),
                };
                if r.is_err() {
                    bg = None;
                } else if let Some(e) = &bg {
                    e.rt.run_gc();
                }
                trim();
                let _ = reply.send(r);
            }
            Job::BgMint { binding, reply } => {
                let r = match &bg {
                    Some(e) => e.call("__bgMint", &[&binding]).and_then(|_| e.settle()),
                    None => Err("no BotGuard VM".into()),
                };
                if let Some(e) = &bg {
                    e.rt.run_gc();
                }
                trim();
                let _ = reply.send(r);
            }
            Job::Solve {
                player_id,
                solver: code,
                n,
                sig,
                reply,
            } => {
                let t = Instant::now();
                let r = solver_engine(&code).and_then(|e| {
                    Ok(Solved {
                        n: solve(&e, "n", &n)?,
                        sig: solve(&e, "sig", &sig)?,
                    })
                });
                // The prepared source stays on disk; no player functions or tables
                // need to remain in memory while audio is playing.
                trim();
                tracing::info!(
                    "[js] player {player_id} solved and released in {:?}",
                    t.elapsed()
                );
                let _ = reply.send(r);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn direct_evaluation_preserves_non_strict_player_semantics() {
        let path = std::env::temp_dir().join(format!("youtube-solver-{}.js", uuid::Uuid::new_v4()));
        std::fs::write(
            &path,
            r#"
            _result.n = function(s) {
                implicitPlayerValue = s;
                if (this !== globalThis) throw new Error('wrong receiver');
                return implicitPlayerValue;
            };
            _result.sig = s => s.slice(1);
        "#,
        )
        .unwrap();
        let result = JsWorker::spawn()
            .solve(
                "test".into(),
                path.clone(),
                vec!["abc".into()],
                vec!["xyz".into()],
            )
            .await
            .unwrap();
        let _ = std::fs::remove_file(path.with_extension("qbc"));
        std::fs::remove_file(path).unwrap();
        assert_eq!(result.n, ["abc"]);
        assert_eq!(result.sig, ["yz"]);
    }

    #[tokio::test]
    async fn solver_is_reloaded_without_retaining_the_previous_runtime() {
        let path = std::env::temp_dir().join(format!("youtube-solver-{}.js", uuid::Uuid::new_v4()));
        let js = JsWorker::spawn();
        std::fs::write(
            &path,
            "const prefix = 'first:'; _result.n = s => prefix + s; _result.sig = s => s.slice(1);",
        )
        .unwrap();
        let first = js
            .solve(
                "test".into(),
                path.clone(),
                vec!["abc".into()],
                vec!["xyz".into()],
            )
            .await
            .unwrap();
        assert_eq!(first.n, ["first:abc"]);
        assert_eq!(first.sig, ["yz"]);
        std::fs::write(
            &path,
            "const prefix = 'second:'; _result.n = s => prefix + s; _result.sig = s => s;",
        )
        .unwrap();
        let second = js
            .solve(
                "test".into(),
                path.clone(),
                vec!["abc".into()],
                vec!["xyz".into()],
            )
            .await
            .unwrap();
        assert_eq!(second.n, ["second:abc"]);
        assert_eq!(second.sig, ["xyz"]);
        let _ = std::fs::remove_file(path.with_extension("qbc"));
        std::fs::remove_file(&path).unwrap();
        assert!(js
            .solve("test".into(), path, vec!["abc".into()], vec![])
            .await
            .is_err());
    }

    #[tokio::test]
    async fn botguard_minter_survives_collection_and_player_release() {
        let js = JsWorker::spawn();
        let interpreter = r#"globalThis.testVm = { a: function(program, ready) {
            ready(function(resolve) {
                __signals[0] = async function(integrity) {
                    return async function(binding) { return new Uint8Array([binding.length, integrity.length]); };
                };
                resolve('snapshot');
            });
        }};"#;
        assert_eq!(
            js.bg_start(
                interpreter.into(),
                "program".into(),
                "testVm".into(),
                "{}".into()
            )
            .await
            .unwrap(),
            "snapshot"
        );
        js.bg_minter("AQID".into()).await.unwrap();
        assert_eq!(js.bg_mint("ab".into()).await.unwrap(), "AgM=");
        let path = std::env::temp_dir().join(format!("youtube-solver-{}.js", uuid::Uuid::new_v4()));
        std::fs::write(&path, "_result.n = s => s; _result.sig = s => s;").unwrap();
        js.solve("test".into(), path.clone(), vec!["abc".into()], vec![])
            .await
            .unwrap();
        let _ = std::fs::remove_file(path.with_extension("qbc"));
        std::fs::remove_file(path).unwrap();
        assert_eq!(js.bg_mint("abcd".into()).await.unwrap(), "BAM=");
    }
}
