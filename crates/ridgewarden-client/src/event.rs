//! Signed events: the unit everything in ridgewarden is built from.
//!
//! An event's `id` is the BLAKE3 hash of its canonical body (every field except `id` and
//! `sig`), and `sig` is an Ed25519 signature over those 32 hash bytes. Payloads are plain
//! JSON on purpose: Part 97 forbids encoding that obscures meaning, so events are signed
//! for integrity and authorship, never encrypted.

use anyhow::{Result, bail};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Who wrote an event: the station's callsign and its Ed25519 public key (hex).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Author {
    pub call: String,
    pub key: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Event {
    pub id: String,
    pub author: Author,
    pub seq: u64,
    pub prev: Option<String>,
    pub lamport: u64,
    pub wall_time: String,
    pub mono_time: u64,
    pub kind: String,
    pub payload: Value,
    pub sig: String,
    /// which of the author's chains this event is in; absent means the main chain
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
}

/// Everything except the event's fields that are derived from it.
pub struct Draft {
    pub author: Author,
    pub seq: u64,
    pub prev: Option<String>,
    pub lamport: u64,
    pub wall_time: String,
    pub mono_time: u64,
    pub kind: String,
    pub payload: Value,
    pub log: Option<String>,
}

/// The chain an event belongs to: the author's key for the main chain, `key:log` for others
/// (telemetry). Sequence numbers, gaps and forks are all per chain.
pub fn chain_id(key: &str, log: Option<&str>) -> String {
    match log {
        Some(l) => format!("{key}:{l}"),
        None => key.to_string(),
    }
}

/// The sub-logs that exist. Kept to a fixed list so one key can't create thousands of
/// chains and crowd everyone else out of the heads list.
pub fn valid_log(l: &str) -> bool {
    l == "telemetry"
}

// hand-rolled canonical JSON: keys sorted at every level, no whitespace. serde_json's map
// order depends on a cargo feature any dependency could switch on, and a hash that
// changed with the build would split the log, so we don't rely on it.
pub fn canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).expect("string serializes"));
                out.push(':');
                canonical(&m[*k], out);
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(x, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// Borrowed view of the signed fields, so signing and verifying hash exactly the same thing.
struct Body<'a> {
    author: &'a Author,
    seq: u64,
    prev: &'a Option<String>,
    lamport: u64,
    wall_time: &'a str,
    mono_time: u64,
    kind: &'a str,
    payload: &'a Value,
    log: &'a Option<String>,
}

impl Body<'_> {
    fn bytes(&self) -> Vec<u8> {
        let mut body = serde_json::json!({
            "author": {"call": self.author.call, "key": self.author.key},
            "seq": self.seq,
            "prev": self.prev,
            "lamport": self.lamport,
            "wall_time": self.wall_time,
            "mono_time": self.mono_time,
            "kind": self.kind,
            "payload": self.payload,
        });
        // only present when set, so main-chain events hash the same as before sub-logs existed
        if let Some(l) = self.log {
            body["log"] = Value::String(l.clone());
        }
        let mut s = String::new();
        canonical(&body, &mut s);
        s.into_bytes()
    }
}

/// A callsign, optionally with a /suffix or -SSID: letters, digits, '/' and '-', 1-20 long.
pub fn valid_call(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= 20
        && c.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'/' || b == b'-')
}

/// Canonical JSON text of any value (keys sorted, no whitespace): what list hashes use.
pub fn canonical_string(v: &Value) -> String {
    let mut s = String::new();
    canonical(v, &mut s);
    s
}

/// Short content hash of a list carried in an event (128 bits, hex).
pub fn list_hash(v: &Value) -> String {
    blake3::hash(canonical_string(v).as_bytes()).to_hex()[..32].to_string()
}

impl Draft {
    pub fn sign(self, key: &SigningKey) -> Event {
        let bytes = Body {
            author: &self.author,
            seq: self.seq,
            prev: &self.prev,
            lamport: self.lamport,
            wall_time: &self.wall_time,
            mono_time: self.mono_time,
            kind: &self.kind,
            payload: &self.payload,
            log: &self.log,
        }
        .bytes();
        let hash = blake3::hash(&bytes);
        let sig = key.sign(hash.as_bytes());
        Event {
            id: hash.to_hex().to_string(),
            author: self.author,
            seq: self.seq,
            prev: self.prev,
            lamport: self.lamport,
            wall_time: self.wall_time,
            mono_time: self.mono_time,
            kind: self.kind,
            payload: self.payload,
            sig: hex::encode(sig.to_bytes()),
            log: self.log,
        }
    }
}

