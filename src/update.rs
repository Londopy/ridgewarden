//! Signed updates, from GitHub when a site has internet or from the mesh when it doesn't.
//!
//! Every release publishes `SHA256SUMS.txt` and `SHA256SUMS.txt.sig`, an Ed25519 signature
//! by a release key whose public half is built into the binary. An update is installed only
//! if the signature checks out and the downloaded archive matches its checksum, and only
//! after the operator says yes.
//!
//! On the mesh, a station with internet republishes the release files as blobs named
//! `ridgewarden-release/v<version>/<file>`; other sites fetch them like any blob. The blob's
//! signer doesn't matter: the release signature is checked the same way either path.
//!
//! A release is only ever looked at in a `WorkDir`, a folder no other user can change, so
//! what was checked is what gets installed.

use std::cmp::{Ordering, Reverse};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rusqlite::params;

use crate::app::App;
use crate::invite::{b64_decode, b64_encode};

pub const REPO: &str = "Londopy/ridgewarden";
pub const BLOB_PREFIX: &str = "ridgewarden-release/";
const SUMS: &str = "SHA256SUMS.txt";
const SIG: &str = "SHA256SUMS.txt.sig";
// What a release's files can weigh. A real checksum list is a couple of KB, a signature 86
// characters, GitHub's answer about the latest release tens of KB and an archive under
// 20 MB; they can come from the mesh too, so anything much bigger is refused unread.
const MAX_SUMS: u64 = 64 * 1024;
const MAX_SIG: u64 = 1024;
const MAX_LATEST: u64 = 1024 * 1024;
const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;

/// Release public keys, hex, comma-separated, set at build time by the release workflow.
pub fn release_keys() -> Result<Vec<VerifyingKey>> {
    let Some(list) = option_env!("RIDGEWARDEN_RELEASE_KEYS") else {
        return Ok(vec![]);
    };
    list.split(',')
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(|k| {
            let b: [u8; 32] = hex::decode(k)?
                .try_into()
                .map_err(|_| anyhow::anyhow!("release key is not 32 bytes"))?;
            Ok(VerifyingKey::from_bytes(&b)?)
        })
        .collect()
}

/// The archive name for this machine, as the release workflow names it.
pub fn asset_name(version: &str) -> String {
    format!(
        "ridgewarden-v{version}-{}-linux.tar.gz",
        std::env::consts::ARCH
    )
}

/// Check `sig` (base64url) over `sums` against any of `keys`.
pub fn verify_sums(sums: &[u8], sig: &str, keys: &[VerifyingKey]) -> Result<()> {
    ensure!(
        !keys.is_empty(),
        "this build has no release key, so it can't verify updates; install from the releases page instead"
    );
    let sig: [u8; 64] = b64_decode(sig.trim())?
        .try_into()
        .map_err(|_| anyhow::anyhow!("release signature is the wrong length"))?;
    let sig = Signature::from_bytes(&sig);
    ensure!(
        keys.iter().any(|k| k.verify_strict(sums, &sig).is_ok()),
        "release signature does not match any release key"
    );
    Ok(())
}

pub fn sign_sums(sums: &[u8], key: &SigningKey) -> String {
    b64_encode(&key.sign(sums).to_bytes())
}

/// `sha256  filename` lines.
pub fn parse_sums(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let (h, f) = l.split_once(char::is_whitespace)?;
            Some((
                f.trim().trim_start_matches('*').to_string(),
                h.trim().to_ascii_lowercase(),
            ))
        })
        .filter(|(_, h)| h.len() == 64)
        .collect()
}

