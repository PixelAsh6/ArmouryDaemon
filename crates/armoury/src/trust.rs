//! The trust anchor of the catalogue: a minisign keyring baked into the binary.
//!
//! The manifest travels signed; the whole manifest authenticates transitively, because
//! each entry already carries the SHA-256 and size of the pack it names. The keyring
//! lives here, is compiled in with `include_str!`, and read-only — it names whose keys
//! may sign a manifest this daemon accepts. Two keys may live at a time (a rotated-in
//! and a rotated-out during the 90-day overlap); a manifest must verify against at
//! least one of them.
//!
//! The private key that matches the first key is the keeper's, and lives only with
//! them. Nothing in this repository signs anything.
//!
//! Tests trade the ring for their own via `ARMOURY_TEST_TRUST`, the same way
//! `ARMOURY_TEST_NS` trades the config and the pipe: a test run sets the variable to a
//! file whose format is the same `keys.json`, so the harness can serve frozen probes
//! signed under a key whose private half died the day the fixtures were made.

use std::sync::OnceLock;

use minisign_verify::{PublicKey, Signature};
use serde::Deserialize;

/// The baked keyring. One file, no fallback: a malformed ring is a build defect, and
/// the same struct parses the override file the test rig writes.
const KEYS_JSON: &str = include_str!("trust/keys.json");

#[derive(Debug, Deserialize)]
struct KeyringJson {
    schema: u32,
    keys: Vec<KeyEntry>,
}

#[derive(Debug, Deserialize)]
struct KeyEntry {
    #[serde(default)]
    comment: String,
    key: String,
}

/// The loaded ring: what may sign a manifest, with the comments that say why each key
/// is trusted.
pub struct TrustKeyring {
    entries: Vec<(String,PublicKey)>,
    /// Where the ring came from, for messages: the baked file, or the test override.
    source: String,
}

impl std::fmt::Debug for TrustKeyring {
    /// Debug without the keys themselves: the ring's shape, not its content.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrustKeyring")
            .field("keys", &self.entries.len())
            .field("source", &self.source)
            .finish()
    }
}

impl TrustKeyring {
    /// The ring this build trusts. Read once; the test override sticks for the process.
    pub fn load() -> &'static TrustKeyring {
        static KEYS: OnceLock<TrustKeyring> = OnceLock::new();
        KEYS.get_or_init(|| {
            match std::env::var("ARMOURY_TEST_TRUST") {
                Ok(path) => {
                    let text = std::fs::read_to_string(&path)
                        .unwrap_or_else(|e| panic!("the test keyring at {path} cannot be read: {e}"));
                    TrustKeyring::from_text(&text, &path)
                        .unwrap_or_else(|e| panic!("the test keyring at {path} is broken: {e}"))
                }
                Err(_) => TrustKeyring::from_text(KEYS_JSON, "the baked keyring")
                    .expect("the baked keyring must always load"),
            }
        })
    }

    fn from_text(text: &str, source: &str) -> Result<TrustKeyring, String> {
        let ring: KeyringJson = serde_json::from_str(text)
            .map_err(|e| format!("the keyring is not valid JSON ({e})"))?;
        if ring.schema != 1 {
            return Err(format!("the keyring's schema is {} and this build understands 1", ring.schema));
        }
        if ring.keys.is_empty() {
            return Err("the keyring carries no keys".to_string());
        }
        let mut entries = Vec::with_capacity(ring.keys.len());
        for entry in &ring.keys {
            let key = PublicKey::from_base64(entry.key.trim())
                .map_err(|e| format!("key {:?} does not parse ({e})", entry.comment))?;
            entries.push((entry.comment.clone(), key));
        }
        Ok(TrustKeyring { entries, source: source.to_string() })
    }

    /// How many keys this ring trusts.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Verify a signature file's text over payload bytes against every key of the ring.
    /// The signature text is a minisign file's whole content: untrusted comment, base64
    /// signature, trusted comment, trusted-comment signature — `Signature::decode` takes
    /// the file as it is, so no line splitting happens on our side.
    pub fn verify(&self, payload: &[u8], sig_text: &str) -> Result<(), String> {
        // the same file may arrive with Windows line endings from the fetch
        let text = sig_text.replace("\r\n", "\n");
        let signature = Signature::decode(&text)
            .map_err(|e| format!("the signature does not parse ({e})"))?;
        let mut reasons = Vec::with_capacity(self.entries.len());
        for (comment, key) in &self.entries {
            match key.verify(payload, &signature, false) {
                Ok(()) => return Ok(()),
                Err(e) => reasons.push(format!("{} ({e})", quote(comment))),
            }
        }
        Err(format!(
            "no key of the ring accepts it: checked {} key(s) from {} ({})",
            self.entries.len(),
            self.source,
            reasons.join("; ")
        ))
    }

    /// The source of the ring, for the messages that name what was checked.
    pub fn source(&self) -> &str {
        &self.source
    }
}

