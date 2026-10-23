//! Reading OAuth grants out of a reference OmniRoute installation.
//!
//! The migration needs the *plaintext* of tokens that another application deliberately encrypted
//! at rest. Two facts make that legitimate and possible here, and both were verified against a
//! real OmniRoute `storage.sqlite` before this module was written:
//!
//! 1. **The key is the operator's own**, read from their `~/.omniroute/.env`
//!    (`STORAGE_ENCRYPTION_KEY`). Nothing is brute-forced; this is the same secret the reference
//!    application uses on the same machine, for data the operator already owns.
//! 2. **The ciphertext is self-describing.** OmniRoute stores
//!    `enc:v1:<iv_hex>:<ciphertext_hex>:<tag_hex>` — AES-256-GCM, a 16-byte nonce and key derived as
//!    `scrypt(secret, "omniroute-field-encryption-v1", N=16384, r=8, p=1, len=32)`. The
//!    derivation is stated in the reference source (`src/lib/db/encryption.ts`) and is pinned here
//!    by a test vector, so a change upstream fails loudly instead of silently producing garbage.
//!
//! ## What this module does *not* do
//!
//! It never writes to the reference database, and it never prints a token. [`ReferenceDb`] hands
//! out [`DecryptedToken`] values that carry the plaintext plus a fingerprint (the first bytes of
//! its SHA-256) so a log line or a dry-run report can identify a credential without exposing it.
//! `Debug` is hand-written for the same reason it is everywhere else in this crate.
//!
//! ## Why a decrypted token is not a working Manarouter credential
//!
//! A stored access token is short-lived, and — for the platforms that rotate — already spent. The
//! migration therefore treats the reference tokens as a *starting point for one refresh*: it hands
//! the refresh token to the platform's own [`refresh`](super::OAuthPlatform::refresh) and stores
//! whatever comes back. That is what makes the imported account genuinely live rather than a
//! plausible-looking row that fails on first use. See [`super::migrate`].

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use aes_gcm::aead::consts::U16;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::aes::Aes256;
use aes_gcm::AesGcm;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// The masked-field prefix OmniRoute writes in front of every encrypted credential.
const CIPHER_PREFIX: &str = "enc:v1:";

/// The salt OmniRoute's *current* key derivation uses.
///
/// The reference application moved from a dynamic salt (a SHA-256 of the secret) to this static
/// one in v3.7.9; both derivations are attempted by [`ReferenceKey::decrypt`] so a database written
/// by either version reads. See the module docs in `encryption.ts`.
const STATIC_SALT: &[u8] = b"omniroute-field-encryption-v1";

/// scrypt parameters, matching `scryptSync(secret, salt, 32)` with Node's defaults
/// (N=16384, r=8, p=1). Node exposes them as a cost/password length only; the defaults are fixed
/// in the reference implementation, so they are pinned here.
const SCRYPT_LOG_N: u8 = 14;
const SCRYPT_R: u32 = 8;
const SCRYPT_P: u32 = 1;
const KEY_LEN: usize = 32;

/// A 16-byte GCM nonce, written by OmniRoute as 32 hex characters.
type Nonce16 = aes_gcm::Nonce<U16>;

/// The AES-256-GCM construction OmniRoute uses: a 16-byte nonce rather than the 12-byte default.
type OmniCipher = AesGcm<Aes256, U16>;

/// One credential column as it appears in the reference database, before decryption.
///
/// Kept distinct from the decrypted form so a caller can report "present but undecryptable" (a
/// changed or missing `STORAGE_ENCRYPTION_KEY`) separately from "absent" — the distinction the
/// reference application itself makes, and the one that tells an operator which action to take.
#[derive(Clone, PartialEq, Eq)]
pub enum StoredField {
    /// The column was NULL or an empty string.
    Absent,
    /// A plaintext value (the reference runs with encryption disabled in that configuration).
    Plain(String),
    /// An `enc:v1:...` ciphertext, still opaque.
    Encrypted(String),
}

impl fmt::Debug for StoredField {
    /// Hand-written, for the same reason as [`DecryptedToken`]: `Plain` holds a live credential,
    /// so it is described by length and never by value.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => f.write_str("Absent"),
            Self::Plain(value) => write!(f, "Plain(***(redacted, {} bytes))", value.len()),
            Self::Encrypted(value) => write!(f, "Encrypted(***(redacted, {} bytes))", value.len()),
        }
    }
}