/// A release version: three numbers and maybe a pre-release, which sorts below its release.
#[derive(Debug, PartialEq, Eq)]
struct Version {
    nums: Vec<u64>,
    pre: Option<String>,
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.nums
            .cmp(&other.nums)
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn parse_version(v: &str) -> Option<Version> {
    let v = v.trim().trim_start_matches('v');
    let (main, pre) = match v.split_once('-') {
        Some((m, p)) => (m, Some(p.to_string())),
        None => (v, None),
    };
    let nums: Option<Vec<u64>> = main.split('.').map(|p| p.parse().ok()).collect();
    nums.filter(|n| n.len() == 3)
        .map(|nums| Version { nums, pre })
}

/// Is `candidate` a newer version than `current`? A pre-release sorts below its release.
pub fn newer(candidate: &str, current: &str) -> bool {
    matches!(
        (parse_version(candidate), parse_version(current)),
        (Some(a), Some(b)) if a > b
    )
}

/// A version the way releases are named (`1.2.3`, `1.2.3-rc1`), so it is safe in a file
/// name and a URL.
fn plain_version(v: &str) -> bool {
    v.starts_with(|c: char| c.is_ascii_digit())
        && parse_version(v).is_some()
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

fn sha256_file(path: &Path) -> Result<String> {
    // sha256sum is on every Linux the agent runs on; no hashing crate needed for one file.
    // On stdin, because GNU sha256sum escapes its output line for a name with a backslash.
    let out = Command::new("sha256sum")
        .stdin(File::open(path).with_context(|| format!("reading {}", path.display()))?)
        .output()
        .context("running sha256sum")?;
    ensure!(out.status.success(), "sha256sum failed");
    Ok(String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string())
}

/// Download `url` to `dest`; curl gives up on anything over `max` bytes.
fn curl(url: &str, dest: &Path, max: u64) -> Result<()> {
    let st = Command::new("curl")
        .args(["-fsSL", "--max-time", "600", "--max-filesize"])
        .arg(max.to_string())
        .arg("-o")
        .arg(dest)
        .arg(url)
        .status()
        .context("running curl (needed to download updates from the internet)")?;
    ensure!(st.success(), "download failed ({st}): {url}");
    Ok(())
}

/// A file that ought to be small, refused if it isn't.
fn read_small(path: &Path, max: u64) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    File::open(path)
        .with_context(|| format!("reading {}", path.display()))?
        .take(max + 1)
        .read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= max,
        "{} is over {max} bytes, too big to be part of a release",
        path.display()
    );
    Ok(data)
}

/// A fresh folder only the user running `update` can open, for one update's files. It is
/// never in the data dir, which the agent's user owns, nor in $TMPDIR, which `su` and
/// `sudo VAR=value` pass on from whoever asked: either way that user could swap files after
/// root checked them and before root installed them. Deleted when dropped.
#[derive(Debug)]
pub struct WorkDir(PathBuf);

impl WorkDir {
    /// A work folder in /var/tmp, or in the system's temp folder off unix.
    pub fn new() -> Result<Self> {
        #[cfg(unix)]
        let parent = PathBuf::from("/var/tmp");
        #[cfg(not(unix))]
        let parent = std::env::temp_dir();
        Self::under(&parent)
    }

    /// A work folder in `parent`, which on unix has to keep other users off what root makes
    /// in it: root's, and sticky or writable by root alone.
    pub fn under(parent: &Path) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let m =
                fs::metadata(parent).with_context(|| format!("checking {}", parent.display()))?;
            ensure!(
                m.is_dir() && safe_parent(m.uid(), m.mode()),
                "{} can't hold an update's files: it has to be root's, and sticky or writable by root alone",
                parent.display()
            );
        }
        let n = getrandom::u64().map_err(|e| anyhow::anyhow!("no OS randomness: {e}"))?;
        let path = parent.join(format!("ridgewarden-update-{n:016x}"));
        #[cfg(unix)]
        let made = {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new().mode(0o700).create(&path)
        };
        #[cfg(not(unix))]
        let made = fs::create_dir(&path);
        made.with_context(|| format!("creating {}", path.display()))?;
        Ok(Self(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// A new folder in this one, for one release's files.
    fn sub(&self, name: &str) -> Result<PathBuf> {
        let dir = self.0.join(name);
        fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(dir)
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        // a folder left behind is root's and closed to others, and there's no one to tell
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Whether a folder with this owner and mode keeps what root makes in it from other users:
/// root's, and either sticky (others can't rename or delete root's entries) or not writable
/// by group or others.
#[cfg(any(unix, test))]
fn safe_parent(uid: u32, mode: u32) -> bool {
    uid == 0 && (mode & 0o1000 != 0 || mode & 0o022 == 0)
}

/// A release found on GitHub or the mesh, its files in a `WorkDir` it can't outlive.
#[derive(Debug)]
pub struct Found<'w> {
    pub version: String,
    pub source: String,
    dir: PathBuf,
    work: PhantomData<&'w WorkDir>,
}

impl Found<'_> {
    fn archive(&self) -> PathBuf {
        self.dir.join(asset_name(&self.version))
    }
}

/// Latest release on GitHub, downloaded into `work`.
pub fn from_github(work: &WorkDir) -> Result<Option<Found<'_>>> {
    latest_from(
        &format!("https://api.github.com/repos/{REPO}/releases/latest"),
        &format!("https://github.com/{REPO}/releases/download"),
        work,
    )
}