/// One line of reason text: keep it readable, both sides trimmed.
fn quote(comment: &str) -> String {
    let comment = comment.trim();
    if comment.is_empty() { "an unlabelled key".to_string() } else { comment.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixtures' public key: the private half of this pair never existed in any
    /// repository - the frozen probes were signed once, with a key whose bytes died with
    /// the temp folder they were generated in. The ring for a test is built in memory.
    const TEST_KEY: &str = "RWTUa5h/DwHow3IGIQ9hySXFqYn7fQTiXHT6t0m5gwKCTcsY1eCakbOz";
    /// The frozen signature of `tools/fixtures/man-schema2.json`, made with that key —
    /// the whole file, comment lines included, exactly what `Signature::decode` takes.
    const FROZEN_SIG: &str = concat!(
        "untrusted comment: ArmouryDaemon harness probe signature\n",
        "RUTUa5h/DwHowwOgZDyCw1SW8zPgOXgZEiate5aw9feBNNQcbQFVpWBpJCMolAKRQYnMz7ocIfFBkSSJxJ8phVPJXyrOUaO7Ugk=\n",
        "trusted comment: timestamp:1793017762\tfile:man-schema2.json\thashed\n",
        "9lASdxKL/v0BgaevnLFpxCU+1FPYyOJZQx6p1NsO4t2bd6u2+qDG5PmpVQtJ91GIaPsHGQcwkMPQTjN5+RTADQ==\n"
    );
    /// The first line changed, so the frozen manifest text must match byte for byte.
    const FROZEN_MANIFEST: &str = concat!(
        r#"{"schema":2,"updated":"2026-10-26","modules":[{"id":"mode-test","name":"Mode test","version":"9.9.9","kind":"mode","description":"served by the harness","file":"packs/mode-test/pack.json","sha256":"0000000000000000000000000000000000000000000000000000000000000000","size":10}]}"#
    );

    #[test]
    fn the_baked_keyring_parses_and_names_its_keys() {
        let ring = TrustKeyring::from_text(KEYS_JSON, "test").expect("the baked keyring must parse");
        assert!(ring.len() >= 1, "a ring with no key trusts nothing");
    }

    #[test]
    fn a_frozen_signature_verifies_against_its_ring() {
        let ring = TrustKeyring::from_text(
            &format!(r#"{{"schema":1,"keys":[{{"comment":"the frozen test key","key":"{TEST_KEY}"}}]}}"#),
            "test",
        )
        .unwrap();
        ring.verify(FROZEN_MANIFEST.as_bytes(), FROZEN_SIG)
            .expect("the frozen fixture must verify against its own ring");
    }

    #[test]
    fn a_tampered_manifest_does_not_verify() {
        let ring = TrustKeyring::from_text(
            &format!(r#"{{"schema":1,"keys":[{{"key":"{TEST_KEY}"}}]}}"#),
            "test",
        )
        .unwrap();
        let wrong = FROZEN_MANIFEST.replacen("Mode test", "Mode Xest", 1);
        assert_ne!(wrong.as_bytes(), FROZEN_MANIFEST.as_bytes());
        let err = ring
            .verify(wrong.as_bytes(), FROZEN_SIG)
            .expect_err("a changed manifest must not verify");
        assert!(err.contains("no key of the ring accepts it"), "{err}");
    }

    #[test]
    fn a_signature_from_a_stranger_does_not_verify() {
        // the ring holds the REAL key; nothing signed under the fixture key may verify
        let ring = TrustKeyring::from_text(KEYS_JSON, "test").unwrap();
        let err = ring
            .verify(FROZEN_MANIFEST.as_bytes(), FROZEN_SIG)
            .expect_err("a fixture key signature must not verify against the real ring");
        assert!(err.contains("no key of the ring accepts it"), "{err}");
    }

    #[test]
    fn an_empty_or_nonsense_signature_is_refused_by_name() {
        let ring = TrustKeyring::from_text(
            &format!(r#"{{"schema":1,"keys":[{{"key":"{TEST_KEY}"}}]}}"#),
            "test",
        )
        .unwrap();
        assert!(ring.verify(FROZEN_MANIFEST.as_bytes(), "").unwrap_err().contains("does not parse"));
        assert!(ring
            .verify(FROZEN_MANIFEST.as_bytes(), "untrusted comment: x\nnot base64!!\n")
            .unwrap_err()
            .contains("does not parse"));
    }

    #[test]
    fn a_broken_keyring_refuses_to_load_with_a_reason() {
        assert!(TrustKeyring::from_text(r#"{"schema":9,"keys":[]}"#, "test")
            .unwrap_err()
            .contains("schema"));
        assert!(TrustKeyring::from_text(r#"{"schema":1,"keys":[]}"#, "test")
            .unwrap_err()
            .contains("no keys"));
        assert!(TrustKeyring::from_text(r#"{"schema":1,"keys":[{"key":"!!"}]}"#, "test")
            .unwrap_err()
            .contains("does not parse"));
    }
}
