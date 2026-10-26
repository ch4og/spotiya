//! Cache only bytecode produced by this executable from the local player source.
//! QuickJS bytecode is not portable and must never be loaded from a download.

use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use rquickjs::{qjs, Ctx, Value};

use super::js::describe;

const MAGIC: &[u8; 8] = b"SPOTQBC1";
const HEADER: usize = 32;
const MAX_CACHE: usize = 16 << 20;

fn fingerprint(path: &Path) -> std::io::Result<u64> {
    let metadata = fs::metadata(path)?;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
    )
        .hash(&mut hash);
    Ok(hash.finish())
}

fn checksum(bytes: &[u8]) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hash);
    hash.finish()
}

fn cached(path: &Path, build: u64, source: u64) -> Option<Vec<u8>> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
        || metadata.len() > MAX_CACHE as u64
    {
        return None;
    }
    let mut header = [0; HEADER];
    file.read_exact(&mut header).ok()?;
    let number = |offset| u64::from_le_bytes(header[offset..offset + 8].try_into().unwrap());
    if &header[..8] != MAGIC || number(8) != build || number(16) != source {
        return None;
    }
    let mut data = Vec::with_capacity(metadata.len() as usize);
    data.extend_from_slice(&header);
    file.take((MAX_CACHE - HEADER) as u64 + 1)
        .read_to_end(&mut data)
        .ok()?;
    if data.len() > MAX_CACHE {
        return None;
    }
    if data.len() <= HEADER {
        return None;
    }
    if number(24) != checksum(&data[HEADER..]) {
        return None;
    }
    Some(data)
}

fn save(path: &Path, build: u64, source: u64, bytecode: &[u8]) -> std::io::Result<()> {
    if bytecode.len() > MAX_CACHE - HEADER {
        return Err(std::io::Error::other("bytecode cache is too large"));
    }
    let temporary = path.with_extension(format!("qbc.{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(MAGIC)?;
        file.write_all(&build.to_le_bytes())?;
        file.write_all(&source.to_le_bytes())?;
        file.write_all(&checksum(bytecode).to_le_bytes())?;
        file.write_all(bytecode)?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    let _ = fs::remove_file(temporary);
    result
}

fn checked<'js>(ctx: &Ctx<'js>, raw: qjs::JSValue) -> Result<Value<'js>, String> {
    // Every caller passes an owned result from this context's QuickJS runtime.
    let value = unsafe { Value::from_raw(ctx.clone(), raw) };
    if value.is_exception() {
        Err(describe(ctx, rquickjs::Error::Exception))
    } else {
        Ok(value)
    }
}

