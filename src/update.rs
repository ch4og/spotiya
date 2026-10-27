use crate::installation::{self, ASSET, Temporary, error};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
pub(crate) const REPO: &str = "klockky/spotiya";
const MAX_ARCHIVE: u64 = 16 * 1024 * 1024;
const MAX_EXPANDED: u64 = 32 * 1024 * 1024;

pub fn parse_version(text: &str) -> io::Result<[u32; 3]> {
    let mut result = [0; 3];
    let parts: Vec<_> = text.strip_prefix('v').unwrap_or(text).split('.').collect();
    if parts.len() != 3 {
        return Err(error("Expected a stable version such as v0.8.0"));
    }
    for (out, part) in result.iter_mut().zip(parts) {
        if part.is_empty() || part.len() > 6 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(error("Invalid release version"));
        }
        *out = part.parse().map_err(|_| error("Invalid release version"))?;
    }
    Ok(result)
}
pub(crate) fn checksum(text: &str, name: &str) -> io::Result<[u8; 32]> {
    let mut found = None;
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 2 || fields[1].trim_start_matches('*') != name {
            continue;
        }
        if found.is_some()
            || fields[0].len() != 64
            || !fields[0].bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Err(error("Invalid or duplicate SHA256 checksum"));
        }
        let mut bytes = [0; 32];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&fields[0][i * 2..i * 2 + 2], 16)
                .map_err(|_| error("Invalid SHA256 checksum"))?;
        }
        found = Some(bytes);
    }
    found.ok_or_else(|| error("Archive is missing from SHA256SUMS"))
}
pub(crate) fn sha256(path: &Path) -> io::Result<[u8; 32]> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 32768];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(hash.finalize().into())
}
fn verify(path: &Path, sums: &str) -> io::Result<()> {
    let expected = checksum(sums, ASSET)?;
    if fs::metadata(path)?.len() > MAX_ARCHIVE {
        return Err(error("Archive exceeds 16 MiB"));
    }
    if sha256(path)? != expected {
        return Err(error(
            "Archive SHA256 mismatch; installation was not started",
        ));
    }
    Ok(())
}
fn extract(archive: &Path, destination: &Path) -> io::Result<PathBuf> {
    let file = fs::File::open(archive)?;
    let mut archive =
        tar::Archive::new(flate2::read::GzDecoder::new(file).take(MAX_EXPANDED + 1024 * 1024));
    let mut total = 0u64;
    for (index, entry) in archive.entries()?.enumerate() {
        if index >= 128 {
            return Err(error("Too many archive entries"));
        }
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let parts: Vec<_> = path.components().collect();
        if parts.is_empty()
            || parts
                .iter()
                .any(|p| !matches!(p, std::path::Component::Normal(_)))
            || parts.len() > 4
            || parts[0].as_os_str() != "spotiya"
        {
            return Err(error("Unsafe path in release archive"));
        }
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            continue;
        }
        if !kind.is_file() {
            return Err(error(
                "Links and special files are not allowed in release archives",
            ));
        }
        let allowed = matches!(
            path.to_str(),
            Some(
                "spotiya/spotiya"
                    | "spotiya/README.md"
                    | "spotiya/LICENSE"
                    | "spotiya/bin/spotiya"
                    | "spotiya/scripts/install.sh"
                    | "spotiya/profiles/mini1.json"
                    | "spotiya/profiles/mini2.json"
                    | "spotiya/profiles/cucumber.json"
                    | "spotiya/profiles/generic.json"
                    | "spotiya/packaging/run.sh"
                    | "spotiya/packaging/start.sh"
                    | "spotiya/packaging/stop.sh"
                    | "spotiya/packaging/boot.sh"
                    | "spotiya/packaging/service.sh"
                    | "spotiya/packaging/cli.sh"
                    | "spotiya/packaging/S99spotify-connect"
            )
        );
        if !allowed {
            return Err(error("Unexpected file in release archive"));
        }
        total = total
            .checked_add(entry.size())
            .ok_or_else(|| error("Archive size overflow"))?;
        if total > MAX_EXPANDED {
            return Err(error("Expanded archive exceeds 32 MiB"));
        }
        let target = destination.join(path);
        fs::create_dir_all(target.parent().unwrap())?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&target)?;
        io::copy(&mut entry, &mut file)?;
        if target.file_name().is_some_and(|s| s == "spotiya") {
            fs::set_permissions(target, fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(destination.join("spotiya"))
}
/// Copies a local archive into a private file first, so replacing the source after
/// verification cannot change what gets installed.
pub(crate) fn copy_limited(source: &Path, copy: &Path, limit: u64) -> io::Result<()> {
    let mut input = fs::File::open(source)?.take(limit + 1);
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(copy)?;
    if io::copy(&mut input, &mut output)? > limit {
        return Err(error(format!(
            "Archive exceeds {} MiB",
            limit / 1024 / 1024
        )));
    }
    Ok(())
}
pub fn local(archive: &Path, sums: &Path) -> io::Result<()> {
    let text = limited_file(sums, 65536)?;
    let stage = Temporary::new(Path::new("/tmp"), "spotiya-update")?;
    let copy = stage.0.join(ASSET);
    copy_limited(archive, &copy, MAX_ARCHIVE)?;
    install_verified(&copy, &text, &stage.0, None)
}
fn install_verified(
    archive: &Path,
    sums: &str,
    stage: &Path,
    expected: Option<&str>,
) -> io::Result<()> {
    verify(archive, sums)?;
    let root = extract(archive, stage)?;
    let binary = root.join("spotiya");
    if let Some(expected) = expected
        && parse_version(&installation::version(&binary)?)? != parse_version(expected)?
    {
        return Err(error("Binary version does not match release tag"));
    }
    installation::install(&root, &binary, expected.is_some())
}
pub fn limited_file(path: &Path, limit: u64) -> io::Result<String> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(error("File is too large"));
    }
    String::from_utf8(bytes).map_err(|_| error("Invalid UTF-8 file"))
}
#[derive(serde::Deserialize)]
pub(crate) struct Release {
    pub tag_name: String,
    pub draft: bool,
    pub prerelease: bool,
    pub assets: Vec<Asset>,
}
#[derive(serde::Deserialize)]
pub(crate) struct Asset {
    pub name: String,
    pub size: u64,
}
impl Release {
    /// A published asset with this name whose size is within `1..=limit`.
    pub fn asset(&self, name: &str, limit: u64) -> Option<&Asset> {
        self.assets
            .iter()
            .find(|a| a.name == name && a.size > 0 && a.size <= limit)
    }
}
/// Fetches release metadata for `latest` or `tags/vX.Y.Z`; `None` when GitHub has no such release.
pub(crate) async fn release(
    client: &reqwest::Client,
    endpoint: &str,
) -> io::Result<Option<Release>> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/{endpoint}");
    let mut response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|_| error("Cannot contact GitHub Releases; check DNS and Internet access"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(error(format!(
            "GitHub returned HTTP {}; retry later",
            response.status().as_u16()
        )));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| error("Release metadata interrupted"))?
    {
        if bytes.len() + chunk.len() > 1024 * 1024 {
            return Err(error("Release metadata is too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| error("Invalid GitHub release metadata"))
}
pub(crate) fn client() -> io::Result<reqwest::Client> {
    reqwest::Client::builder()
        .https_only(true)
        .user_agent(concat!("spotiya/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|_| error("Cannot initialize HTTPS client"))
}
async fn download(client: &reqwest::Client, url: &str, path: &Path, limit: u64) -> io::Result<()> {
    download_progress(client, url, path, limit, |_, _| {}).await
}
pub(crate) async fn download_progress(
    client: &reqwest::Client,
    url: &str,
    path: &Path,
    limit: u64,
    mut progress: impl FnMut(u64, Option<u64>),
) -> io::Result<()> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|_| error("Release download failed; check DNS and Internet access"))?;
    if !response.status().is_success() {
        return Err(error(format!(
            "Release download returned HTTP {}",
            response.status().as_u16()
        )));
    }
    if response.content_length().is_some_and(|n| n > limit) {
        return Err(error("Release asset is too large"));
    }
    let length = response.content_length();
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    let mut total = 0u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| error("Release download interrupted"))?
    {
        total += chunk.len() as u64;
        if total > limit {
            return Err(error("Release asset exceeds size limit"));
        }
        file.write_all(&chunk)?;
        progress(total, length);
    }
    file.sync_all()
}
pub fn remote(check: bool, tag: Option<&str>) -> io::Result<()> {
    if let Some(tag) = tag {
        parse_version(tag)?;
    }
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
        let client = client()?;
        let endpoint = if let Some(tag) = tag { format!("tags/v{}", tag.trim_start_matches('v')) } else { "latest".into() };
        let Some(release) = release(&client, &endpoint).await? else {
            println!("No published {}release found. Installed: {}. Actions artifacts are not releases.", if tag.is_some() { "matching " } else { "stable " }, env!("CARGO_PKG_VERSION"));
            return if check { Ok(()) } else { Err(error("Nothing to install from GitHub Releases")) };
        };
        let latest = parse_version(&release.tag_name)?;
        if release.draft || release.prerelease { return Err(error("Only published stable releases can be installed")); }
        if let Some(tag) = tag && latest != parse_version(tag)? { return Err(error("GitHub returned a different release")); }
        println!("Installed: {}. Release: {}.",env!("CARGO_PKG_VERSION"),release.tag_name);
        if !release.assets.iter().any(|a| a.name == ASSET) { return Err(error("Release has no ARMv7 archive")); }
        let (Some(archive_asset), Some(_)) = (release.asset(ASSET, MAX_ARCHIVE), release.asset("SHA256SUMS", 65536)) else { return Err(error("Release assets are missing or exceed size limits")); };
        let current = parse_version(env!("CARGO_PKG_VERSION"))?;
        if check { println!("{}",if latest > current { "An update is available." } else { "No newer release." }); return Ok(()); }
        if latest <= current { println!("No newer release; installation skipped."); return Ok(()); }
        if !cfg!(target_arch="arm") { return Err(error("Published binaries require ARMv7 Linux; run update on the speaker")); }
        let stage = Temporary::new(Path::new("/tmp"),"spotiya-update")?;
        if installation::free_bytes(&stage.0)? < MAX_EXPANDED + MAX_ARCHIVE + 2*1024*1024 { return Err(error("Not enough temporary space to download and unpack release")); }
        let download_base = format!("https://github.com/{REPO}/releases/download/{}",release.tag_name);
        let archive = stage.0.join(ASSET); let sums = stage.0.join("SHA256SUMS");
        println!("Downloading archive and SHA256SUMS...");
        download(&client,&format!("{download_base}/SHA256SUMS"),&sums,65536).await?;
        download(&client,&format!("{download_base}/{ASSET}"),&archive,MAX_ARCHIVE).await?;
        if fs::metadata(&archive)?.len() != archive_asset.size { return Err(error("Downloaded archive size does not match release")); }
        install_verified(&archive, &limited_file(&sums,65536)?, &stage.0, Some(&release.tag_name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn archive(path: &Path, name: &str, kind: tar::EntryType) {
        let file = fs::File::create(path).unwrap();
        let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut tar = tar::Builder::new(gzip);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_mode(0o644);
        header.set_size(3);
        header.set_cksum();
        tar.append_data(&mut header, name, &b"abc"[..]).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }
    #[test]
    fn checksums_reject_corruption_missing_and_duplicate_entries() {
        let dir = Temporary::new(&std::env::temp_dir(), "spotiya-checksum-test").unwrap();
        let path = dir.0.join(ASSET);
        fs::write(&path, b"data").unwrap();
        let line = format!("{:x}  {ASSET}\n", Sha256::digest(b"data"));
        verify(&path, &line).unwrap();
        assert!(verify(&path, &line.repeat(2)).is_err());
        assert!(verify(&path, &line.replace(ASSET, "other")).is_err());
        fs::write(&path, b"changed").unwrap();
        assert!(verify(&path, &line).is_err());
    }
    #[test]
    fn archive_extraction_rejects_links_unexpected_paths_and_duplicates() {
        let dir = Temporary::new(&std::env::temp_dir(), "spotiya-archive-test").unwrap();
        let path = dir.0.join("release.gz");
        for (name, kind) in [
            ("spotiya/spotiya", tar::EntryType::Symlink),
            ("outside/file", tar::EntryType::Regular),
            ("spotiya/secret", tar::EntryType::Regular),
        ] {
            archive(&path, name, kind);
            assert!(extract(&path, &dir.0).is_err());
        }
        archive(&path, "spotiya/spotiya", tar::EntryType::Regular);
        extract(&path, &dir.0).unwrap();
        assert_eq!(fs::read(dir.0.join("spotiya/spotiya")).unwrap(), b"abc");
        assert!(extract(&path, &dir.0).is_err());
    }
    #[test]
    fn archive_parent_traversal_is_rejected() {
        let dir = Temporary::new(&std::env::temp_dir(), "spotiya-traversal-test").unwrap();
        let path = dir.0.join("release.gz");
        let gzip = flate2::write::GzEncoder::new(
            fs::File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut tar = tar::Builder::new(gzip);
        let mut header = tar::Header::new_gnu();
        let name = b"spotiya/../../outside";
        header.as_mut_bytes()[..name.len()].copy_from_slice(name);
        header.set_size(3);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, &b"abc"[..]).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        assert!(extract(&path, &dir.0).is_err());
        assert!(!dir.0.join("outside").exists());
    }
    #[test]
    fn release_versions_are_numeric_and_cannot_inject_url_paths() {
        assert!(parse_version("v0.10.0").unwrap() > parse_version("0.9.99").unwrap());
        for value in [
            "v1.0",
            "../latest",
            "1.0.0-beta",
            "1.0.0?token=x",
            "1.0.-1",
            "",
        ] {
            assert!(parse_version(value).is_err());
        }
    }
}