impl Event {
    pub fn chain(&self) -> String {
        chain_id(&self.author.key, self.log.as_deref())
    }

    /// Recompute the id from the body and check the signature against the author's key.
    pub fn verify(&self) -> Result<()> {
        let bytes = Body {
            author: &self.author,
            seq: self.seq,
            prev: &self.prev,
            lamport: self.lamport,
            wall_time: &self.wall_time,
            mono_time: self.mono_time,
            kind: &self.kind,
            payload: &self.payload,
            log: &self.log,
        }
        .bytes();
        let hash = blake3::hash(&bytes);
        if hash.to_hex().as_str() != self.id {
            bail!("event id does not match its body");
        }
        // callsigns end up in pages and log lines; keep them to what a callsign can be
        if self.log.as_deref().is_some_and(|l| !valid_log(l)) {
            bail!("log name is not valid");
        }
        if !valid_call(&self.author.call) {
            bail!("author callsign is not a callsign");
        }
        let key_bytes: [u8; 32] = hex::decode(&self.author.key)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("author key is not 32 bytes"))?;
        // one spelling per key: hex::decode takes capitals too, and the key string is what
        // the revocation and fork tables are keyed on
        if hex::encode(key_bytes) != self.author.key {
            bail!("author key is not canonical lowercase hex");
        }
        let vk = VerifyingKey::from_bytes(&key_bytes)?;
        let sig_bytes: [u8; 64] = hex::decode(&self.sig)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("signature is not 64 bytes"))?;
        // verify_strict also refuses low-order keys, which "verify" any signature
        vk.verify_strict(hash.as_bytes(), &Signature::from_bytes(&sig_bytes))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn draft(payload: Value) -> Draft {
        let k = key();
        Draft {
            author: Author {
                call: "N0CALL".into(),
                key: hex::encode(k.verifying_key().to_bytes()),
            },
            seq: 0,
            prev: None,
            lamport: 1,
            wall_time: "2026-11-18T00:00:00Z".into(),
            mono_time: 0,
            kind: "test".into(),
            payload,
            log: None,
        }
    }

    #[test]
    fn signed_event_verifies() {
        let e = draft(json!({"b": 1, "a": [1, {"z": 2, "y": 3}]})).sign(&key());
        e.verify().unwrap();
    }

    #[test]
    fn key_order_does_not_change_id() {
        let a = draft(serde_json::from_str(r#"{"a":1,"b":{"c":2,"d":3}}"#).unwrap()).sign(&key());
        let b = draft(serde_json::from_str(r#"{"b":{"d":3,"c":2},"a":1}"#).unwrap()).sign(&key());
        assert_eq!(a.id, b.id);
    }

    #[test]
    fn tampered_payload_is_rejected() {
        let mut e = draft(json!({"snr": 20})).sign(&key());
        e.payload = json!({"snr": 30});
        assert!(e.verify().is_err());
    }

    #[test]
    fn sub_log_is_signed_and_separates_chains() {
        let mut d = draft(json!({}));
        d.log = Some("telemetry".into());
        let mut e = d.sign(&key());
        e.verify().unwrap();
        assert!(e.chain().ends_with(":telemetry"));
        // moving it to the main chain changes what was signed
        e.log = None;
        assert!(e.verify().is_err());
        let main = draft(json!({})).sign(&key());
        assert_eq!(main.chain(), main.author.key);
    }

    #[test]
    fn tampered_author_is_rejected() {
        let mut e = draft(json!({})).sign(&key());
        // any other call sign than the one signed
        e.author.call = format!("{}X", e.author.call);
        assert!(e.verify().is_err());
    }

    #[test]
    fn json_round_trip_keeps_signature_valid() {
        let e = draft(json!({"x": 1.5, "s": "héllo \"q\""})).sign(&key());
        let back: Event = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        back.verify().unwrap();
    }
}
