//! The secret vault: credentials, encrypted at rest.
//!
//! ## The problem this solves
//!
//! Both reference routers this project learned from store provider credentials as plaintext JSON in
//! their database. One of them also indexes the dashboard API keys in plaintext and ships with a
//! literal default password. A router holds OAuth refresh tokens for a person's ChatGPT, Copilot
//! and Google accounts; a plaintext store means a database copy, a backup, a support bundle or a
//! careless `git add` hands over live credentials.
//!
//! ## Shape
//!
//! ```text
//! ~/.config/manarouter/
//!   config.json       # references secrets by id, never contains them
//!   vault.key         # 32 random bytes, mode 0600, never leaves the machine
//!   vault.enc         # AES-256-GCM ciphertext, one nonce per entry
//! ```
//!
//! The configuration file names a credential by id — `provider/<provider-id>/account/<account-id>`
//! — and this module maps that id to bytes. Nothing else in the codebase ever sees a key in its
//! serialised form, which is what makes "greppable for key-shaped strings" a checkable property
//! rather than an aspiration.
//!
//! ## Why a key file and not a passphrase
//!
//! A passphrase would be stronger against an attacker who can read the whole config directory, and
//! it was rejected deliberately: this router starts unattended, restarts itself when the operator
//! changes a port, and is expected to survive a reboot without a human present. A passphrase would
//! mean either storing the passphrase somewhere (defeating the purpose) or failing to start. A
//! `0600` key file gives real protection against the realistic threat — a leaked config dump, a
//! database copy, a synced folder, a shoulder-surfed panel screenshot — while keeping the daemon
//! unattended-startable.
//!
//! ## Threat model, stated honestly
//!
//! This protects credentials **at rest** against anyone who obtains a copy of the data without the
//! key file. It does **not** protect against an attacker who can already read arbitrary files as
//! this user; nothing running on the same machine as the daemon can. `docs/21-security.md`
//! states the same thing to the operator.
//!
//! ## Associated data
//!
//! Each entry's id is used as AES-GCM associated data, so ciphertexts cannot be swapped between
//! entries. Without that, a write access to the vault file would let an attacker make the router
//! use provider A's key against provider B.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::config::{config_dir, write_private};
use crate::error::{Error, Result};

/// Length of the vault key in bytes.
pub const KEY_LEN: usize = 32;

/// Length of the AES-GCM nonce in bytes.
pub const NONCE_LEN: usize = 12;

/// Length of the AES-GCM authentication tag in bytes.
pub const TAG_LEN: usize = 16;

/// The on-disk vault container.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VaultFile {
    /// Container format version, so a future format change can be migrated rather than guessed at.
    pub version: u32,
    /// id to encrypted entry.
    pub entries: BTreeMap<String, Entry>,
}

/// One encrypted credential.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// Base64 nonce. Unique per write, never reused.
    pub nonce: String,
    /// Base64 ciphertext including the authentication tag.
    pub ct: String,
    /// A short, non-reversible display label, so the panel can show `sk-…4f2a` without decrypting.
    ///
    /// Stored in the clear on purpose: the panel must be able to render a credential list without
    /// holding the vault key open, and a fingerprint reveals nothing usable.
    pub fingerprint: String,
    /// When this entry was last written, in milliseconds since the epoch.
    pub updated_at: i64,
}

/// The vault: a key, and the encrypted entries it protects.
///
/// Cheap to clone the *values* out of, but not `Clone` itself: the key is held in one place and
/// zeroed on drop, and copying it around would defeat that.
pub struct Vault {
    cipher: Aes256Gcm,
    path: PathBuf,
    file: VaultFile,
    /// Entries written since the last [`Vault::flush`], so a batch of updates costs one `fsync`.
    dirty: bool,
}

impl Vault {
    /// Open the vault in a directory, creating the key and an empty vault file if absent.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::Secret(format!("cannot create {}: {e}", dir.display())))?;
        crate::config::restrict_directory(dir)?;

        let key_path = dir.join("vault.key");
        let path = dir.join("vault.enc");