/// The release `latest` names (JSON with a `tag_name`, as GitHub's API answers), if it is
/// newer than this build, downloaded from `{download}/<tag>/` into a new folder in `work`.
pub fn latest_from<'w>(
    latest: &str,
    download: &str,
    work: &'w WorkDir,
) -> Result<Option<Found<'w>>> {
    let dir = work.sub("web")?;
    let meta = dir.join("latest.json");
    curl(latest, &meta, MAX_LATEST)?;
    let v: serde_json::Value = serde_json::from_slice(&read_small(&meta, MAX_LATEST)?)?;
    let tag = v["tag_name"].as_str().context("release has no tag")?;
    let version = tag
        .strip_prefix('v')
        .filter(|v| plain_version(v))
        .with_context(|| format!("release tag {tag:?} is not a version"))?
        .to_string();
    if !newer(&version, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let archive = asset_name(&version);
    for (f, max) in [
        (SUMS, MAX_SUMS),
        (SIG, MAX_SIG),
        (archive.as_str(), MAX_ARCHIVE),
    ] {
        curl(&format!("{download}/{tag}/{f}"), &dir.join(f), max)?;
    }
    Ok(Some(Found {
        version,
        source: "GitHub".into(),
        dir,
        work: PhantomData,
    }))
}

/// Complete releases on the mesh newer than this build, each exported into a new folder in
/// `work`. One whose files are bigger than a real release's is passed over unexported.
pub fn from_mesh<'w>(app: &App, work: &'w WorkDir) -> Result<Vec<Found<'w>>> {
    let names: Vec<(String, String)> = app
        .log
        .store
        .conn()
        .prepare("SELECT name, root FROM blobs WHERE complete = 1 AND name LIKE 'ridgewarden-release/v%'")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let mut by_version: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (name, root) in names {
        let rest = name.trim_start_matches(BLOB_PREFIX).trim_start_matches('v');
        if let Some((ver, file)) = rest.split_once('/') {
            by_version
                .entry(ver.to_string())
                .or_default()
                .insert(file.to_string(), root);
        }
    }
    let mut out = vec![];
    for (version, files) in by_version {
        if !plain_version(&version) || !newer(&version, env!("CARGO_PKG_VERSION")) {
            continue;
        }
        let archive = asset_name(&version);
        let mut parts = vec![];
        for (f, max) in [
            (SUMS, MAX_SUMS),
            (SIG, MAX_SIG),
            (archive.as_str(), MAX_ARCHIVE),
        ] {
            let Some(root) = files.get(f) else { break };
            let Some(m) = crate::blobs::manifest(&app.log.store.conn(), root)? else {
                break;
            };
            if m.size > max {
                tracing::warn!(
                    "release v{version} on the mesh: {f} is {} bytes, more than a real one",
                    m.size
                );
                break;
            }
            parts.push((f, m));
        }
        if parts.len() < 3 {
            continue;
        }
        let dir = work.sub(&format!("v{version}"))?;
        let exported: Result<()> = parts
            .iter()
            .try_for_each(|(f, m)| app.blobs.export(m, &dir.join(f)));
        match exported {
            Ok(()) => out.push(Found {
                version,
                source: "the mesh".into(),
                dir,
                work: PhantomData,
            }),
            Err(e) => tracing::warn!("release v{version} on the mesh: {e:#}"),
        }
    }
    Ok(out)
}