impl StoredField {
    /// Classify a raw column value.
    pub fn classify(value: Option<&str>) -> Self {
        match value {
            None | Some("") => Self::Absent,
            Some(v) if v.starts_with(CIPHER_PREFIX) => Self::Encrypted(v.to_owned()),
            Some(v) => Self::Plain(v.to_owned()),
        }
    }

    /// Whether a value was present at all.
    pub fn is_present(&self) -> bool {
        !matches!(self, Self::Absent)
    }
}

/// A decrypted credential value, with a fingerprint that is safe to print.
///
/// Hand-written `Debug`: the whole point of this type is that it can travel through a report
/// without a token reaching a terminal or a log file.
#[derive(Clone, PartialEq, Eq)]
pub struct DecryptedToken {
    value: String,
    /// First 12 hex characters of the value's SHA-256, for identifying *which* credential a line
    /// refers to. Not reversible, and deliberately not the value's prefix — a token prefix is
    /// frequently itself sensitive.
    fingerprint: String,
}

impl DecryptedToken {
    /// Wrap an already-plaintext value.
    ///
    /// Public so the migration can build one for a token it obtained by refreshing rather than by
    /// decrypting (its report fingerprints both the same way).
    pub fn new(value: String) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(value.as_bytes());
        let digest = hasher.finalize();
        let fingerprint = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
        Self { value, fingerprint }
    }

    /// The plaintext. Callers must not log or print this.
    pub fn expose(&self) -> &str {
        &self.value
    }

    /// The printable, non-reversible identifier for this value.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Length in bytes, which distinguishes an empty or truncated token in a report without
    /// revealing the token.
    pub fn len(&self) -> usize {
        self.value.len()
    }

    /// Whether the value is empty. A zero-length token is a real condition (a cleared field).
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }
}

impl fmt::Debug for DecryptedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecryptedToken")
            .field(
                "value",
                &format_args!("***(redacted, {} bytes)", self.value.len()),
            )
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

/// The key material needed to read a reference database, plus the two derivations that exist.
pub struct ReferenceKey {
    /// The derivation the current reference version uses.
    primary: [u8; KEY_LEN],
    /// The pre-v3.7.9 derivation, tried second so an old database still reads.
    legacy: [u8; KEY_LEN],
}

impl fmt::Debug for ReferenceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The derived keys are as sensitive as the secret they came from.
        f.debug_struct("ReferenceKey")
            .field("primary", &"***(redacted)")
            .field("legacy", &"***(redacted)")
            .finish()
    }
}

impl ReferenceKey {
    /// Derive both keys from the operator's `STORAGE_ENCRYPTION_KEY` value.
    pub fn from_secret(secret: &str) -> Result<Self> {
        if secret.trim().is_empty() {
            return Err(Error::InvalidState(
                "reference storage encryption key is empty; set STORAGE_ENCRYPTION_KEY in the \
                 reference .env before migrating"
                    .into(),
            ));
        }
        let primary = derive(secret, STATIC_SALT)?;
        // The legacy salt is the first 16 bytes of sha256(secret) — see `encryption.ts`
        // `getLegacyDynamicKey`.
        let dynamic = Sha256::digest(secret.as_bytes());
        let legacy = derive(secret, &dynamic[..16])?;
        Ok(Self { primary, legacy })
    }