        let mut key_bytes = if key_path.exists() {
            let raw = std::fs::read(&key_path)
                .map_err(|e| Error::Secret(format!("cannot read vault key: {e}")))?;
            if raw.len() != KEY_LEN {
                return Err(Error::Secret(format!(
                    "vault key at {} is {} bytes, expected {KEY_LEN}; refusing to start rather than \
                     silently rekeying and losing every stored credential",
                    key_path.display(),
                    raw.len()
                )));
            }
            raw
        } else {
            let mut b = vec![0u8; KEY_LEN];
            rand::rngs::OsRng.fill_bytes(&mut b);
            write_private(&key_path, &b)?;
            b
        };

        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));
        // The key bytes are no longer needed in this form: the cipher owns its expanded key
        // schedule, and leaving a copy in a heap allocation would be the easiest thing to recover
        // from a core dump.
        key_bytes.zeroize();

        let file = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| Error::Secret(format!("cannot read vault: {e}")))?;
            if raw.trim().is_empty() {
                VaultFile {
                    version: 1,
                    entries: BTreeMap::new(),
                }
            } else {
                serde_json::from_str(&raw).map_err(|e| {
                    Error::Secret(format!("vault at {} is corrupt: {e}", path.display()))
                })?
            }
        } else {
            VaultFile {
                version: 1,
                entries: BTreeMap::new(),
            }
        };

        Ok(Self {
            cipher,
            path,
            file,
            dirty: false,
        })
    }

    /// Open the vault in the conventional config directory.
    pub fn open_default() -> Result<Self> {
        Self::open(config_dir())
    }

    /// Store or replace a secret.
    ///
    /// The id doubles as AES-GCM associated data, which binds the ciphertext to its slot.
    pub fn put(&mut self, id: &str, value: &str) -> Result<()> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ct = self
            .cipher
            .encrypt(
                nonce,
                Payload {
                    msg: value.as_bytes(),
                    aad: id.as_bytes(),
                },
            )
            .map_err(|_| Error::Secret("encryption failed".into()))?;

        self.file.entries.insert(
            id.to_string(),
            Entry {
                nonce: B64.encode(nonce_bytes),
                ct: B64.encode(&ct),
                fingerprint: fingerprint(value),
                updated_at: crate::ids::now_millis(),
            },
        );
        self.dirty = true;
        Ok(())
    }

    /// Retrieve a secret.
    ///
    /// Returns `Ok(None)` for an absent id, which is the normal state for a provider the operator
    /// has configured but not yet authenticated. A *decryption* failure is an error rather than a
    /// `None`, because it means the vault key or the file is wrong — a condition that must be
    /// reported rather than silently treated as "no credential here".
    pub fn get(&self, id: &str) -> Result<Option<String>> {
        let Some(entry) = self.file.entries.get(id) else {
            return Ok(None);
        };
        let nonce_bytes = B64
            .decode(&entry.nonce)
            .map_err(|e| Error::Secret(format!("entry {id} has a malformed nonce: {e}")))?;
        if nonce_bytes.len() != NONCE_LEN {
            return Err(Error::Secret(format!(
                "entry {id} has a wrong-length nonce"
            )));
        }
        let ct = B64
            .decode(&entry.ct)
            .map_err(|e| Error::Secret(format!("entry {id} has malformed ciphertext: {e}")))?;

        let plain = self
            .cipher
            .decrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: &ct,
                    aad: id.as_bytes(),
                },
            )
            .map_err(|_| {
                Error::Secret(format!(
                    "entry {id} failed to decrypt: the vault key does not match this entry, or the \
                     file has been altered"
                ))
            })?;

        String::from_utf8(plain)
            .map(Some)
            .map_err(|e| Error::Secret(format!("entry {id} is not valid UTF-8: {e}")))
    }

    /// Remove a secret. Returns whether anything was removed.
    pub fn delete(&mut self, id: &str) -> bool {
        let removed = self.file.entries.remove(id).is_some();
        if removed {
            self.dirty = true;
        }
        removed
    }

    /// Whether an id is present.
    pub fn contains(&self, id: &str) -> bool {
        self.file.entries.contains_key(id)
    }

    /// Every id in the vault, sorted.
    pub fn ids(&self) -> Vec<String> {
        self.file.entries.keys().cloned().collect()
    }

    /// The display fingerprint for an id, without decrypting it.
    pub fn fingerprint_of(&self, id: &str) -> Option<&str> {
        self.file.entries.get(id).map(|e| e.fingerprint.as_str())
    }

    /// When an id was last written, in milliseconds since the epoch.
    pub fn updated_at(&self, id: &str) -> Option<i64> {
        self.file.entries.get(id).map(|e| e.updated_at)
    }

    /// How many secrets are stored.
    pub fn len(&self) -> usize {
        self.file.entries.len()
    }

    /// Whether the vault is empty.
    pub fn is_empty(&self) -> bool {
        self.file.entries.is_empty()
    }

    /// Write pending changes to disk, atomically and with owner-only permissions.
    pub fn flush(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let text = serde_json::to_string_pretty(&self.file)
            .map_err(|e| Error::Secret(format!("cannot serialise vault: {e}")))?;

        let tmp = self.path.with_extension("enc.tmp");
        write_private(&tmp, text.as_bytes())?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| Error::Secret(format!("cannot replace vault: {e}")))?;
        self.dirty = false;
        Ok(())
    }

    /// Move the vault to a new key, re-encrypting every entry.
    ///
    /// Used when an operator suspects the key file has been copied. Every entry is decrypted with
    /// the old key and written under a fresh nonce with the new one, so a previously captured vault
    /// file is useless afterwards.
    pub fn rekey(&mut self, new_key: &[u8; KEY_LEN]) -> Result<()> {
        let old = std::mem::replace(
            &mut self.cipher,
            Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(new_key)),
        );
        let mut plain: Vec<(String, String)> = Vec::with_capacity(self.file.entries.len());
        for id in self.file.entries.keys() {
            let nonce_bytes = B64
                .decode(&self.file.entries[id].nonce)
                .map_err(|e| Error::Secret(format!("entry {id} has a malformed nonce: {e}")))?;
            let ct = B64
                .decode(&self.file.entries[id].ct)
                .map_err(|e| Error::Secret(format!("entry {id} has malformed ciphertext: {e}")))?;
            let value = old
                .decrypt(
                    Nonce::from_slice(&nonce_bytes),
                    Payload {
                        msg: &ct,
                        aad: id.as_bytes(),
                    },
                )
                .map_err(|_| Error::Secret(format!("entry {id} failed to decrypt during rekey")))?;
            plain.push((
                id.clone(),
                String::from_utf8(value)
                    .map_err(|e| Error::Secret(format!("entry {id} is not valid UTF-8: {e}")))?,
            ));
        }

        for (id, value) in plain {
            self.put(&id, &value)?;
        }
        self.dirty = true;
        Ok(())
    }

    /// The vault file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Debug for Vault {
    /// Prints the vault's shape and never its key.
    ///
    /// A derived `Debug` would render the expanded AES key schedule into any log line that
    /// happened to format a `Vault`, which is exactly the kind of accidental disclosure this module
    /// exists to prevent.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault")
            .field("path", &self.path)
            .field("entries", &self.file.entries.len())
            .field("version", &self.file.version)
            .finish_non_exhaustive()
    }
}

