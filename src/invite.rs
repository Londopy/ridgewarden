//! One-line invites: `rw1.<payload>.<signature>`, both base64url.
//!
//! The payload names the inviting station, where to reach it, and the stations it trusts.
//! Joining with it means "I trust whoever this station trusts", so a new site needs one
//! line instead of a list of 64-digit keys.

use anyhow::{Context, Result, bail, ensure};
use ed25519_dalek::{Signature, Signer, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::event::{canonical_string, valid_call};
use crate::identity::Identity;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Station {
    pub key: String,
    pub call: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Invite {
    pub v: u32,
    pub by: Station,
    /// where the inviting station's core answers, e.g. http://10.1.2.3:8790
    pub peers: Vec<String>,
    pub stations: Vec<Station>,
    pub at: i64,
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

pub fn b64_decode(s: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut n = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        let v = B64
            .iter()
            .position(|b| *b == c)
            .context("invite has a character that isn't base64url")?;
        n = (n << 6 | v as u32) & 0xffff;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((n >> bits) as u8);
        }
    }
    // only one spelling per byte string: leftover bits must be zero
    ensure!(
        n & ((1 << bits) - 1) == 0,
        "invite is not encoded canonically"
    );
    Ok(out)
}

pub fn valid_key(k: &str) -> bool {
    k.len() == 64
        && k.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn make(id: &Identity, peers: Vec<String>, stations: Vec<Station>, at: i64) -> Result<String> {
    let inv = Invite {
        v: 1,
        by: Station {
            key: id.author.key.clone(),
            call: id.author.call.clone(),
        },
        peers,
        stations,
        at,
    };
    let body = canonical_string(&serde_json::to_value(&inv)?);
    let sig = id.key.sign(body.as_bytes());
    Ok(format!(
        "rw1.{}.{}",
        b64_encode(body.as_bytes()),
        b64_encode(&sig.to_bytes())
    ))
}

/// Parse an invite and check its signature and contents.
pub fn read(line: &str) -> Result<Invite> {
    let mut parts = line.trim().split('.');
    let (Some("rw1"), Some(body), Some(sig), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        bail!("not a ridgewarden invite (expected rw1.<payload>.<signature>)");
    };
    let body = b64_decode(body)?;
    let inv: Invite = serde_json::from_slice(&body).context("invite payload is not valid")?;
    ensure!(inv.v == 1, "invite version {} is not supported", inv.v);
    ensure!(valid_key(&inv.by.key), "invite signer key is not valid");
    ensure!(
        canonical_string(&serde_json::from_slice(&body)?).as_bytes() == body.as_slice(),
        "invite payload is not in canonical form"
    );
    let key: [u8; 32] = hex::decode(&inv.by.key)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("bad key"))?;
    let sig: [u8; 64] = b64_decode(sig)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invite signature is the wrong length"))?;
    VerifyingKey::from_bytes(&key)?
        .verify_strict(&body, &Signature::from_bytes(&sig))
        .context("invite signature does not match: it was altered or mistyped")?;
    for s in inv.stations.iter().chain(std::iter::once(&inv.by)) {
        ensure!(valid_key(&s.key), "invite lists a key that isn't valid");
        ensure!(
            valid_call(&s.call),
            "invite lists a callsign that isn't valid: {:?}",
            s.call
        );
    }
    for p in &inv.peers {
        ensure!(
            p.starts_with("http://") && p.len() < 200,
            "invite peer {p:?} is not an http URL"
        );
    }
    Ok(inv)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        for n in 0..20 {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(b64_decode(&b64_encode(&data)).unwrap(), data);
        }
        assert_eq!(b64_encode(b"hello"), "aGVsbG8");
        assert!(
            b64_decode("aGVsbG9").is_err(),
            "non-zero padding bits are refused"
        );
    }

    #[test]
    fn invite_round_trips_and_detects_tampering() {
        let id = Identity::from_seed("N0AAA", [4; 32]);
        let other = Station {
            key: "ab".repeat(32),
            call: "N0BBB".into(),
        };
        let line = make(
            &id,
            vec!["http://10.1.2.3:8790".into()],
            vec![other.clone()],
            1,
        )
        .unwrap();
        let inv = read(&line).unwrap();
        assert_eq!(inv.by.call, "N0AAA");
        assert_eq!(inv.stations, vec![other]);
        // change one character of the payload
        let mut bad: Vec<char> = line.chars().collect();
        let i = 10;
        bad[i] = if bad[i] == 'A' { 'B' } else { 'A' };
        assert!(read(&bad.into_iter().collect::<String>()).is_err());
        assert!(read("hello").is_err());
    }
}