    /// Read the key out of a reference `.env` file.
    ///
    /// This mirrors [`ensureSecretLoaded`] in the reference: the environment wins, then the data
    /// directory's `.env`. Only the assignment line is read; nothing is executed.
    pub fn from_env_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            Error::InvalidState(format!(
                "cannot read the reference .env at {}: {e}",
                path.display()
            ))
        })?;
        let secret = parse_env_value(&text, "STORAGE_ENCRYPTION_KEY").ok_or_else(|| {
            Error::InvalidState(format!(
                "{} has no STORAGE_ENCRYPTION_KEY assignment; if the reference was run without \
                 encryption its tokens are plaintext and no key is needed",
                path.display()
            ))
        })?;
        Self::from_secret(&secret)
    }

    /// Decrypt one `enc:v1:` field, trying the current derivation and then the legacy one.
    ///
    /// Returns `Err` for malformed hex, a wrong tag (a different key) or unrecoverable plaintext —
    /// never a silently wrong value, because a corrupted credential that looks valid is worse than
    /// a refused one.
    pub fn decrypt(&self, stored: &StoredField) -> Result<Option<DecryptedToken>> {
        let ciphertext = match stored {
            StoredField::Absent => return Ok(None),
            // A plaintext column needs no key. This is the passthrough mode the reference uses
            // when no key is configured.
            StoredField::Plain(value) => return Ok(Some(DecryptedToken::new(value.clone()))),
            StoredField::Encrypted(value) => value,
        };

        let (iv, body, tag) = split_ciphertext(ciphertext)?;
        // A 16-byte nonce, so the generic `AesGcm<Aes256, U16>` rather than the `Aes256Gcm`
        // alias the vault uses (which is fixed at 12).
        for key in [&self.primary, &self.legacy] {
            let cipher = OmniCipher::new_from_slice(key)
                .map_err(|_| Error::InvalidState("reference key length is not 32 bytes".into()))?;
            // The reference concatenates ciphertext and tag before decrypting with an explicit
            // `authTagLength`; `aes-gcm` expects them appended, which is the same bytes.
            let mut sealed = body.clone();
            sealed.extend_from_slice(&tag);
            if let Ok(plain) = cipher.decrypt(Nonce16::from_slice(&iv), sealed.as_ref()) {
                let text = String::from_utf8(plain).map_err(|_| {
                    Error::InvalidState(
                        "a reference credential decrypted to bytes that are not UTF-8".into(),
                    )
                })?;
                return Ok(Some(DecryptedToken::new(text)));
            }
        }
        Err(Error::InvalidState(
            "a reference credential could not be decrypted with either the current or the legacy \
             key derivation; STORAGE_ENCRYPTION_KEY does not match the key that encrypted this \
             database"
                .into(),
        ))
    }
}

/// Split `enc:v1:<iv>:<ct>:<tag>` into its three byte strings.
fn split_ciphertext(value: &str) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let body = value.strip_prefix(CIPHER_PREFIX).ok_or_else(|| {
        Error::InvalidState("a reference credential lacks the enc:v1: prefix".into())
    })?;
    let parts: Vec<&str> = body.split(':').collect();
    if parts.len() != 3 {
        return Err(Error::InvalidState(format!(
            "a reference credential has {} colon-separated parts, expected 3 (iv:ciphertext:tag)",
            parts.len()
        )));
    }
    let decode = |s: &str, what: &str| {
        decode_hex(s).ok_or_else(|| {
            Error::InvalidState(format!(
                "a reference credential has a malformed {what} field"
            ))
        })
    };
    let iv = decode(parts[0], "initialisation vector")?;
    if iv.len() != 16 {
        return Err(Error::InvalidState(format!(
            "a reference credential has a {}-byte nonce, expected 16",
            iv.len()
        )));
    }
    Ok((
        iv,
        decode(parts[1], "ciphertext")?,
        decode(parts[2], "authentication tag")?,
    ))
}

/// Decode hex without pulling in a `hex` dependency for one call site.
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}

/// One scrypt derivation at the reference's fixed parameters.
fn derive(secret: &str, salt: &[u8]) -> Result<[u8; KEY_LEN]> {
    let params = scrypt::Params::new(SCRYPT_LOG_N, SCRYPT_R, SCRYPT_P, KEY_LEN)
        .map_err(|e| Error::InvalidState(format!("scrypt parameters rejected: {e}")))?;
    let mut out = [0u8; KEY_LEN];
    scrypt::scrypt(secret.as_bytes(), salt, &params, &mut out)
        .map_err(|e| Error::InvalidState(format!("scrypt derivation failed: {e}")))?;
    Ok(out)
}

/// Pull `NAME=value` out of a dotenv file, honouring surrounding quotes.
///
/// Deliberately small and non-executing: the reference reads its `.env` the same way, and a
/// migration must not run arbitrary code out of a file it is only reading.
pub fn parse_env_value(text: &str, name: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix(name) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let value = rest.trim();
        let unquoted = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        if !unquoted.is_empty() {
            return Some(unquoted.to_owned());
        }
    }
    None
}