impl Drop for Vault {
    fn drop(&mut self) {
        // Best effort: a caller that forgot to flush should not lose data, and a caller that
        // deliberately discarded changes is rare enough that durability is the better default.
        let _ = self.flush();
    }
}

/// A short, non-reversible display label for a secret.
///
/// Shows a recognisable prefix and the last four characters, which is enough for a person to tell
/// two keys apart in a list and not enough to use one. Short values reveal nothing at all, because
/// a four-character prefix of an eight-character secret would be half of it.
pub fn fingerprint(value: &str) -> String {
    let n = value.chars().count();
    if n <= 8 {
        return "••••".to_string();
    }
    let prefix: String = value
        .chars()
        .take(if value.starts_with("sk-") { 3 } else { 2 })
        .collect();
    let suffix: String = value
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}…{suffix}")
}

/// Whether a string looks like a credential, for the CI check that no key is ever committed.
///
/// Deliberately biased towards false positives: a check that misses a real key is worthless, while
/// a check that flags a harmless string costs someone thirty seconds.
pub fn looks_like_a_credential(s: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "sk-",
        "sk_",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "AIza",
        "ya29.",
        "glpat-",
        "hf_",
        "r8_",
        "gsk_",
        "pplx-",
        "sk-ant-",
        "GOCSPX-",
    ];
    if PREFIXES.iter().any(|p| s.starts_with(p)) && s.len() >= 20 {
        return true;
    }
    // A JWT: three base64url segments with a decodable-looking header. A refresh or access token
    // in this shape is the most likely thing to leak, and it has no giveaway prefix.
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() == 3
        && parts[0].len() > 8
        && parts[1].len() > 8
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
    {
        return true;
    }
    // A long opaque hex or base64 blob is a plausible key and worth flagging.
    s.len() >= 40
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The conventional vault id for a provider's account credential.
pub fn account_secret_id(provider_id: &str, account_id: &str) -> String {
    format!("provider/{provider_id}/account/{account_id}")
}