pub(super) fn eval(ctx: &Ctx<'_>, source: &Path) -> Result<(), String> {
    let raw_ctx = ctx.as_raw().as_ptr();
    let cache = source.with_extension("qbc");
    let identity = std::env::current_exe()
        .and_then(|p| fingerprint(&p))
        .ok()
        .zip(fingerprint(source).ok());
    let mut compiled = None;
    if let Some((build, source_id)) = identity {
        if let Some(data) = cached(&cache, build, source_id) {
            // Only our own private, build-matched cache reaches JS_ReadObject.
            // Without ROM_DATA QuickJS copies everything it needs from this buffer.
            let raw = unsafe {
                qjs::JS_ReadObject(
                    raw_ctx,
                    data[HEADER..].as_ptr(),
                    (data.len() - HEADER) as _,
                    qjs::JS_READ_OBJ_BYTECODE as i32,
                )
            };
            compiled = checked(ctx, raw).ok();
        }
    }
    let compiled = match compiled {
        Some(compiled) => compiled,
        None => {
            let source_text =
                fs::read_to_string(source).map_err(|e| format!("{}: {e}", source.display()))?;
            let code = CString::new(source_text).map_err(|e| e.to_string())?;
            // Preserve the non-strict semantics of the original Function constructor.
            let raw = unsafe {
                qjs::JS_Eval(
                    raw_ctx,
                    code.as_ptr(),
                    code.as_bytes().len() as _,
                    c"youtube-player.js".as_ptr(),
                    (qjs::JS_EVAL_TYPE_GLOBAL | qjs::JS_EVAL_FLAG_COMPILE_ONLY) as i32,
                )
            };
            let compiled = checked(ctx, raw)?;
            drop(code);
            // Return parser allocations before allocating the serialized cache.
            unsafe { qjs::JS_RunGC(qjs::JS_GetRuntime(raw_ctx)) };
            super::js::trim();
            if let Some((build, source_id)) = identity {
                let mut size = 0;
                let buffer = unsafe {
                    qjs::JS_WriteObject(
                        raw_ctx,
                        &mut size,
                        compiled.as_raw(),
                        qjs::JS_WRITE_OBJ_BYTECODE as i32,
                    )
                };
                if !buffer.is_null() {
                    let bytes = unsafe { std::slice::from_raw_parts(buffer, size as usize) };
                    if let Err(e) = save(&cache, build, source_id, bytes) {
                        tracing::debug!("[js] bytecode cache: {e}");
                    }
                    unsafe { qjs::js_free(raw_ctx, buffer.cast()) };
                } else {
                    // Cache failure must not discard a successfully compiled player.
                    let _ = ctx.catch();
                }
            }
            compiled
        }
    };
    // JS_EvalFunction consumes its argument; retain the guard's own reference.
    let raw =
        unsafe { qjs::JS_EvalFunction(raw_ctx, qjs::JS_DupValue(raw_ctx, compiled.as_raw())) };
    checked(ctx, raw)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rquickjs::{Context, Runtime};
    use std::os::unix::fs::PermissionsExt;

    struct Fixture {
        directory: std::path::PathBuf,
        source: std::path::PathBuf,
    }
    impl Fixture {
        fn new(source: &str) -> Self {
            let directory =
                std::env::temp_dir().join(format!("yt-bytecode-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&directory).unwrap();
            let path = directory.join("player.js");
            fs::write(&path, source).unwrap();
            Self {
                directory,
                source: path,
            }
        }
        fn run(&self) -> String {
            let runtime = Runtime::new().unwrap();
            let context = Context::full(&runtime).unwrap();
            context.with(|ctx| {
                eval(&ctx, &self.source).unwrap();
                ctx.eval::<String, _>("String(answer)").unwrap()
            })
        }
        fn cache(&self) -> std::path::PathBuf {
            self.source.with_extension("qbc")
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.directory).unwrap();
        }
    }

    #[test]
    fn cached_script_preserves_function_source_and_non_strict_semantics() {
        let fixture = Fixture::new("function original(){ implicit = 'ok'; return this === globalThis && implicit; } var answer = original() + original.toString();");
        let first = fixture.run();
        assert!(fixture.cache().is_file());
        assert!(first.starts_with("okfunction original"));
        assert_eq!(fixture.run(), first);
    }

    #[test]
    fn corrupted_cache_is_rebuilt_from_source() {
        let fixture = Fixture::new("var answer = 42;");
        assert_eq!(fixture.run(), "42");
        let mut data = fs::read(fixture.cache()).unwrap();
        *data.last_mut().unwrap() ^= 1;
        fs::write(fixture.cache(), &data).unwrap();
        assert_eq!(fixture.run(), "42");
        assert_ne!(fs::read(fixture.cache()).unwrap(), data);
    }

    #[test]
    fn changed_build_or_source_invalidates_the_cache() {
        let fixture = Fixture::new("var answer = 42;");
        assert_eq!(fixture.run(), "42");
        let mut data = fs::read(fixture.cache()).unwrap();
        data[8] ^= 1;
        fs::write(fixture.cache(), &data).unwrap();
        assert_eq!(fixture.run(), "42");
        assert_ne!(fs::read(fixture.cache()).unwrap(), data);
        let next = fixture.directory.join("next.js");
        fs::write(&next, "var answer = 43;").unwrap();
        fs::rename(next, &fixture.source).unwrap();
        assert_eq!(fixture.run(), "43");
        assert_eq!(fixture.run(), "43");
    }

    #[test]
    fn writable_cache_and_symlink_are_replaced_without_following_them() {
        let fixture = Fixture::new("var answer = 42;");
        assert_eq!(fixture.run(), "42");
        fs::set_permissions(fixture.cache(), fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(fixture.run(), "42");
        assert_eq!(fs::metadata(fixture.cache()).unwrap().mode() & 0o777, 0o600);
        fs::remove_file(fixture.cache()).unwrap();
        let target = fixture.directory.join("untouched");
        fs::write(&target, "sentinel").unwrap();
        std::os::unix::fs::symlink(&target, fixture.cache()).unwrap();
        assert_eq!(fixture.run(), "42");
        assert_eq!(fs::read_to_string(target).unwrap(), "sentinel");
        assert!(!fs::symlink_metadata(fixture.cache()).unwrap().is_symlink());
    }

    #[test]
    fn cache_write_failure_does_not_prevent_playback() {
        let fixture = Fixture::new("var answer = 42;");
        fs::create_dir(fixture.cache()).unwrap();
        assert_eq!(fixture.run(), "42");
        assert_eq!(fs::read_dir(&fixture.directory).unwrap().count(), 2);
    }

    #[test]
    fn oversized_cache_is_rejected_before_reading_it() {
        let fixture = Fixture::new("var answer = 42;");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(fixture.cache())
            .unwrap();
        file.set_len(MAX_CACHE as u64 + 1).unwrap();
        assert!(cached(&fixture.cache(), 0, 0).is_none());
        drop(file);
    }
}