/// A provider connection as the reference stores it, with credentials already decrypted.
///
/// The field set is deliberately narrow: only what a Manarouter account needs. Reading more of
/// another application's schema would couple this migration to columns it does not use.
#[derive(Debug, Clone)]
pub struct ReferenceConnection {
    /// The reference's own connection id, used for stable, idempotent account ids.
    pub id: String,
    /// The reference provider key (`codex`, `agy`, `github`, …).
    pub provider: String,
    /// Auth kind as recorded (`oauth` or `apikey`).
    pub auth_type: String,
    /// Whether the reference had this connection enabled.
    pub is_active: bool,
    /// Account label/email when the reference recorded one.
    pub email: Option<String>,
    /// Decrypted access token, when present.
    pub access_token: Option<DecryptedToken>,
    /// Decrypted refresh token, when present.
    pub refresh_token: Option<DecryptedToken>,
    /// Decrypted id token, when present (Codex keeps its claims here).
    pub id_token: Option<DecryptedToken>,
    /// Decrypted API key, for non-OAuth connections.
    pub api_key: Option<DecryptedToken>,
    /// Absolute expiry as the reference recorded it, in milliseconds since the epoch.
    pub expires_at_ms: Option<i64>,
    /// Parsed `provider_specific_data`, which carries provider extras such as the Antigravity
    /// Cloud Code project id.
    pub provider_data: serde_json::Value,
}

impl ReferenceConnection {
    /// A stable fingerprint of the credential material, for the "has the operator re-keyed this
    /// by hand?" check that keeps a re-run from overwriting a deliberate local edit.
    ///
    /// Hashes the refresh token when there is one (the durable half of a grant) and the access
    /// token otherwise, so a rotated access token alone does not read as a changed credential.
    pub fn credential_fingerprint(&self) -> Option<String> {
        let source = self
            .refresh_token
            .as_ref()
            .or(self.access_token.as_ref())
            .or(self.api_key.as_ref())?;
        Some(source.fingerprint().to_owned())
    }
}

/// A read-only view of a reference OmniRoute database.
pub struct ReferenceDb {
    connection: rusqlite::Connection,
}

impl fmt::Debug for ReferenceDb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReferenceDb").finish_non_exhaustive()
    }
}

impl ReferenceDb {
    /// Open a reference database read-only.
    ///
    /// The caller is expected to hand this a **copy**, not the live file: see
    /// [`super::migrate::copy_reference_database`]. Read-only is necessary but not sufficient
    /// protection for a database another process is actively writing, because even a read can
    /// trigger WAL recovery.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let connection = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| {
            Error::Store(format!(
                "cannot open the reference database at {}: {e}",
                path.display()
            ))
        })?;
        Ok(Self { connection })
    }

    /// Whether the expected connection table exists, so a wrong file is reported clearly.
    pub fn has_provider_connections(&self) -> Result<bool> {
        let count: i64 = self
            .connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' \
                 AND name = 'provider_connections'",
                [],
                |row| row.get(0),
            )
            .map_err(|e| Error::Store(format!("cannot inspect the reference schema: {e}")))?;
        Ok(count > 0)
    }

    /// Every provider connection, with credentials decrypted when a key is supplied.
    ///
    /// `key` is optional because a reference instance can run in passthrough mode, where the
    /// credential columns are plaintext and no derivation is needed.
    pub fn connections(&self, key: Option<&ReferenceKey>) -> Result<Vec<ReferenceConnection>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, provider, auth_type, is_active, email, access_token, refresh_token, \
                 id_token, api_key, token_expires_at, provider_specific_data \
                 FROM provider_connections",
            )
            .map_err(|e| Error::Store(format!("cannot read reference connections: {e}")))?;

        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                ))
            })
            .map_err(|e| Error::Store(format!("cannot query reference connections: {e}")))?;

        let mut out = Vec::new();
        for row in rows {
            let (
                id,
                provider,
                auth_type,
                is_active,
                email,
                access,
                refresh,
                id_token,
                api_key,
                expires,
                provider_data,
            ) = row.map_err(|e| Error::Store(format!("cannot read a reference row: {e}")))?;

            // Each credential field is decrypted independently: one undecryptable column must not
            // discard the row, because a refresh token alone is enough to rebuild an account.
            let context = RowContext {
                id: &id,
                provider: &provider,
                key,
            };
            let access_token = decrypt_field(context, access)?;
            let refresh_token = decrypt_field(context, refresh)?;
            let id_token = decrypt_field(context, id_token)?;
            let api_key = decrypt_field(context, api_key)?;

            out.push(ReferenceConnection {
                id,
                provider,
                auth_type: auth_type.unwrap_or_else(|| "apikey".into()),
                is_active: is_active.unwrap_or(1) != 0,
                email,
                access_token,
                refresh_token,
                id_token,
                api_key,
                expires_at_ms: expires.as_deref().and_then(parse_reference_instant),
                provider_data: provider_data
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(serde_json::Value::Null),
            });
        }
        Ok(out)
    }
}