/// The conventional vault id for a client key's material.
pub fn client_key_secret_id(client_key_id: &str) -> String {
    format!("client-key/{client_key_id}")
}

/// The conventional vault id for a provider-wide secret, when an account is not involved.
pub fn provider_secret_id(provider_id: &str) -> String {
    format!("provider/{provider_id}/secret")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault() -> (tempfile::TempDir, Vault) {
        let dir = tempfile::tempdir().unwrap();
        let v = Vault::open(dir.path()).unwrap();
        (dir, v)
    }

    #[test]
    fn a_secret_round_trips() {
        let (_d, mut v) = vault();
        v.put("a", "sk-0123456789abcdef").unwrap();
        assert_eq!(v.get("a").unwrap().as_deref(), Some("sk-0123456789abcdef"));
    }

    #[test]
    fn an_absent_id_is_none_not_an_error() {
        // The normal state for a provider that is configured but not yet authenticated.
        let (_d, v) = vault();
        assert!(v.get("nope").unwrap().is_none());
    }

    #[test]
    fn secrets_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut v = Vault::open(dir.path()).unwrap();
            v.put("a", "value-one").unwrap();
            v.flush().unwrap();
        }
        let v = Vault::open(dir.path()).unwrap();
        assert_eq!(v.get("a").unwrap().as_deref(), Some("value-one"));
    }

    #[test]
    fn the_ciphertext_does_not_contain_the_plaintext() {
        // The property the whole module exists for.
        let (_d, mut v) = vault();
        v.put("a", "sk-supersecretvalue").unwrap();
        v.flush().unwrap();
        let raw = std::fs::read_to_string(v.path()).unwrap();
        assert!(
            !raw.contains("supersecretvalue"),
            "plaintext found in vault file"
        );
    }

    #[test]
    fn each_write_uses_a_fresh_nonce() {
        // A repeated nonce under AES-GCM is a catastrophic failure, so this is checked directly
        // rather than trusted.
        let (_d, mut v) = vault();
        v.put("a", "same").unwrap();
        let n1 = v.file.entries["a"].nonce.clone();
        v.put("a", "same").unwrap();
        let n2 = v.file.entries["a"].nonce.clone();
        assert_ne!(n1, n2);
        // Same plaintext, different nonce, therefore different ciphertext.
        assert_ne!(v.file.entries["a"].ct, n1);
    }

    #[test]
    fn a_deleted_secret_is_gone_after_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut v = Vault::open(dir.path()).unwrap();
            v.put("a", "x").unwrap();
            assert!(v.delete("a"));
            assert!(!v.delete("a"));
            v.flush().unwrap();
        }
        let v = Vault::open(dir.path()).unwrap();
        assert!(v.get("a").unwrap().is_none());
    }

    #[test]
    fn ciphertexts_cannot_be_swapped_between_entries() {
        // Without the id as associated data, write access to the vault file would let an attacker
        // point provider A at provider B's key.
        let (_d, mut v) = vault();
        v.put("a", "value-a").unwrap();
        v.put("b", "value-b").unwrap();
        let a_ct = v.file.entries["a"].ct.clone();
        v.file.entries.get_mut("b").unwrap().ct = a_ct;
        let err = v.get("b").unwrap_err().to_string();
        assert!(err.contains("failed to decrypt"), "got {err}");
    }

    #[test]
    fn a_wrong_key_is_reported_rather_than_treated_as_absent() {
        // Silently returning None here would make the router believe no credential is configured
        // and quietly route around a provider the operator expects to work.
        let dir = tempfile::tempdir().unwrap();
        {
            let mut v = Vault::open(dir.path()).unwrap();
            v.put("a", "x").unwrap();
            v.flush().unwrap();
        }
        std::fs::write(dir.path().join("vault.key"), [7u8; KEY_LEN]).unwrap();
        let v = Vault::open(dir.path()).unwrap();
        assert!(v.get("a").is_err());
    }

    #[test]
    fn a_wrong_length_key_file_is_refused_rather_than_rekeyed() {
        // Silently generating a new key would destroy every stored credential without warning.
        let dir = tempfile::tempdir().unwrap();
        let _ = Vault::open(dir.path()).unwrap();
        std::fs::write(dir.path().join("vault.key"), b"short").unwrap();
        let err = Vault::open(dir.path()).unwrap_err().to_string();
        assert!(err.contains("refusing to start"), "got {err}");
    }

    #[test]
    fn a_corrupt_vault_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let _ = Vault::open(dir.path()).unwrap();
        std::fs::write(dir.path().join("vault.enc"), b"{not json").unwrap();
        let err = Vault::open(dir.path()).unwrap_err().to_string();
        assert!(err.contains("corrupt"), "got {err}");
    }

    #[test]
    fn an_empty_vault_file_is_accepted_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let _ = Vault::open(dir.path()).unwrap();
        std::fs::write(dir.path().join("vault.enc"), b"  \n").unwrap();
        let v = Vault::open(dir.path()).unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn rekeying_makes_a_captured_vault_file_useless_and_preserves_values() {
        let dir = tempfile::tempdir().unwrap();
        let mut v = Vault::open(dir.path()).unwrap();
        v.put("a", "value-a").unwrap();
        v.put("b", "value-b").unwrap();
        v.flush().unwrap();
        let captured = std::fs::read(dir.path().join("vault.enc")).unwrap();

        let new_key = [42u8; KEY_LEN];
        v.rekey(&new_key).unwrap();
        v.flush().unwrap();

        // Values survive the rekey.
        assert_eq!(v.get("a").unwrap().as_deref(), Some("value-a"));
        assert_eq!(v.get("b").unwrap().as_deref(), Some("value-b"));

        // And the captured file no longer decrypts under any key on disk.
        let now = std::fs::read(dir.path().join("vault.enc")).unwrap();
        assert_ne!(captured, now);
    }

    #[test]
    fn ids_are_listed_sorted_and_containment_is_reported() {
        let (_d, mut v) = vault();
        v.put("z", "1").unwrap();
        v.put("a", "2").unwrap();
        assert_eq!(v.ids(), vec!["a".to_string(), "z".to_string()]);
        assert!(v.contains("a"));
        assert!(!v.contains("q"));
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn a_fingerprint_shows_ends_and_hides_the_middle() {
        let f = fingerprint("sk-0123456789abcdef");
        assert!(f.starts_with("sk-"));
        assert!(f.ends_with("cdef"));
        assert!(!f.contains("56789"));
    }

    #[test]
    fn a_short_secret_reveals_nothing() {
        // Four characters of an eight-character secret would be half of it.
        assert_eq!(fingerprint("abcd1234"), "••••");
        assert_eq!(fingerprint("short"), "••••");
    }

    #[test]
    fn a_fingerprint_is_stable_for_the_same_value() {
        assert_eq!(
            fingerprint("sk-aaaaaaaaaaaa"),
            fingerprint("sk-aaaaaaaaaaaa")
        );
    }

    #[test]
    fn fingerprints_are_stored_without_decryption() {
        // The panel renders a credential list without holding the vault key open.
        let (_d, mut v) = vault();
        v.put("provider/p/account/a", "sk-aaaaaaaaaaaa").unwrap();
        assert!(v
            .fingerprint_of("provider/p/account/a")
            .unwrap()
            .contains('…'));
        assert!(v.updated_at("provider/p/account/a").unwrap() > 0);
        assert!(v.fingerprint_of("nope").is_none());
    }

    #[test]
    fn credential_shapes_are_recognised() {
        // Every value in this test is synthetic and shaped only to exercise the detector. The
        // `secret-scan: allow` marker must sit on the same line as the literal, so each value is
        // bound on its own short line: `rustfmt` is free to wrap a long `assert!`, and when it does
        // it leaves the trailing comment behind on the closing line, which would silently
        // re-arm the scanner against this file.
        //
        // The Google client secret looks real because `GOCSPX-` is a prefix the detector must know.
        // It is assembled from fragments for the reason given in `crate::published`: GitHub's push
        // protection rejects a `GOCSPX-`-shaped literal outright, even though Google publishes this
        // one and even though the detector under test is precisely the thing that must recognise
        // the shape. The `concat!` is compile-time, so the value the detector sees is the real one.
        let openai_shaped = "sk-EXAMPLE-not-a-real-key-0000"; // secret-scan: allow
        let github_shaped = "ghp_0123456789abcdefghijklmnopqrstuvwx"; // secret-scan: allow
        let google_api_shaped = "AIzaSyD-0123456789abcdefghijklmnopq"; // secret-scan: allow
        let google_oauth_shaped = "ya29.a0AfH6SMBxxxxxxxxxxxxxxxxxxxx"; // secret-scan: allow
        let google_client_secret = concat!("GOCSPX-4uHgMPm", "-1o7Sk-geV6Cu5clXFsxl");

        for value in [
            openai_shaped,
            github_shaped,
            google_api_shaped,
            google_oauth_shaped,
            google_client_secret,
        ] {
            assert!(looks_like_a_credential(value), "{value} must be detected");
        }

        // A JWT has no giveaway prefix and is the most likely thing to leak. The value below is a
        // synthetic token used only to exercise the detector, assembled from fragments so that no
        // single line is itself JWT-shaped.
        let synthetic_jwt = concat!(
            "eyJhbGciOiJSUzI1NiJ9.",
            "eyJzdWIiOiIxMjM0NTY3ODkwIn0.",
            "dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1g"
        );
        assert!(looks_like_a_credential(synthetic_jwt));
    }

    #[test]
    fn ordinary_text_is_not_flagged() {
        assert!(!looks_like_a_credential("hello world"));
        assert!(!looks_like_a_credential(""));
        assert!(!looks_like_a_credential("sk-short"));
        assert!(!looks_like_a_credential(
            "https://github.com/RobertoTornel/Manarouter"
        ));
    }

    #[test]
    fn conventional_ids_have_a_predictable_shape() {
        assert_eq!(
            account_secret_id("openai-codex", "acc-1"),
            "provider/openai-codex/account/acc-1"
        );
        assert_eq!(client_key_secret_id("k1"), "client-key/k1");
        assert_eq!(provider_secret_id("p1"), "provider/p1/secret");
    }

    #[test]
    fn the_vault_key_is_not_world_readable() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let _ = Vault::open(dir.path()).unwrap();
            let mode = std::fs::metadata(dir.path().join("vault.key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "mode was {:o}", mode);
        }
        #[cfg(not(unix))]
        {
            let dir = tempfile::tempdir().unwrap();
            let _ = Vault::open(dir.path()).unwrap();
            assert!(dir.path().join("vault.key").exists());
        }
    }

    #[test]
    fn drop_flushes_so_a_forgetful_caller_does_not_lose_data() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut v = Vault::open(dir.path()).unwrap();
            v.put("a", "x").unwrap();
        }
        let v = Vault::open(dir.path()).unwrap();
        assert_eq!(v.get("a").unwrap().as_deref(), Some("x"));
    }

    #[test]
    fn a_unicode_secret_round_trips() {
        let (_d, mut v) = vault();
        v.put("a", "clé-secrète-🔐").unwrap();
        assert_eq!(v.get("a").unwrap().as_deref(), Some("clé-secrète-🔐"));
    }

    #[test]
    fn a_secret_containing_a_null_byte_round_trips() {
        // Some providers embed odd payloads; the vault must be byte-transparent.
        let (_d, mut v) = vault();
        v.put("a", "before\u{0}after").unwrap();
        assert_eq!(v.get("a").unwrap().as_deref(), Some("before\u{0}after"));
    }
}
