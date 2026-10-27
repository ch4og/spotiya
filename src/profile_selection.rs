use crate::{
    installation::{BASE, INIT, RECEIVER_PID, SUPERVISOR_PID},
    profiles::{self, Profile},
};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::{Duration, Instant},
};

const SETTING: &str = "profile-selection.json";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Saved {
    version: u32,
    pub selection: String,
    pub profile: Option<Profile>,
}
#[derive(Clone)]
pub struct Choice {
    pub saved: Saved,
    pub name: String,
    pub detail: String,
}
impl Choice {
    pub fn key(&self) -> &str {
        &self.saved.selection
    }
    fn id(&self) -> &str {
        self.saved
            .profile
            .as_ref()
            .map_or(self.key(), |p| p.id.as_str())
    }
}
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}
pub fn read(state: &Path) -> io::Result<Option<Saved>> {
    let file = match fs::File::open(state.join(SETTING)) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut text = String::new();
    file.take(65537).read_to_string(&mut text)?;
    if text.len() > 65536 {
        return Err(invalid("Profile selection exceeds 64 KiB"));
    }
    let mut saved: Saved = serde_json::from_str(&text)
        .map_err(|e| invalid(format!("Invalid profile selection: {e}")))?;
    if saved.version != 1
        || saved.selection.is_empty()
        || saved.selection.len() > 4096
        || saved.selection.chars().any(char::is_control)
    {
        return Err(invalid("Invalid profile selection"));
    }
    if let Some(p) = saved.profile.take() {
        saved.profile = Some(profiles::parse(&serde_json::to_string(&p)?)?);
    } else if !matches!(
        saved.selection.as_str(),
        "auto" | "mini1" | "mini2" | "cucumber" | "generic"
    ) {
        return Err(invalid("Unknown saved profile"));
    }
    Ok(Some(saved))
}
pub fn choices(state: &Path) -> io::Result<Vec<Choice>> {
    let mut out = vec![];
    for (id, name, detail) in [
        (
            "auto",
            "Automatic",
            "Saved profile.json or hardware detection",
        ),
        (
            "mini1",
            "Yandex Station Mini (YNDX-0004)",
            "Mini 1 audio and Quasar IPC",
        ),
        (
            "mini2",
            "Yandex Station Mini 2",
            "Mini 2 controls and Quasar IPC",
        ),
        (
            "cucumber",
            "Yandex Station Midi",
            "Midi audio and Quasar IPC",
        ),
        ("generic", "Generic ALSA", "Audio only"),
    ] {
        out.push(Choice {
            saved: Saved {
                version: 1,
                selection: id.into(),
                profile: None,
            },
            name: name.into(),
            detail: detail.into(),
        });
    }
    let directory = state.parent().unwrap_or(state).join("profiles");
    let mut custom = vec![];
    match fs::read_dir(&directory) {
        Ok(entries) => {
            for entry in entries.take(128) {
                let entry = entry?;
                let path = entry.path();
                if path.extension().is_none_or(|s| s != "json")
                    || matches!(
                        path.file_name().and_then(|s| s.to_str()),
                        Some("mini1.json" | "mini2.json" | "cucumber.json" | "generic.json")
                    )
                {
                    continue;
                }
                let Ok(profile) = profiles::read_file(&path) else {
                    continue;
                };
                let path = fs::canonicalize(path)?;
                let Some(key) = path.to_str() else { continue };
                custom.push(Choice {
                    name: profile.name.clone(),
                    detail: format!(
                        "{} ({})",
                        profile.id,
                        path.file_name().unwrap().to_string_lossy()
                    ),
                    saved: Saved {
                        version: 1,
                        selection: key.into(),
                        profile: Some(profile),
                    },
                });
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    custom.sort_by(|a, b| a.name.cmp(&b.name));
    out.extend(custom);
    if let Ok(Some(saved)) = read(state)
        && let Some(p) = saved.profile.as_ref()
        && !out.iter().any(|c| c.key() == saved.selection)
    {
        out.push(Choice {
            name: p.name.clone(),
            detail: format!("{} (saved copy)", p.id),
            saved,
        });
    }
    Ok(out)
}
pub fn resolve(state: &Path, selection: &str) -> io::Result<Saved> {
    let selection = if selection == "yandexmini" {
        "mini1"
    } else {
        selection
    };
    if matches!(
        selection,
        "auto" | "mini1" | "mini2" | "cucumber" | "generic"
    ) {
        return Ok(Saved {
            version: 1,
            selection: selection.into(),
            profile: None,
        });
    }
    if Path::new(selection).is_file() {
        let path = fs::canonicalize(selection)?;
        return Ok(Saved {
            version: 1,
            selection: path
                .to_str()
                .ok_or_else(|| invalid("Invalid profile path"))?
                .into(),
            profile: Some(profiles::read_file(&path)?),
        });
    }
    let options = choices(state)?;
    if let Some(c) = options.iter().find(|c| c.key() == selection) {
        return Ok(c.saved.clone());
    }
    let found: Vec<_> = options.iter().filter(|c| c.id() == selection).collect();
    match found.as_slice() {
        [c] => Ok(c.saved.clone()),
        [] => Err(invalid(
            "Unknown profile; use profiles or specify a JSON file",
        )),
        _ => Err(invalid("Several profiles have this id; select a file path")),
    }
}
fn atomic_write(state: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = state.join(format!(".profile-selection-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&temporary, state.join(SETTING))
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
fn transaction(
    state: &Path,
    saved: &Saved,
    mut apply: impl FnMut(&str) -> io::Result<()>,
) -> io::Result<()> {
    let previous = match fs::read(state.join(SETTING)) {
        Ok(data) => Some(data),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let old_id = profiles::load(None, state).ok().map(|r| r.profile.id);
    let target = profiles::resolve_saved(saved.clone(), state)?.profile.id;
    atomic_write(state, &serde_json::to_vec_pretty(saved)?)?;
    if let Err(error) = apply(&target) {
        let restored = match previous {
            Some(data) => atomic_write(state, &data),
            None => fs::remove_file(state.join(SETTING)),
        };
        // Recover the receiver even if the old selection could not be written back.
        let recovered = old_id.is_some_and(|id| apply(&id).is_ok());
        return Err(io::Error::other(format!(
            "Profile switch failed: {error}. {}; receiver recovery {}",
            match restored {
                Ok(()) => "Previous selection restored".into(),
                Err(e) => format!("Previous selection could not be restored ({e})"),
            },
            if recovered {
                "succeeded"
            } else {
                "needs checking"
            }
        )));
    }
    Ok(())
}
fn service_pid(file: &str) -> Option<u32> {
    crate::installation::live_pid(Path::new(BASE), Path::new(file))
}
fn restart(state: &Path, expected: &str) -> io::Result<()> {
    let selection = read(state)?.map_or_else(|| "auto".into(), |s| s.selection);
    // Stop may wait 8 seconds for the receiver, so allow that plus a start.
    let output = crate::installation::command(
        Path::new(INIT),
        &[OsStr::new("restart")],
        Duration::from_secs(20),
    )?;
    if !output.status.success() {
        return Err(io::Error::other(
            "Receiver restart failed; see supervisor log",
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        if let Some(pid) = service_pid(RECEIVER_PID)
            && let Ok(text) = fs::read_to_string(crate::control::STATUS)
            && let Ok(status) = serde_json::from_str::<crate::control::Status>(&text)
            && status.receiver_pid == pid
            && status.profile == expected
            && status.profile_selection == selection
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(io::Error::other(
        "Receiver did not become ready with the selected profile",
    ))
}
fn probe_audio(audio: &profiles::Audio) -> io::Result<()> {
    let probe = || -> Result<(), alsa::Error> {
        let mixer = alsa::mixer::Mixer::new(&audio.mixer_device, true)?;
        let control = mixer
            .find_selem(&alsa::mixer::SelemId::new(
                &audio.mixer_control,
                audio.mixer_index,
            ))
            .ok_or_else(|| alsa::Error::new("Profile mixer", libc::ENOENT))?;
        control.get_playback_volume(audio.mixer_channel.alsa())?;
        Ok(())
    };
    probe().map_err(|_| invalid("The selected ALSA mixer is unavailable on this speaker"))
}
pub fn select(state: &Path, selection: &str) -> io::Result<String> {
    let _lock = crate::file_lock(state, "profile-selection.lock")?;
    let saved = resolve(state, selection)?;
    let target = profiles::resolve_saved(saved.clone(), state)?;
    let managed = fs::canonicalize(state).ok().is_some_and(|path| {
        fs::canonicalize(format!("{BASE}/receiver-state")).is_ok_and(|installed| path == installed)
    });
    let running =
        managed && (service_pid(SUPERVISOR_PID).is_some() || service_pid(RECEIVER_PID).is_some());
    if running {
        probe_audio(&target.profile.audio)?;
    }
    transaction(state, &saved, |id| {
        if running { restart(state, id) } else { Ok(()) }
    })?;
    Ok(format!(
        "Profile {} selected. {}",
        target.profile.id,
        if running {
            "Receiver restarted."
        } else {
            "Saved for the next receiver start."
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("spotiya-selection-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&p).unwrap();
        p
    }
    #[test]
    fn mini1_alias_is_saved_canonically_and_packaged_profile_is_not_duplicated() {
        let root = fixture();
        let state = root.join("state");
        fs::create_dir(&state).unwrap();
        let directory = root.join("profiles");
        fs::create_dir(&directory).unwrap();
        fs::write(
            directory.join("mini1.json"),
            include_str!("../profiles/mini1.json"),
        )
        .unwrap();
        let saved = resolve(&state, "yandexmini").unwrap();
        assert_eq!(saved.selection, "mini1");
        transaction(&state, &saved, |_| Ok(())).unwrap();
        assert_eq!(read(&state).unwrap().unwrap().selection, "mini1");
        assert_eq!(profiles::load(None, &state).unwrap().profile.id, "mini1");
        assert_eq!(
            choices(&state)
                .unwrap()
                .iter()
                .filter(|c| c.id() == "mini1")
                .count(),
            1
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn selection_persists_and_custom_profiles_are_imported_as_snapshots() {
        let root = fixture();
        let state = root.join("state");
        fs::create_dir(&state).unwrap();
        let dir = root.join("profiles");
        fs::create_dir(&dir).unwrap();
        let mut p = profiles::load(Some("generic"), &state).unwrap().profile;
        p.id = "another-speaker".into();
        p.name = "Another Speaker".into();
        let source = dir.join("another.json");
        fs::write(&source, serde_json::to_vec(&p).unwrap()).unwrap();
        let spec = resolve(&state, "another-speaker").unwrap();
        transaction(&state, &spec, |_| Ok(())).unwrap();
        fs::remove_file(source).unwrap();
        assert_eq!(
            profiles::load(None, &state).unwrap().profile.id,
            "another-speaker"
        );
        assert!(
            choices(&state)
                .unwrap()
                .iter()
                .any(|c| c.id() == "another-speaker")
        );
        let mini = resolve(&state, "mini2").unwrap();
        transaction(&state, &mini, |_| Ok(())).unwrap();
        assert_eq!(profiles::load(None, &state).unwrap().profile.id, "mini2");
        let cucumber = resolve(&state, "cucumber").unwrap();
        transaction(&state, &cucumber, |_| Ok(())).unwrap();
        assert_eq!(profiles::load(None, &state).unwrap().profile.id, "cucumber");
        assert_eq!(
            profiles::load(Some("generic"), &state).unwrap().profile.id,
            "generic"
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn failed_switch_restores_previous_selection_and_recovers_receiver() {
        let root = fixture();
        let original = resolve(&root, "generic").unwrap();
        transaction(&root, &original, |_| Ok(())).unwrap();
        let next = resolve(&root, "mini2").unwrap();
        let mut attempts = vec![];
        assert!(
            transaction(&root, &next, |id| {
                attempts.push(id.to_owned());
                if id == "mini2" {
                    Err(io::Error::other("fixture failure"))
                } else {
                    Ok(())
                }
            })
            .is_err()
        );
        assert_eq!(attempts, ["mini2", "generic"]);
        assert_eq!(profiles::load(None, &root).unwrap().profile.id, "generic");
        fs::write(root.join(SETTING), b"broken").unwrap();
        assert!(read(&root).is_err());
        transaction(&root, &original, |_| Ok(())).unwrap();
        assert_eq!(profiles::load(None, &root).unwrap().profile.id, "generic");
        fs::remove_dir_all(root).unwrap();
    }
}