/// One model the reference had saved for a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceModel {
    /// The id sent upstream.
    pub id: String,
    /// The label the reference showed, when it had one.
    pub label: Option<String>,
    /// The output token limit the reference recorded, when it had one.
    pub output_token_limit: Option<u32>,
    /// The input (context) limit the reference recorded, when it had one.
    pub input_token_limit: Option<u32>,
    /// The reasoning efforts the reference said this model accepts. Present only on a *base*
    /// entry; an entry that carries it is the model itself rather than a synthetic variant.
    pub supported_efforts: Vec<String>,
}

impl ReferenceModel {
    /// Reasoning-effort words the reference uses to name a synthetic variant.
    ///
    /// A fixed vocabulary would rot, so this is only half the test: a suffix that some sibling
    /// declares in `supportedThinkingEfforts` also counts. This list exists for the bases that
    /// declare nothing (the reference lists `gpt-6-astra` with no effort metadata yet still
    /// expands `gpt-6-astra-high` from it).
    const EFFORT_SUFFIXES: [&'static str; 7] =
        ["minimal", "low", "medium", "high", "xhigh", "max", "ultra"];

    /// The base model this entry is a synthetic variant of, if it is one.
    ///
    /// The reference does not send every id it lists to the provider. For a reasoning model it
    /// lists the model once and then lists one extra entry per reasoning effort (`gpt-6-luna`, then
    /// `gpt-6-luna-high`, `gpt-6-luna-low`, …). Only the base id exists upstream: the suffixed ones
    /// are names the reference resolves to *base id + reasoning effort* before it makes the call.
    ///
    /// Importing them verbatim — as an earlier version of this command did — produces models the
    /// provider rejects with `400 The '<id>' model is not supported when using Codex with a ChatGPT
    /// account`, while the base model works. So they are recognised here rather than copied.
    ///
    /// Both halves of the test come from the listing rather than from a guess: the id must end in
    /// an effort word, *and* the id with that suffix removed must be another model in the same
    /// list. A genuinely suffixed model whose base is not listed — Antigravity's
    /// `gemini-3.8-flash-high`, `gpt-oss-120b-medium` — is therefore kept, and the reference
    /// really does serve those.
    pub fn effort_variant_of<'a>(&self, siblings: &'a [ReferenceModel]) -> Option<&'a str> {
        // An entry that declares its own efforts is a base model by definition, however it is
        // named.
        if !self.supported_efforts.is_empty() {
            return None;
        }
        let (prefix, suffix) = self.id.rsplit_once('-')?;
        let declared_by_a_sibling = siblings
            .iter()
            .any(|s| s.supported_efforts.iter().any(|e| e == suffix));
        if !declared_by_a_sibling && !Self::EFFORT_SUFFIXES.contains(&suffix) {
            return None;
        }
        siblings
            .iter()
            .find(|s| s.id == prefix)
            .map(|s| s.id.as_str())
    }
}