/// The newest of `found`, wherever it came from, that checks out against `keys`, and why
/// each newer one didn't. Older ones aren't looked at.
pub fn newest_verified<'w>(
    mut found: Vec<Found<'w>>,
    keys: &[VerifyingKey],
) -> (Option<Found<'w>>, Vec<String>) {
    found.sort_by_key(|f| Reverse(parse_version(&f.version)));
    let mut passed_over = vec![];
    for f in found {
        match verify(&f, keys) {
            Ok(()) => return (Some(f), passed_over),
            Err(e) => passed_over.push(format!("v{} from {}: {e:#}", f.version, f.source)),
        }
    }
    (None, passed_over)
}

/// 0.1.0 kept its update files in `<data_dir>/update`, where the agent's own user could
/// swap them; nothing uses that folder now.
pub fn clear_old_folder(data_dir: &Path) -> Result<()> {
    let old = data_dir.join("update");
    match fs::remove_dir_all(&old) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}, which nothing uses now", old.display()))
        }
        _ => Ok(()),
    }
}

/// Check the signature and checksum of a found release.
pub fn verify(found: &Found, keys: &[VerifyingKey]) -> Result<()> {
    let sums = read_small(&found.dir.join(SUMS), MAX_SUMS)?;
    let sig = String::from_utf8(read_small(&found.dir.join(SIG), MAX_SIG)?)
        .context("the release signature is not text")?;
    verify_sums(&sums, &sig, keys)?;
    let name = asset_name(&found.version);
    let want = parse_sums(&String::from_utf8_lossy(&sums))
        .remove(&name)
        .with_context(|| format!("{name} is not in the signed checksum list"))?;
    ensure!(
        sha256_file(&found.archive())? == want,
        "{name} does not match its signed checksum"
    );
    Ok(())
}

/// Check `found` against `keys`, unpack it in its work folder and put its binary in place of
/// `exe` (the running one).
pub fn install_verified(found: &Found, keys: &[VerifyingKey], exe: &Path) -> Result<()> {
    verify(found, keys)?;
    let unpack = found.dir.join("unpack");
    fs::create_dir(&unpack).with_context(|| format!("creating {}", unpack.display()))?;
    // root's tar would otherwise give the files the owner and mode they had on the
    // machine that built the release
    let st = Command::new("tar")
        .args(["--no-same-owner", "--no-same-permissions", "-xzf"])
        .arg(found.archive())
        .arg("-C")
        .arg(&unpack)
        .status()
        .context("running tar")?;
    ensure!(st.success(), "could not unpack the release archive");
    let top = unpack.join(asset_name(&found.version).trim_end_matches(".tar.gz"));
    let new = top.join("ridgewarden");
    // through a link, the copy below would install whatever the link points at
    let plain = fs::symlink_metadata(&top).is_ok_and(|m| m.is_dir())
        && fs::symlink_metadata(&new).is_ok_and(|m| m.is_file());
    ensure!(
        plain,
        "the release archive has no ridgewarden binary as a plain file"
    );
    let staged = exe.with_extension("new");
    // left by an install that stopped part way, or put there by someone else; either way
    // nothing is written through it
    match fs::remove_file(&staged) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {}", staged.display()));
        }
        _ => {}
    }
    let installed = stage(&new, &staged).and_then(|()| {
        // rename over the running binary: the old process keeps its open copy until restart
        fs::rename(&staged, exe).with_context(|| format!("replacing {}", exe.display()))
    });
    if let Err(e) = installed {
        // the install failed whatever happens here, and that is the error worth reporting
        let _ = fs::remove_file(&staged);
        return Err(e);
    }
    // the binary is in place either way; this only makes the rename last through a power cut
    #[cfg(unix)]
    if let Some(dir) = exe.parent()
        && let Err(e) = File::open(dir).and_then(|d| d.sync_all())
    {
        tracing::warn!(
            "installed, but could not flush {} to disk: {e}",
            dir.display()
        );
    }
    Ok(())
}

/// Copy `new` to `staged` as a file that didn't exist before, runnable, and on disk.
fn stage(new: &Path, staged: &Path) -> Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o755);
    let mut out = opts
        .open(staged)
        .with_context(|| format!("creating {} (run with sudo?)", staged.display()))?;
    io::copy(&mut File::open(new)?, &mut out)?;
    // the umask may have taken some of those bits off
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        out.set_permissions(fs::Permissions::from_mode(0o755))?;
    }
    out.sync_all()?;
    Ok(())
}

