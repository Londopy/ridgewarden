//! The agent's keypair, bound to the licensee's callsign.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use ed25519_dalek::SigningKey;

use crate::event::Author;

pub struct Identity {
    pub key: SigningKey,
    pub author: Author,
}

impl Identity {
    pub fn generate(call: &str) -> Result<Self> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("no OS randomness: {e}"))?;
        Ok(Self::from_seed(call, seed))
    }

    pub fn from_seed(call: &str, seed: [u8; 32]) -> Self {
        let key = SigningKey::from_bytes(&seed);
        let author = Author {
            call: call.trim().to_ascii_uppercase(),
            key: hex::encode(key.verifying_key().to_bytes()),
        };
        Self { key, author }
    }

    /// Load the key at `path`, or create one there if it doesn't exist yet.
    pub fn load_or_create(path: &Path, call: &str) -> Result<Self> {
        if path.exists() {
            // apps share the agent's group, so a key restored with looser permissions
            // (cp, tar, rsync) must go back to owner-only before anything else runs
            if let Err(e) = set_mode(path, 0o600) {
                tracing::warn!("could not make {} owner-only: {e:#}", path.display());
            }
            let text = fs::read_to_string(path)
                .with_context(|| format!("reading key {}", path.display()))?;
            let bytes = hex::decode(text.trim()).context("key file is not hex")?;
            let seed: [u8; 32] = bytes
                .try_into()
                .map_err(|_| anyhow::anyhow!("key file is not 32 bytes"))?;
            return Ok(Self::from_seed(call, seed));
        }
        if call.trim().is_empty() {
            bail!("a callsign is needed to create a key");
        }
        let id = Self::generate(call)?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        // write then rename, so a power cut never leaves a half-written key
        let tmp = path.with_extension("tmp");
        write_private(&tmp, &hex::encode(id.key.to_bytes()))?;
        fs::rename(&tmp, path)?;
        Ok(id)
    }
}

/// Write a file only its owner can read, with no window where it is world-readable.
pub fn write_private(p: &Path, text: &str) -> Result<()> {
    write_mode(p, text, 0o600)
}

/// Write a file its owner and group can read: the app token, which apps reach by being
/// in the agent's group, while the station key stays owner-only.
pub fn write_group(p: &Path, text: &str) -> Result<()> {
    write_mode(p, text, 0o640)
}

#[cfg(unix)]
fn write_mode(p: &Path, text: &str, mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(p)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_mode(p: &Path, text: &str, _mode: u32) -> Result<()> {
    fs::write(p, text)?;
    Ok(())
}

/// Set a path's permission bits; a no-op where there are none.
#[cfg(unix)]
pub fn set_mode(p: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
pub fn set_mode(_p: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("identity.key");
        let a = Identity::load_or_create(&p, "n0call").unwrap();
        let b = Identity::load_or_create(&p, "N0CALL").unwrap();
        assert_eq!(a.author, b.author);
        assert_eq!(a.author.call, "N0CALL");
    }
}
