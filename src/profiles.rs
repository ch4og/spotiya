use serde::{Deserialize, Serialize};
use std::{
    ffi::{CString, c_char},
    fs,
    io::{self, Read},
    net::SocketAddr,
    path::{Component, Path},
};

const MINI1: &str = include_str!("../profiles/mini1.json");
const MINI2: &str = include_str!("../profiles/mini2.json");
const CUCUMBER: &str = include_str!("../profiles/cucumber.json");
const GENERIC: &str = include_str!("../profiles/generic.json");

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Audio {
    pub device: String,
    pub format: String,
    pub mixer_device: String,
    pub mixer_control: String,
    pub mixer_index: u32,
    pub mixer_channel: Channel,
}
impl Default for Audio {
    fn default() -> Self {
        Self {
            device: "default".into(),
            format: "S16".into(),
            mixer_device: "default".into(),
            mixer_control: "Master".into(),
            mixer_index: 0,
            mixer_channel: Channel::FrontLeft,
        }
    }
}
#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    #[default]
    FrontLeft,
    FrontRight,
    Mono,
}
impl Channel {
    pub fn alsa(self) -> alsa::mixer::SelemChannelId {
        use alsa::mixer::SelemChannelId;
        match self {
            Self::FrontLeft => SelemChannelId::FrontLeft,
            Self::FrontRight => SelemChannelId::FrontRight,
            Self::Mono => SelemChannelId::mono(),
        }
    }
}
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceProtocol {
    Quasar,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Voice {
    pub protocol: VoiceProtocol,
    pub address: SocketAddr,
}
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Driver {
    Mini2,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Keys {
    pub alice: u32,
    pub volume_up: u32,
    pub volume_down: u32,
}
impl Default for Keys {
    fn default() -> Self {
        Self {
            alice: 582,
            volume_up: 115,
            volume_down: 114,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Controls {
    pub driver: Driver,
    pub input_device: Option<String>,
    pub input_name: Option<String>,
    #[serde(default)]
    pub keys: Keys,
    pub ring_prefix: Option<String>,
    pub display_prefix: Option<String>,
    /// Directory of Quasar's ring patterns, played for next, previous and play/pause.
    pub ring_animations: Option<String>,
    /// Quasar's colour correction for this body colour (`ledconfigs/*.json`).
    pub ring_colors: Option<String>,
    /// Directory of Quasar's clock animations, played for next, previous and pause.
    pub clock_animations: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub version: u32,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub audio: Audio,
    pub voice: Option<Voice>,
    pub controls: Option<Controls>,
}
#[derive(Clone, Copy, Default, Serialize)]
pub struct Capabilities {
    pub buttons: bool,
    pub ring: bool,
    pub display: bool,
    pub voice: bool,
}
impl Capabilities {
    pub fn controls(self) -> bool {
        self.buttons || self.ring || self.display
    }
}
#[derive(Serialize)]
pub struct Resolved {
    pub profile: Profile,
    pub capabilities: Capabilities,
    pub source: String,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
fn valid_text(s: &str, limit: usize) -> bool {
    !s.trim().is_empty() && s.len() <= limit && !s.chars().any(char::is_control)
}
fn valid_path(s: &str) -> bool {
    valid_text(s, 80)
        && Path::new(s).is_absolute()
        && !Path::new(s).components().any(|p| p == Component::ParentDir)
}
pub(crate) fn parse(text: &str) -> io::Result<Profile> {
    let p: Profile =
        serde_json::from_str(text).map_err(|e| invalid(format!("Invalid device profile: {e}")))?;
    if p.version != 1
        || p.id.is_empty()
        || p.id.len() > 64
        || !p
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        || !valid_text(&p.name, 96)
    {
        return Err(invalid("Invalid profile version, id or name"));
    }
    for value in [
        &p.audio.device,
        &p.audio.mixer_device,
        &p.audio.mixer_control,
    ] {
        if !valid_text(value, 128) {
            return Err(invalid("Invalid ALSA device or mixer name"));
        }
    }
    if p.audio
        .format
        .parse::<librespot::playback::config::AudioFormat>()
        .is_err()
    {
        return Err(invalid("Unsupported audio format"));
    }
    if let Some(v) = &p.voice
        && (!v.address.ip().is_loopback() || v.address.port() == 0)
    {
        return Err(invalid(
            "Quasar IPC address must be a loopback address with a nonzero port",
        ));
    }
    if let Some(c) = &p.controls {
        if c.input_name
            .as_ref()
            .is_some_and(|name| !valid_text(name, 128))
        {
            return Err(invalid("Invalid input device name"));
        }
        let keys = [c.keys.alice, c.keys.volume_up, c.keys.volume_down];
        if keys.iter().any(|k| *k == 0 || *k >= 768)
            || keys[0] == keys[1]
            || keys[0] == keys[2]
            || keys[1] == keys[2]
        {
            return Err(invalid(
                "Button codes must be distinct Linux key codes in 1..767",
            ));
        }
        for path in [
            &c.input_device,
            &c.ring_prefix,
            &c.display_prefix,
            &c.ring_animations,
            &c.ring_colors,
            &c.clock_animations,
        ]
        .into_iter()
        .flatten()
        {
            if !valid_path(path) {
                return Err(invalid(
                    "Control paths must be absolute, bounded and contain no parent traversal",
                ));
            }
        }
    }
    Ok(p)
}
fn rooted(root: &Path, path: &str) -> std::path::PathBuf {
    root.join(path.trim_start_matches('/'))
}
fn detect(root: &Path) -> &'static str {
    // Mini 1's device tree identifies only the Amlogic SoC.
    if let Ok(file) = fs::File::open(rooted(root, "/system/vendor/quasar/quasar.cfg"))
        && let Ok(config) = serde_json::from_reader::<_, serde_json::Value>(file.take(256 * 1024))
    {
        match config
            .pointer("/common/deviceType")
            .and_then(|v| v.as_str())
        {
            Some("yandexmini") => return "mini1",
            Some("yandexmini_2") => return "mini2",
            Some("cucumber") => return "cucumber",
            _ => {}
        }
    }
    let compatible = fs::read(rooted(root, "/proc/device-tree/compatible")).unwrap_or_default();
    let model = fs::read(rooted(root, "/proc/device-tree/model")).unwrap_or_default();
    if compatible
        .split(|b| *b == 0)
        .any(|s| s == b"yandex,station-mini2")
        || model.split(|b| *b == 0).next() == Some(b"Yandex Station Mini 2".as_slice())
    {
        "mini2"
    } else {
        "generic"
    }
}
fn capabilities(p: &Profile, root: &Path) -> Capabilities {
    let mut caps = Capabilities {
        voice: p.voice.is_some(),
        ..Default::default()
    };
    if let Some(c) = &p.controls {
        caps.buttons = c.input_device.as_ref().is_some_and(|s| {
            rooted(root, s).exists()
                && c.input_name.as_ref().is_none_or(|expected| {
                    Path::new(s)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|event| {
                            fs::read_to_string(rooted(
                                root,
                                &format!("/sys/class/input/{event}/device/name"),
                            ))
                            .is_ok_and(|name| name.trim() == expected)
                        })
                })
        });
        caps.ring = c.ring_prefix.as_ref().is_some_and(|prefix| {
            ["red", "green", "blue"].iter().all(|color| {
                (1..=6).all(|segment| {
                    rooted(root, &format!("{prefix}{color}:ring{segment}/brightness")).exists()
                })
            })
        });
        caps.display = c.display_prefix.as_ref().is_some_and(|prefix| {
            (1..=4).all(|digit| {
                (b'A'..=b'G').all(|segment| {
                    rooted(
                        root,
                        &format!("{prefix}{}{digit}/brightness", char::from(segment)),
                    )
                    .exists()
                })
            }) && ["PLUS1", "PLUS2", "PLUS3", "PLUS4", "MINUS", "DEGREE"]
                .iter()
                .all(|suffix| rooted(root, &format!("{prefix}{suffix}/brightness")).exists())
        });
    }
    caps
}
pub fn load(selection: Option<&str>, state_dir: &Path) -> io::Result<Resolved> {
    let saved = if let Some(choice) = selection {
        Some(crate::profile_selection::resolve(state_dir, choice)?)
    } else {
        crate::profile_selection::read(state_dir)?
    };
    if let Some(saved) = saved {
        resolve_saved(saved, state_dir)
    } else {
        load_at("auto", state_dir, Path::new("/"))
    }
}
pub(crate) fn resolve_saved(
    saved: crate::profile_selection::Saved,
    state_dir: &Path,
) -> io::Result<Resolved> {
    if let Some(profile) = saved.profile {
        let caps = capabilities(&profile, Path::new("/"));
        Ok(Resolved {
            profile,
            capabilities: caps,
            source: saved.selection,
        })
    } else {
        load_at(&saved.selection, state_dir, Path::new("/"))
    }
}
pub(crate) fn read_file(path: &Path) -> io::Result<Profile> {
    let mut text = String::new();
    fs::File::open(path)?
        .take(32769)
        .read_to_string(&mut text)?;
    if text.len() > 32768 {
        return Err(invalid("Device profile exceeds 32 KiB"));
    }
    parse(&text)
}
fn load_at(selection: &str, state_dir: &Path, root: &Path) -> io::Result<Resolved> {
    let saved = state_dir.join("profile.json");
    let selection = if selection == "auto" {
        if saved.exists() {
            saved
                .to_str()
                .ok_or_else(|| invalid("Invalid profile path"))?
        } else {
            detect(root)
        }
    } else {
        selection
    };
    let profile = match selection {
        "mini1" => parse(MINI1)?,
        "mini2" => parse(MINI2)?,
        "cucumber" => parse(CUCUMBER)?,
        "generic" => parse(GENERIC)?,
        path => read_file(Path::new(path))?,
    };
    let caps = capabilities(&profile, root);
    Ok(Resolved {
        profile,
        capabilities: caps,
        source: selection.into(),
    })
}
pub fn list(state_dir: &Path) -> io::Result<()> {
    for choice in crate::profile_selection::choices(state_dir)? {
        println!("{}  {} — {}", choice.key(), choice.name, choice.detail);
    }
    Ok(())
}

#[repr(C)]
struct NativeAnimation<T> {
    frames: *const T,
    count: u32,
}
impl<T> NativeAnimation<T> {
    fn of(frames: &[T]) -> Self {
        Self {
            frames: frames.as_ptr(),
            count: frames.len() as u32,
        }
    }
}
type RingAnimation = NativeAnimation<crate::animations::RingFrame>;
type ClockAnimation = NativeAnimation<crate::animations::ClockFrame>;
/// Mirrors `struct mini2_config` in controls/mini2/config.h.
#[repr(C)]
struct NativeConfig {
    input: *const c_char,
    ring_prefix: *const c_char,
    display_prefix: *const c_char,
    alice: u32,
    volume_up: u32,
    volume_down: u32,
    next: RingAnimation,
    previous: RingAnimation,
    pause: RingAnimation,
    resume: RingAnimation,
    clock_next: ClockAnimation,
    clock_previous: ClockAnimation,
    clock_pause: ClockAnimation,
    volume: RingAnimation,
}
unsafe extern "C" {
    fn mini2_buttons_run(config: *const NativeConfig) -> i32;
}
impl Controls {
    pub fn run(&self, caps: Capabilities) -> io::Result<i32> {
        fn string(value: &Option<String>, enabled: bool) -> io::Result<Option<CString>> {
            value
                .as_deref()
                .filter(|_| enabled)
                .map(CString::new)
                .transpose()
                .map_err(|e| invalid(e.to_string()))
        }
        let input = string(&self.input_device, caps.buttons)?;
        let ring = string(&self.ring_prefix, caps.ring)?;
        let display = string(&self.display_prefix, caps.display)?;
        let pointer = |s: &Option<CString>| s.as_ref().map_or(std::ptr::null(), |s| s.as_ptr());
        // The frames outlive the driver call below, which reads them until it returns.
        let animations = match (&self.ring_animations, caps.ring) {
            (Some(directory), true) => crate::animations::Ring::load(
                Path::new(directory),
                self.ring_colors.as_deref().map(Path::new),
            ),
            _ => crate::animations::Ring::default(),
        };
        let clock = match (&self.clock_animations, caps.display) {
            (Some(directory), true) => crate::animations::Clock::load(Path::new(directory)),
            _ => crate::animations::Clock::default(),
        };
        let config = NativeConfig {
            input: pointer(&input),
            ring_prefix: pointer(&ring),
            display_prefix: pointer(&display),
            alice: self.keys.alice,
            volume_up: self.keys.volume_up,
            volume_down: self.keys.volume_down,
            next: NativeAnimation::of(&animations.next),
            previous: NativeAnimation::of(&animations.previous),
            pause: NativeAnimation::of(&animations.pause),
            resume: NativeAnimation::of(&animations.resume),
            clock_next: NativeAnimation::of(&clock.next),
            clock_previous: NativeAnimation::of(&clock.previous),
            clock_pause: NativeAnimation::of(&clock.pause),
            volume: NativeAnimation::of(&animations.volume),
        };
        Ok(unsafe { mini2_buttons_run(&config) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("spotiya-profile-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&dir).unwrap();
            Self(dir)
        }
        fn file(&self, path: &str, data: impl AsRef<[u8]>) {
            let path = rooted(&self.0, path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, data).unwrap();
        }
        fn load(&self, selection: &str) -> io::Result<Resolved> {
            load_at(selection, &self.0.join("state"), &self.0)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn unknown_hardware_never_claims_mini2_controls_or_native_ipc() {
        let f = Fixture::new();
        f.file("/dev/input/event0", b"");
        f.file("/sys/class/leds/mini2:red:ring1/brightness", b"0");
        let r = f.load("auto").unwrap();
        assert_eq!(r.profile.id, "generic");
        assert!(!r.capabilities.controls() && !r.capabilities.voice);
    }
    #[test]
    fn mini1_uses_quasar_model_on_generic_device_tree_without_mini2_controls() {
        let f = Fixture::new();
        f.file("/proc/device-tree/compatible", b"amlogic, axg\0");
        f.file("/proc/device-tree/model", b"Amlogic\0");
        f.file(
            "/system/vendor/quasar/quasar.cfg",
            br#"{"common":{"deviceType":"yandexmini"}}"#,
        );
        f.file("/dev/input/event2", b"");
        f.file("/sys/class/input/event2/device/name", b"adc_keypad\n");
        let r = f.load("auto").unwrap();
        assert_eq!(r.profile.id, "mini1");
        assert_eq!(r.profile.audio.device, "default");
        assert_eq!(r.profile.audio.mixer_control, "Master");
        assert_eq!(
            r.profile.voice.unwrap().address,
            "127.0.0.1:9875".parse().unwrap()
        );
        assert!(r.capabilities.voice);
        assert!(!r.capabilities.controls());
    }
    #[test]
    fn lite_and_invalid_model_metadata_do_not_enable_mini1_ipc() {
        let f = Fixture::new();
        f.file("/proc/device-tree/model", b"Amlogic\0");
        for config in [
            br#"{"common":{"deviceType":"yandexmicro"}}"#.as_slice(),
            b"not json",
        ] {
            f.file("/system/vendor/quasar/quasar.cfg", config);
            let r = f.load("auto").unwrap();
            assert_eq!(r.profile.id, "generic");
            assert!(!r.capabilities.voice);
        }
        assert_eq!(f.load("mini1").unwrap().profile.id, "mini1");
    }
    #[test]
    fn mini2_detection_probes_whole_devices_and_separates_capabilities() {
        let f = Fixture::new();
        f.file(
            "/proc/device-tree/compatible",
            b"yandex,station-mini2\0amlogic, axg\0",
        );
        f.file("/dev/input/event0", b"");
        f.file("/sys/class/input/event0/device/name", b"sar0_pad0\n");
        for color in ["red", "green", "blue"] {
            for segment in 1..=6 {
                f.file(
                    &format!("/sys/class/leds/mini2:{color}:ring{segment}/brightness"),
                    b"0",
                );
            }
        }
        let r = f.load("auto").unwrap();
        assert_eq!(r.profile.id, "mini2");
        assert!(
            r.capabilities.buttons
                && r.capabilities.ring
                && !r.capabilities.display
                && r.capabilities.voice
        );
        for digit in 1..=4 {
            for segment in b'A'..=b'G' {
                f.file(
                    &format!(
                        "/sys/class/leds/mini2:white:clock{}{digit}/brightness",
                        char::from(segment)
                    ),
                    b"0",
                );
            }
        }
        for suffix in ["PLUS1", "PLUS2", "PLUS3", "PLUS4", "MINUS", "DEGREE"] {
            f.file(
                &format!("/sys/class/leds/mini2:white:clock{suffix}/brightness"),
                b"0",
            );
        }
        assert!(f.load("auto").unwrap().capabilities.display);
        f.file("/sys/class/input/event0/device/name", b"headphones\n");
        assert!(!f.load("auto").unwrap().capabilities.buttons);
        assert!(f.load("auto").unwrap().capabilities.display);
        fs::remove_file(rooted(&f.0, "/sys/class/leds/mini2:blue:ring6/brightness")).unwrap();
        assert!(!f.load("auto").unwrap().capabilities.ring);
        assert!(!f.load("generic").unwrap().capabilities.controls());
    }
    #[test]
    fn cucumber_uses_quasar_without_controls() {
        let f = Fixture::new();
        f.file(
            "/system/vendor/quasar/quasar.cfg",
            br#"{"common":{"deviceType":"cucumber"}}"#,
        );
        let r = f.load("auto").unwrap();
        assert_eq!(r.profile.id, "cucumber");
        assert_eq!(r.profile.audio.device, "default");
        assert_eq!(r.profile.audio.mixer_control, "Master");
        assert_eq!(
            r.profile.voice.unwrap().address,
            "127.0.0.1:9875".parse().unwrap()
        );
        assert!(r.capabilities.voice);
        assert!(!r.capabilities.controls());
    }
    #[test]
    fn saved_custom_profile_overrides_detection_and_survives_explicit_selection() {
        let f = Fixture::new();
        let mut p: serde_json::Value = serde_json::from_str(GENERIC).unwrap();
        p["id"] = "my-station".into();
        p["audio"]["device"] = "plughw:1,0".into();
        p["audio"]["mixer_control"] = "PCM".into();
        p["audio"]["mixer_channel"] = "mono".into();
        p["audio"]["format"] = "S32".into();
        p["voice"] = serde_json::json!({"protocol":"quasar","address":"[::1]:12345"});
        f.file("state/profile.json", serde_json::to_vec(&p).unwrap());
        let r = f.load("auto").unwrap();
        assert_eq!(r.profile.id, "my-station");
        assert_eq!(r.profile.audio.device, "plughw:1,0");
        assert!(r.capabilities.voice && !r.capabilities.controls());
        assert_eq!(r.profile.voice.unwrap().address.port(), 12345);
        assert_eq!(f.load("mini2").unwrap().profile.id, "mini2");
        f.file("state/profile.json", b"{\"typo\":true}");
        assert!(f.load("auto").is_err());
    }
    #[test]
    fn invalid_profiles_fail_before_touching_hardware() {
        let mut p: serde_json::Value = serde_json::from_str(MINI2).unwrap();
        for (path, value) in [
            ("/version", serde_json::json!(2)),
            ("/audio/format", serde_json::json!("bad")),
            ("/audio/mixer_control", serde_json::json!("bad\u{0}name")),
            ("/voice/address", serde_json::json!("192.168.1.1:9875")),
            ("/controls/keys/alice", serde_json::json!(115)),
            (
                "/controls/input_device",
                serde_json::json!("/dev/../etc/passwd"),
            ),
            ("/controls/driver", serde_json::json!("unknown")),
        ] {
            let original = p.pointer(path).unwrap().clone();
            *p.pointer_mut(path).unwrap() = value;
            assert!(parse(&p.to_string()).is_err(), "{path}");
            *p.pointer_mut(path).unwrap() = original;
        }
        let f = Fixture::new();
        f.file("state/profile.json", vec![b' '; 32769]);
        assert!(f.load("auto").is_err());
    }
}
