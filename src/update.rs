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

use std::collections::BTreeMap;
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

fn parse_version(v: &str) -> Option<(Vec<u64>, Option<String>)> {
    let v = v.trim().trim_start_matches('v');
    let (main, pre) = match v.split_once('-') {
        Some((m, p)) => (m, Some(p.to_string())),
        None => (v, None),
    };
    let nums: Option<Vec<u64>> = main.split('.').map(|p| p.parse().ok()).collect();
    nums.filter(|n| n.len() == 3).map(|n| (n, pre))
}

/// Is `candidate` a newer version than `current`? A pre-release sorts below its release.
pub fn newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some((a, ap)), Some((b, bp))) => match a.cmp(&b) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => match (ap, bp) {
                (None, Some(_)) => true,
                (Some(x), Some(y)) => x > y,
                _ => false,
            },
        },
        _ => false,
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    // sha256sum is on every Linux the agent runs on; no hashing crate needed for one file
    let out = Command::new("sha256sum")
        .arg(path)
        .output()
        .context("running sha256sum")?;
    ensure!(out.status.success(), "sha256sum failed");
    Ok(String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string())
}

fn curl(url: &str, dest: &Path) -> Result<()> {
    let st = Command::new("curl")
        .args(["-fsSL", "--max-time", "600", "-o"])
        .arg(dest)
        .arg(url)
        .status()
        .context("running curl (needed to download updates from the internet)")?;
    ensure!(st.success(), "download failed: {url}");
    Ok(())
}

pub struct Found {
    pub version: String,
    /// where the files are: a directory of downloaded files
    pub dir: PathBuf,
    pub source: String,
}

/// Latest release on GitHub, downloaded into `dir` (sums, signature, archive).
pub fn from_github(dir: &Path) -> Result<Option<Found>> {
    std::fs::create_dir_all(dir)?;
    let meta = dir.join("latest.json");
    curl(
        &format!("https://api.github.com/repos/{REPO}/releases/latest"),
        &meta,
    )?;
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&meta)?)?;
    let tag = v["tag_name"].as_str().context("release has no tag")?;
    let version = tag.trim_start_matches('v').to_string();
    if !newer(&version, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    for f in [SUMS, SIG, &asset_name(&version)] {
        curl(
            &format!("https://github.com/{REPO}/releases/download/{tag}/{f}"),
            &dir.join(f),
        )?;
    }
    Ok(Some(Found {
        version,
        dir: dir.to_path_buf(),
        source: "GitHub".into(),
    }))
}

/// Newest complete release on the mesh, exported into `dir`.
pub fn from_mesh(app: &App, dir: &Path) -> Result<Vec<Found>> {
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
    let mut candidates: Vec<(String, BTreeMap<String, String>)> = by_version
        .into_iter()
        .filter(|(v, files)| {
            newer(v, env!("CARGO_PKG_VERSION"))
                && [SUMS, SIG, &asset_name(v)]
                    .iter()
                    .all(|f| files.contains_key(*f))
        })
        .collect();
    // newest first, so a broken newest release falls back to the next one
    candidates.sort_by(|(a, _), (b, _)| {
        if newer(a, b) {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        }
    });
    let mut out = vec![];
    for (version, files) in candidates {
        let vdir = dir.join(format!("v{version}"));
        std::fs::create_dir_all(&vdir)?;
        let exported: Result<()> = [SUMS, SIG, &asset_name(&version)].iter().try_for_each(|f| {
            let m = crate::blobs::manifest(&app.log.store.conn(), &files[*f])?
                .context("release blob vanished")?;
            app.blobs.export(&m, &vdir.join(f))
        });
        match exported {
            Ok(()) => out.push(Found {
                version,
                dir: vdir,
                source: "the mesh".into(),
            }),
            Err(e) => tracing::warn!("release v{version} on the mesh: {e:#}"),
        }
    }
    Ok(out)
}

/// Check the signature and checksum of a found release. Returns the archive path.
pub fn verify(found: &Found) -> Result<PathBuf> {
    let sums = std::fs::read(found.dir.join(SUMS))?;
    let sig = std::fs::read_to_string(found.dir.join(SIG))?;
    verify_sums(&sums, &sig, &release_keys()?)?;
    let name = asset_name(&found.version);
    let want = parse_sums(&String::from_utf8_lossy(&sums))
        .remove(&name)
        .with_context(|| format!("{name} is not in the signed checksum list"))?;
    let archive = found.dir.join(&name);
    ensure!(
        sha256_file(&archive)? == want,
        "{name} does not match its signed checksum"
    );
    Ok(archive)
}

/// Swap the running binary for the one in the verified archive.
pub fn install(archive: &Path) -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let unpack = archive.with_extension("unpack");
    std::fs::create_dir_all(&unpack)?;
    let st = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(&unpack)
        .status()?;
    ensure!(st.success(), "could not unpack the release archive");
    let new = std::fs::read_dir(&unpack)?
        .filter_map(|e| e.ok())
        .map(|e| e.path().join("ridgewarden"))
        .find(|p| p.exists())
        .context("release archive has no ridgewarden binary")?;
    let staged = exe.with_extension("new");
    std::fs::copy(&new, &staged)
        .with_context(|| format!("writing next to {} (run with sudo?)", exe.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    // rename over the running binary: the old process keeps its open copy until restart
    std::fs::rename(&staged, &exe)?;
    Ok(exe)
}

/// Republish a verified release's files on the mesh as blobs.
pub fn publish_on_mesh(app: &App, found: &Found) -> Result<usize> {
    verify(found)?;
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
}