/// Republish a verified release's files on the mesh as blobs.
pub fn publish_on_mesh(app: &App, found: &Found) -> Result<usize> {
    verify(found, &release_keys()?)?;
    let mut n = 0;
    for f in [SUMS, SIG, &asset_name(&found.version)] {
        let name = format!("{BLOB_PREFIX}v{}/{f}", found.version);
        let exists: bool = app.log.store.conn().query_row(
            "SELECT count(*) > 0 FROM blobs WHERE name = ?1 AND complete = 1",
            params![name],
            |r| r.get(0),
        )?;
        if !exists {
            crate::blobs::add_file(
                &app.log,
                &app.blobs,
                &found.dir.join(f),
                &name,
                crate::blobs::DEFAULT_PRIORITY,
            )?;
            n += 1;
        }
    }
    Ok(n)
}

/// Make a release signing key: the private half goes to `out` (keep it secret, e.g. as a
/// CI secret), the public half is returned for the build.
pub fn new_release_key(out: &Path) -> Result<String> {
    if out.exists() {
        bail!("{} already exists; not overwriting a key", out.display());
    }
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("no OS randomness: {e}"))?;
    let key = SigningKey::from_bytes(&seed);
    crate::identity::write_private(out, &hex::encode(seed))?;
    Ok(hex::encode(key.verifying_key().to_bytes()))
}

pub fn load_release_key(hex_seed: &str) -> Result<SigningKey> {
    let seed: [u8; 32] = hex::decode(hex_seed.trim())?
        .try_into()
        .map_err(|_| anyhow::anyhow!("release key is not 32 bytes"))?;
    Ok(SigningKey::from_bytes(&seed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare() {
        assert!(newer("0.2.0", "0.1.9"));
        assert!(newer("1.0.0", "1.0.0-rc2"));
        assert!(newer("1.0.0-rc2", "1.0.0-rc1"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0-rc1", "0.1.0"));
        assert!(!newer("garbage", "0.1.0"));
    }

    #[test]
    fn signed_sums_verify_and_tampering_fails() {
        let key = SigningKey::from_bytes(&[5; 32]);
        let sums = b"aa  ridgewarden-v0.2.0-x86_64-linux.tar.gz\n";
        let sig = sign_sums(sums, &key);
        verify_sums(sums, &sig, &[key.verifying_key()]).unwrap();
        assert!(
            verify_sums(
                b"bb  ridgewarden-v0.2.0-x86_64-linux.tar.gz\n",
                &sig,
                &[key.verifying_key()]
            )
            .is_err()
        );
        let other = SigningKey::from_bytes(&[6; 32]);
        assert!(verify_sums(sums, &sig, &[other.verifying_key()]).is_err());
        assert!(
            verify_sums(sums, &sig, &[]).is_err(),
            "no release key means no updates"
        );
    }

    #[test]
    fn sums_parse() {
        let h = "ab".repeat(32);
        let m = parse_sums(&format!("{h}  a.tar.gz\n{h} *b.deb\nnot a line\n"));
        assert_eq!(m.get("a.tar.gz"), Some(&h));
        assert_eq!(m.get("b.deb"), Some(&h));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn small_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SHA256SUMS.txt");
        fs::write(&path, vec![b'a'; 100]).unwrap();
        assert_eq!(read_small(&path, 100).unwrap().len(), 100);
        let e = read_small(&path, 99).unwrap_err();
        assert!(e.to_string().contains("too big"), "{e:#}");
    }

    #[test]
    fn only_a_root_folder_others_cant_change_holds_a_work_folder() {
        assert!(safe_parent(0, 0o41777), "/tmp and /var/tmp: sticky");
        assert!(safe_parent(0, 0o40755), "writable by root alone");
        assert!(
            !safe_parent(0, 0o40777),
            "anyone may rename root's folder away"
        );
        assert!(!safe_parent(0, 0o40775), "the group may");
        assert!(!safe_parent(1000, 0o40700), "someone else's");
    }
}