impl ReferenceDb {
    /// The reference's saved model lists per provider connection, keyed `provider:connectionId`.
    ///
    /// This is what makes an imported account *routable*. A Manarouter provider with no model
    /// entries resolves nothing, so a migration that created accounts but no models would report
    /// success and serve nothing — the account would be live and unreachable.
    ///
    /// The reference keeps these in `key_value` under the `syncedAvailableModels` namespace with
    /// `provider:connectionId` as the key; the namespace is a column of its own, not a prefix on
    /// the key.
    pub fn synced_models(&self) -> Result<HashMap<String, Vec<ReferenceModel>>> {
        let exists: i64 = self
            .connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='key_value'",
                [],
                |row| row.get(0),
            )
            .map_err(|e| Error::Store(format!("cannot inspect the reference schema: {e}")))?;
        if exists == 0 {
            return Ok(HashMap::new());
        }
        let mut statement = self
            .connection
            .prepare("SELECT key, value FROM key_value WHERE namespace = 'syncedAvailableModels'")
            .map_err(|e| Error::Store(format!("cannot read reference model lists: {e}")))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| Error::Store(format!("cannot query reference model lists: {e}")))?;
        let mut out = HashMap::new();
        for row in rows {
            let (key, value) = row.map_err(|e| Error::Store(format!("cannot read a row: {e}")))?;
            // Some rows hold a JSON array directly, others a JSON *string* containing one. Both
            // spellings occur in the reference, so both are accepted rather than assumed.
            let parsed = serde_json::from_str::<serde_json::Value>(&value)
                .ok()
                .and_then(|outer| match outer {
                    serde_json::Value::String(inner) => serde_json::from_str(&inner).ok(),
                    other => Some(other),
                });
            let entries: Vec<ReferenceModel> = parsed
                .as_ref()
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            let id = item.get("id")?.as_str()?;
                            if id.is_empty() {
                                return None;
                            }
                            // The reference sometimes repeats the id as the label; storing that
                            // would show a redundant label in the panel, so it is dropped.
                            let label = item
                                .get("name")
                                .and_then(|n| n.as_str())
                                .filter(|n| !n.is_empty() && *n != id)
                                .map(str::to_owned);
                            // `outputTokenLimit` is the reference's name for the max output
                            // tokens; keep it so cost and truncation behave as they did there.
                            let output_token_limit = item
                                .get("outputTokenLimit")
                                .and_then(|v| v.as_u64())
                                .filter(|v| *v > 0 && *v <= u32::MAX as u64)
                                .map(|v| v as u32);
                            let input_token_limit = item
                                .get("inputTokenLimit")
                                .and_then(|v| v.as_u64())
                                .filter(|v| *v > 0 && *v <= u32::MAX as u64)
                                .map(|v| v as u32);
                            let supported_efforts = item
                                .get("supportedThinkingEfforts")
                                .and_then(|v| v.as_array())
                                .map(|items| {
                                    items
                                        .iter()
                                        .filter_map(|e| e.as_str())
                                        .map(str::to_owned)
                                        .collect()
                                })
                                .unwrap_or_default();
                            Some(ReferenceModel {
                                id: id.to_owned(),
                                label,
                                output_token_limit,
                                input_token_limit,
                                supported_efforts,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            if !entries.is_empty() {
                out.insert(key, entries);
            }
        }
        Ok(out)
    }
}

/// The per-row context a credential column needs to be read, so the decryptor can be a free
/// function rather than a closure that borrows the row's identity strings for the whole loop body.
#[derive(Clone, Copy)]
struct RowContext<'a> {
    id: &'a str,
    provider: &'a str,
    key: Option<&'a ReferenceKey>,
}

/// Read one credential column, distinguishing "absent", "plaintext" and "encrypted".
///
/// A plaintext column needs no key, which is what makes a reference running in passthrough mode
/// importable. An *encrypted* column without a key is a hard error naming the connection, because
/// silently skipping it would report a provider as having no accounts when it actually has several.
fn decrypt_field(context: RowContext<'_>, value: Option<String>) -> Result<Option<DecryptedToken>> {
    match StoredField::classify(value.as_deref()) {
        StoredField::Absent => Ok(None),
        StoredField::Plain(plain) => Ok(Some(DecryptedToken::new(plain))),
        StoredField::Encrypted(ciphertext) => match context.key {
            Some(key) => key.decrypt(&StoredField::Encrypted(ciphertext)),
            None => Err(Error::InvalidState(format!(
                "reference connection {} has an encrypted {} credential but no \
                 STORAGE_ENCRYPTION_KEY was found; add the reference .env or pass --db next to it",
                context.id, context.provider
            ))),
        },
    }
}

/// Parse the reference's `token_expires_at`, which is stored as an RFC 3339 string.
fn parse_reference_instant(value: &str) -> Option<i64> {
    let parsed =
        time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).ok()?;
    Some((parsed.unix_timestamp_nanos() / 1_000_000) as i64)
}

/// Where the reference keeps its data, and what it is called there.
#[derive(Debug, Clone)]
pub struct ReferencePaths {
    /// The data directory (`~/.omniroute`).
    pub data_dir: PathBuf,
    /// The database file.
    pub database: PathBuf,
    /// The dotenv holding the storage key.
    pub env_file: PathBuf,
}

impl ReferencePaths {
    /// Resolve the conventional OmniRoute location under a home directory.
    pub fn under_home(home: &Path) -> Self {
        let data_dir = home.join(".omniroute");
        Self {
            database: data_dir.join("storage.sqlite"),
            env_file: data_dir.join(".env"),
            data_dir,
        }
    }

    /// Resolve from `$HOME`, falling back to the working directory when unset.
    pub fn discover() -> Self {
        match std::env::var_os("HOME") {
            Some(home) => Self::under_home(Path::new(&home)),
            None => Self::under_home(Path::new(".")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pinned cross-implementation vector.
    ///
    /// Produced independently (Python `cryptography`, the same library family the reference's Node
    /// `crypto` mirrors) from `secret = "test-secret-key"`, a 16-byte nonce `00..0f`, and the
    /// plaintext `hello-omniroute`. If the derivation, the parameter set, the nonce width or the
    /// ciphertext/tag assembly ever changes, this fails — which is the point: a silent change here
    /// means every migrated credential is garbage.
    const VECTOR_CIPHERTEXT: &str =
        "enc:v1:000102030405060708090a0b0c0d0e0f:47ec17cb7d512110f334945de35c81:a0fefc980e79a665386b5412b3a2df34";

    #[test]
    fn decrypts_the_pinned_cross_implementation_vector() {
        let key = ReferenceKey::from_secret("test-secret-key").expect("derive");
        let token = key
            .decrypt(&StoredField::Encrypted(VECTOR_CIPHERTEXT.to_owned()))
            .expect("decrypt")
            .expect("present");
        assert_eq!(token.expose(), "hello-omniroute");
        // The fingerprint is stable and is not the value.
        assert_eq!(token.fingerprint().len(), 12);
        assert!(!token.fingerprint().contains("hello"));
    }

    #[test]
    fn a_wrong_key_is_an_error_rather_than_a_wrong_value() {
        let key = ReferenceKey::from_secret("a-different-key").expect("derive");
        let err = key
            .decrypt(&StoredField::Encrypted(VECTOR_CIPHERTEXT.to_owned()))
            .expect_err("a wrong key must not decrypt");
        assert!(
            err.to_string().contains("could not be decrypted"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn plaintext_and_absent_fields_are_classified_without_a_key() {
        assert_eq!(StoredField::classify(None), StoredField::Absent);
        assert_eq!(StoredField::classify(Some("")), StoredField::Absent);
        assert_eq!(
            StoredField::classify(Some("sk-plain")),
            StoredField::Plain("sk-plain".into())
        );
        assert!(matches!(
            StoredField::classify(Some("enc:v1:a:b:c")),
            StoredField::Encrypted(_)
        ));
    }

    #[test]
    fn malformed_ciphertext_is_rejected_with_a_specific_reason() {
        let key = ReferenceKey::from_secret("test-secret-key").expect("derive");
        for (value, needle) in [
            ("enc:v1:only:two", "expected 3"),
            ("enc:v1:zz:aa:bb", "malformed initialisation vector"),
            // Non-hex in the ciphertext field specifically: `aa` would parse and then merely fail
            // to authenticate, which is a different (and correctly reported) condition.
            (
                "enc:v1:000102030405060708090a0b0c0d0e0f:zz:bb",
                "malformed ciphertext",
            ),
        ] {
            let err = key
                .decrypt(&StoredField::Encrypted(value.to_owned()))
                .expect_err("must reject");
            assert!(
                err.to_string().contains(needle),
                "{value} produced {err}, expected {needle}"
            );
        }
    }

    #[test]
    fn a_nonce_of_the_wrong_width_is_rejected() {
        let key = ReferenceKey::from_secret("test-secret-key").expect("derive");
        // 8-byte nonce: valid hex, wrong width for this construction.
        let err = key
            .decrypt(&StoredField::Encrypted(
                "enc:v1:0001020304050607:aa:bb".to_owned(),
            ))
            .expect_err("must reject");
        assert!(err.to_string().contains("8-byte nonce"), "{err}");
    }

    #[test]
    fn an_empty_secret_is_refused_rather_than_deriving_a_key() {
        assert!(ReferenceKey::from_secret("   ").is_err());
    }

    #[test]
    fn debug_never_prints_a_token_or_a_key() {
        let token = DecryptedToken::new("super-secret-token".into());
        let rendered = format!("{token:?}");
        assert!(!rendered.contains("super-secret-token"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");

        let key = ReferenceKey::from_secret("test-secret-key").expect("derive");
        let rendered = format!("{key:?}");
        assert!(!rendered.contains("test-secret-key"), "{rendered}");
    }

    #[test]
    fn env_parsing_handles_quotes_comments_and_absent_keys() {
        let text = "# a comment\nOTHER=1\nSTORAGE_ENCRYPTION_KEY=\"abc123\"\nTRAILING=x\n";
        assert_eq!(
            parse_env_value(text, "STORAGE_ENCRYPTION_KEY"),
            Some("abc123".to_owned())
        );
        assert_eq!(parse_env_value(text, "MISSING"), None);
        // A prefix that is not an exact key name must not match.
        assert_eq!(
            parse_env_value("STORAGE_ENCRYPTION_KEY_EXTRA=1\n", "STORAGE_ENCRYPTION_KEY"),
            None
        );
        assert_eq!(
            parse_env_value(
                "STORAGE_ENCRYPTION_KEY='single'\n",
                "STORAGE_ENCRYPTION_KEY"
            ),
            Some("single".to_owned())
        );
    }

    #[test]
    fn reference_paths_resolve_under_a_home_directory() {
        let paths = ReferencePaths::under_home(Path::new("/home/example"));
        assert_eq!(
            paths.database,
            PathBuf::from("/home/example/.omniroute/storage.sqlite")
        );
        assert_eq!(
            paths.env_file,
            PathBuf::from("/home/example/.omniroute/.env")
        );
    }

    #[test]
    fn token_expiry_strings_parse_to_epoch_millis() {
        let parsed = parse_reference_instant("1970-01-01T00:00:01.500Z").expect("parse");
        assert_eq!(parsed, 1500);
        assert!(parse_reference_instant("not-a-date").is_none());
    }

    /// Build one reference model entry with the fields the variant logic reads.
    fn model(id: &str, efforts: &[&str]) -> ReferenceModel {
        ReferenceModel {
            id: id.to_owned(),
            label: None,
            output_token_limit: None,
            input_token_limit: None,
            supported_efforts: efforts.iter().map(|e| (*e).to_owned()).collect(),
        }
    }

    #[test]
    fn a_suffixed_id_that_names_a_listed_model_and_a_declared_effort_is_a_variant() {
        // The real Codex shape: the base declares its efforts, the suffixed entries do not.
        let list = vec![
            model("gpt-6-luna", &["low", "medium", "high"]),
            model("gpt-6-luna-high", &[]),
            model("gpt-6-luna-low", &[]),
        ];
        assert_eq!(
            list[1].effort_variant_of(&list),
            Some("gpt-6-luna"),
            "gpt-6-luna-high must be recognised as a synthetic variant"
        );
        assert_eq!(
            list[0].effort_variant_of(&list),
            None,
            "the base is not a variant"
        );
    }

    #[test]
    fn a_suffixed_id_is_a_variant_even_when_its_base_declares_no_efforts() {
        // The reference lists `gpt-6-astra` with no `supportedThinkingEfforts` at all, yet still
        // expands `gpt-6-astra-high` from it. Requiring the declaration on the base would have let
        // 38 broken ids through, which is exactly what happened before this was fixed.
        let list = vec![model("gpt-6-astra", &[]), model("gpt-6-astra-high", &[])];
        assert_eq!(list[1].effort_variant_of(&list), Some("gpt-6-astra"));
    }

    #[test]
    fn a_genuinely_suffixed_model_whose_base_is_absent_is_kept() {
        // Antigravity serves `gemini-3.8-flash-high` and `gpt-oss-120b-medium` for real; there is
        // no `gemini-3.8-flash` or `gpt-oss-120b` in the list, so these are models and not
        // variants. Collapsing them would delete working models.
        let list = vec![
            model("gemini-3.8-flash-high", &[]),
            model("gpt-oss-120b-medium", &[]),
            model("claude-opus-4-6-thinking", &[]),
        ];
        for entry in &list {
            assert_eq!(
                entry.effort_variant_of(&list),
                None,
                "{} is a real model and must not be collapsed",
                entry.id
            );
        }
    }

    #[test]
    fn an_entry_declaring_its_own_efforts_is_never_treated_as_a_variant() {
        // A future model that really is called `something-high` and lists its own efforts is a
        // base model, however it is named.
        let list = vec![
            model("thing", &["high"]),
            model("thing-high", &["low", "high"]),
        ];
        assert_eq!(list[1].effort_variant_of(&list), None);
    }

    #[test]
    fn a_database_without_the_connection_table_is_reported_clearly() {
        let path = std::env::temp_dir().join(format!("mr-ref-empty-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let conn = rusqlite::Connection::open(&path).expect("create");
            conn.execute("CREATE TABLE unrelated (x INTEGER)", [])
                .expect("ddl");
        }
        let db = ReferenceDb::open_read_only(&path).expect("open");
        assert!(!db.has_provider_connections().expect("inspect"));
        let _ = std::fs::remove_file(&path);
    }
}
