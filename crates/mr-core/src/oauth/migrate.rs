//! `mr migrate omniroute` — importing a reference OmniRoute installation.
//!
//! Two rules from `docs/17-migrating-from-omniroute.md` override convenience, and both are kept
//! here:
//!
//! 1. **The original database is never written to.** It belongs to a running application, it may
//!    be mid-write, and it may be locked. The migration copies it — including its `-wal` and
//!    `-shm` siblings — reads the copy, and deletes the copy.
//! 2. **Nothing is written until `--apply`.** Planning is a pure read; a dry run is the default.
//!
//! ## What changed about OAuth, and why
//!
//! The migration document originally said OAuth grants *cannot* be carried across, because a
//! grant is bound to the client that obtained it: the token endpoint rejects a refresh presented
//! by a different client with `invalid_grant`.
//!
//! That reasoning is correct in general and **wrong for the platforms this build implements**.
//! They speak as the *same public installed-application clients* the reference uses — OmniRoute
//! ships the identical Antigravity client id/secret and the identical Codex client id, because
//! both are taken from the vendors' own published clients. A refresh therefore succeeds. This was
//! not assumed; it was verified against a live reference database before this module existed
//! (see the pinned vector in [`super::reference`] and the notes in `docs/17`).
//!
//! So a grant is imported by **one refresh**: the reference's refresh token is handed to the
//! platform's own [`OAuthPlatform::refresh_with_context`], and whatever comes back is stored as a
//! native credential. A stored *access* token is never copied directly — those are short-lived,
//! and for Codex already rotated.
//!
//! ## Why a platform whose client identity differs is still refused
//!
//! `cline`, `openference`, `kiro` and the device-flow providers are not implemented as Manarouter
//! platforms at all, or use a client identity this build does not speak. Importing their tokens
//! would create an account that looks configured and fails on first use, which the document is
//! right to call the worst outcome. They are reported as **needing a fresh login**, not skipped
//! silently.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::catalog::catalog;
use crate::config::{config_dir, database_path, snapshot_path, Config};
use crate::dialect::AuthKind;
use crate::error::{Error, Result};
use crate::ids::{AccountId, Millis, OAuthCredentialId};
use crate::model::{Account, ModelEntry, ModelSource};
use crate::oauth::reference::{
    parse_env_value, ReferenceConnection, ReferenceDb, ReferenceKey, ReferenceModel, ReferencePaths,
};
use crate::oauth::{platform as oauth_platform, AccountIdentity, TokenSet};
use crate::secret::Vault;
use crate::snapshot::RoutingSnapshot;
use crate::store::{OAuthCredentialRecord, Store};

/// How a reference provider key maps onto a Manarouter OAuth platform.
///
/// Only platforms this build actually implements appear here. A reference connection whose
/// provider is absent from this table is reported, never guessed at — the provider ids are close
/// enough (`agy` vs `antigravity`) that a heuristic would eventually map one onto the wrong
/// client identity and produce a credential that cannot refresh.
pub const PROVIDER_MAP: &[(&str, &str)] = &[
    // Reference key -> Manarouter platform id (which is also the catalogue provider id).
    ("codex", "openai-codex"),
    ("agy", "antigravity"),
    ("antigravity", "antigravity"),
    ("github", "github-copilot"),
    ("github-copilot", "github-copilot"),
];

/// Reference providers that are deliberately never imported, with the reason shown to the
/// operator.
///
/// Kept explicit rather than implied: a provider that vanishes from a migration report without a
/// reason reads as a bug, which is exactly what `docs/17` warns about.
const REFUSED_WEB_SESSION: &[&str] = &[
    "claude-web",
    "deepseek-web",
    "gemini-web",
    "zai-web",
    "duckduckgo-web",
    "lmarena",
    "veoaifree-web",
];

/// The reference's placeholder for "this account still needs a Google Cloud project".
///
/// Copying it through would produce an Antigravity account whose every request fails with
/// "project metadata is missing or malformed", so it is treated as *not set*.
const PROJECT_PLACEHOLDER: &str = "__REQUIRES_GCP_PROJECT__";

/// What the migration decided to do with one reference connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferAction {
    /// The grant can be refreshed and imported.
    Import,
    /// An account with this identity already exists locally and will be left alone.
    AlreadyPresent,
    /// The platform is implemented but the stored grant is unusable (no refresh token, or a
    /// shape this build cannot drive). A fresh login is required.
    NeedsRelogin,
    /// The grant is readable but the account cannot serve requests until a Cloud Code project is
    /// onboarded. `--apply` attempts the onboarding; if it fails the account is not created.
    NeedsProject,
}

impl TransferAction {
    /// A short human word for the report.
    pub fn label(self) -> &'static str {
        match self {
            Self::Import => "import",
            Self::AlreadyPresent => "kept local",
            Self::NeedsRelogin => "needs a fresh login",
            Self::NeedsProject => "needs a Cloud Code project",
        }
    }
}

/// One connection the migration intends to carry across.
#[derive(Debug, Clone)]
pub struct PlannedTransfer {
    /// The reference's connection id, which seeds a stable local account id.
    pub reference_id: String,
    /// The reference provider key, as stored.
    pub reference_provider: String,
    /// The Manarouter OAuth platform id.
    pub platform: &'static str,
    /// The Manarouter catalogue provider id to attach the account to.
    pub provider_id: String,
    /// Display label (the reference email when present).
    pub label: String,
    /// Email or login, when the reference recorded one.
    pub email: Option<String>,
    /// Whether a refresh token is present, which is what makes the grant importable.
    pub has_refresh: bool,
    /// Printable fingerprint of the credential, so a dry run identifies it without exposing it.
    pub fingerprint: Option<String>,
    /// Whether the reference had this connection enabled.
    pub was_active: bool,
    /// The Cloud Code project id, for Antigravity connections.
    pub cloudcode_project: Option<String>,
    /// Why the account cannot be imported as-is, when it cannot. Shown to the operator, so it
    /// names the action that fixes it rather than restating the symptom.
    pub reason: Option<String>,
    /// What will happen on `--apply`.
    pub action: TransferAction,
}

/// A whole migration plan: what will be imported, and everything that will not, with reasons.
#[derive(Debug, Clone)]
pub struct MigrationPlan {
    /// The database the plan was read from.
    pub source: PathBuf,
    /// SHA-256 of the source's main file at read time (hex).
    pub source_hash: String,
    /// Whether the source changed while it was being read (expected when the reference runs).
    pub source_changed: bool,
    /// Connections to carry across.
    pub transfers: Vec<PlannedTransfer>,
    /// Providers refused by policy, with the reason.
    pub refused: Vec<(String, String)>,
    /// Providers skipped because this build cannot express them, with the reason.
    pub skipped: Vec<(String, String)>,
}

impl MigrationPlan {
    /// Transfers that `--apply` would attempt: the ones with a usable grant.
    ///
    /// `NeedsProject` is included because the apply path can still succeed for it — the platform's
    /// `complete_login` onboards a Cloud Code project. A dry run cannot know whether that network
    /// call will succeed, so it reports the account as needing a project rather than pretending it
    /// is ready or writing it off.
    pub fn importable(&self) -> Vec<&PlannedTransfer> {
        self.transfers
            .iter()
            .filter(|t| {
                matches!(
                    t.action,
                    TransferAction::Import | TransferAction::NeedsProject
                )
            })
            .collect()
    }
}

/// What one connection produced when applied.
#[derive(Debug, Clone)]
pub struct AppliedTransfer {
    /// The provider the account was attached to.
    pub provider_id: String,
    /// The local account id.
    pub account_id: String,
    /// The account label.
    pub label: String,
    /// Whether a new provider entry had to be created from the catalogue.
    pub provider_created: bool,
}

/// The result of an applied migration.
#[derive(Debug, Clone)]
pub struct MigrationReport {
    /// The plan that was executed.
    pub plan: MigrationPlan,
    /// Accounts written, in order.
    pub applied: Vec<AppliedTransfer>,
    /// Accounts whose credential was already present but whose model list gained entries on this
    /// run. Reported separately so an operator can see that a second run changed something
    /// without mistaking it for a re-import.
    pub models_updated: Vec<String>,
    /// Connections that failed, with the reason. One failure never aborts the rest.
    pub failed: Vec<(String, String)>,
    /// Where the configuration was written.
    pub config_path: PathBuf,
    /// Where the routing snapshot was written.
    pub snapshot_path: PathBuf,
}

impl fmt::Display for MigrationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} account(s) imported, {} failed",
            self.applied.len(),
            self.failed.len()
        )
    }
}

/// Options for a migration run.
#[derive(Debug, Clone, Default)]
pub struct MigrationOptions {
    /// Perform the import. Without it, nothing is written.
    pub apply: bool,
    /// The reference database to read. Defaults to the conventional location.
    pub database: Option<PathBuf>,
    /// The reference data directory, used to find the `.env` holding the storage key.
    pub data_dir: Option<PathBuf>,
    /// Restrict the run to these reference provider keys.
    pub only: Vec<String>,
}

/// The reference database, copied to a private temporary location.
pub struct ReferenceCopy {
    /// Directory holding the copy; removed when this value is dropped. Held only for its
    /// `Drop`, which is what deletes the copy at the end of the read phase.
    _dir: tempfile::TempDir,
    /// The main database file inside that directory.
    path: PathBuf,
    /// SHA-256 of the *original* main file at copy time.
    original_hash: String,
    /// The original path, kept so a re-hash can be attempted.
    original: PathBuf,
}

impl fmt::Debug for ReferenceCopy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReferenceCopy")
            .field("path", &self.path)
            .field("original", &self.original)
            .finish_non_exhaustive()
    }
}

impl ReferenceCopy {
    /// The copied database's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// SHA-256 of the original at copy time.
    pub fn original_hash(&self) -> &str {
        &self.original_hash
    }

    /// Re-hash the original and report whether it changed while the migration read it.
    ///
    /// A change is **reported, not fatal**. The reference is frequently running, and SQLite
    /// rewriting a page while a copy is taken will change the file; failing the migration for that
    /// would make it unusable in exactly the situation an operator is most likely to be in. The
    /// hash of the *copy* is what the import actually read, and that is what the report states.
    pub fn original_changed(&self) -> bool {
        match hash_file(&self.original) {
            Ok(current) => current != self.original_hash,
            // An unreadable original is not evidence of tampering.
            Err(_) => false,
        }
    }
}

/// Copy a reference database (and its WAL siblings) somewhere private and consistent.
///
/// The siblings matter: a live SQLite database in WAL mode keeps recent commits in `-wal`, so
/// copying only the main file silently loses the newest rows. The copy is opened read-write and
/// checkpointed so the WAL is folded in, leaving one self-contained file to read.
pub fn copy_reference_database(source: &Path) -> Result<ReferenceCopy> {
    if !source.exists() {
        return Err(Error::NotFound(format!(
            "no reference database at {}; pass --db with the correct path",
            source.display()
        )));
    }
    let original_hash = hash_file(source)?;
    let dir = tempfile::Builder::new()
        .prefix("mr-migrate-")
        .tempdir()
        .map_err(|e| Error::Store(format!("cannot create a temporary directory: {e}")))?;
    let target = dir.path().join("reference.sqlite");
    std::fs::copy(source, &target).map_err(|e| {
        Error::Store(format!(
            "cannot copy the reference database from {}: {e}",
            source.display()
        ))
    })?;
    // The WAL and shared-memory files are copied too, but their absence is entirely normal (a
    // cleanly closed database has neither).
    for suffix in ["-wal", "-shm"] {
        let from = sibling(source, suffix);
        if from.exists() {
            let to = sibling(&target, suffix);
            std::fs::copy(&from, &to)
                .map_err(|e| Error::Store(format!("cannot copy {}: {e}", from.display())))?;
        }
    }
    // Fold the WAL into the copy so subsequent reads see a single consistent file.
    {
        let connection = rusqlite::Connection::open(&target)
            .map_err(|e| Error::Store(format!("cannot open the copied reference database: {e}")))?;
        // A missing WAL makes a checkpoint a no-op rather than an error, and a busy database
        // simply keeps its WAL — either way the read below is still consistent.
        let _ = connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }
    Ok(ReferenceCopy {
        _dir: dir,
        path: target,
        original_hash,
        original: source.to_path_buf(),
    })
}

/// `path` with a suffix appended, without disturbing the extension.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.push_str(suffix);
    path.with_file_name(name)
}

/// SHA-256 of a file, hex-encoded.
pub fn hash_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)
        .map_err(|e| Error::Store(format!("cannot read {}: {e}", path.display())))?;
    let mut hasher = Sha256::new();
    // Streamed in fixed chunks: a reference database is legitimately hundreds of megabytes and
    // reading it into memory to hash it would be the largest allocation in the process.
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| Error::Store(format!("cannot read {}: {e}", path.display())))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Read the reference's storage-encryption key, if it has one.
///
/// Returns `Ok(None)` when the reference ran without encryption at all, which is a supported
/// configuration (its own "passthrough mode") rather than an error.
pub fn reference_key(paths: &ReferencePaths) -> Result<Option<ReferenceKey>> {
    if !paths.env_file.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&paths.env_file).map_err(|e| {
        Error::InvalidState(format!(
            "cannot read the reference .env at {}: {e}",
            paths.env_file.display()
        ))
    })?;
    match parse_env_value(&text, "STORAGE_ENCRYPTION_KEY") {
        Some(secret) => Ok(Some(ReferenceKey::from_secret(&secret)?)),
        None => Ok(None),
    }
}

/// The Manarouter platform a reference provider key maps to, if this build implements it.
pub fn platform_for(reference_provider: &str) -> Option<&'static str> {
    let key = reference_provider.trim().to_ascii_lowercase();
    PROVIDER_MAP
        .iter()
        .find(|(from, _)| *from == key)
        .map(|(_, to)| *to)
        .filter(|to| oauth_platform(to).is_some())
}

/// Build the "previous token set" a platform needs to refresh correctly.
///
/// This is where per-platform knowledge lives, and it is the difference between an import that
/// works and one that produces a plausible-looking account:
///
/// * **Codex** persists an `id_token`, and the relay reads `chatgpt-account-id` out of it. Without
///   carrying it across, every request is a hard upstream 400.
/// * **Copilot has no refresh grant.** Its durable credential is the long-lived *GitHub* token,
///   which the reference keeps in the `access_token` column; Manarouter expects it in
///   `refresh_token` and mints a session token from it.
/// * **Antigravity** needs its Cloud Code project id, or the relay's request envelope is
///   rejected. The reference stores it in `provider_specific_data`.
pub fn previous_tokens_for(platform_id: &str, connection: &ReferenceConnection) -> TokenSet {
    let mut tokens = TokenSet::new(
        connection
            .access_token
            .as_ref()
            .map(|t| t.expose().to_owned())
            .unwrap_or_default(),
    );
    tokens.refresh_token = connection
        .refresh_token
        .as_ref()
        .map(|t| t.expose().to_owned());
    if let Some(expires) = connection.expires_at_ms {
        tokens.expires_at = Millis::from_millis(expires);
    }
    match platform_id {
        "openai-codex" => {
            if let Some(id_token) = connection.id_token.as_ref() {
                tokens.extra.insert(
                    "id_token".into(),
                    serde_json::Value::String(id_token.expose().to_owned()),
                );
            }
        }
        "github-copilot" => {
            // The reference's GitHub token is the re-mintable half of the credential. Move it to
            // where Manarouter expects it: `refresh()` mints a Copilot session token from
            // `refresh_token` and never sends the GitHub token upstream itself.
            tokens.refresh_token = connection
                .access_token
                .as_ref()
                .map(|t| t.expose().to_owned())
                .or(tokens.refresh_token);
        }
        "antigravity" => {
            if let Some(project) = cloudcode_project_from(connection) {
                tokens.extra.insert(
                    "cloudcode_project_id".into(),
                    serde_json::Value::String(project),
                );
            }
            tokens.extra.insert(
                "oauth_tier".into(),
                serde_json::Value::String("antigravity-ide".into()),
            );
        }
        _ => {}
    }
    tokens
}

/// Pull a usable Cloud Code project id out of the reference's provider-specific data.
///
/// The reference uses the literal `__REQUIRES_GCP_PROJECT__` to mean "not set yet". Treating that
/// as a project id is the single most likely way to produce a configured account that cannot
/// serve a single request, so it is filtered here along with the shape the relay validates.
pub fn cloudcode_project_from(connection: &ReferenceConnection) -> Option<String> {
    let value = connection.provider_data.get("projectId")?.as_str()?;
    if value == PROJECT_PLACEHOLDER || value.is_empty() || value.len() > 256 {
        return None;
    }
    let usable = value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    usable.then(|| value.to_owned())
}

/// The credential half a refresh needs for this platform's grant shape.
///
/// Codex and Antigravity store a real refresh token. Copilot has no OAuth refresh grant at all:
/// the durable credential is the long-lived *GitHub* token, which the reference keeps in the
/// access-token column and Manarouter expects in `refresh_token` so it can mint a session token
/// from it. Getting this wrong is silent — the account imports and then fails every request — so
/// it is decided in one place that both the planner and the importer call.
fn durable_credential<'a>(
    platform_id: &str,
    connection: &'a ReferenceConnection,
) -> Option<&'a str> {
    match platform_id {
        "github-copilot" => connection
            .access_token
            .as_ref()
            .or(connection.refresh_token.as_ref())
            .map(|t| t.expose()),
        _ => connection.refresh_token.as_ref().map(|t| t.expose()),
    }
}

/// Decide what to do with one reference connection, without touching anything.
pub fn plan_connection(
    connection: &ReferenceConnection,
    existing_identities: &BTreeMap<String, BTreeMap<String, String>>,
) -> PlanDecision {
    let provider_key = connection.provider.trim().to_ascii_lowercase();

    // Policy first. These providers authenticate by browser cookie or by scraping a web session,
    // which this project refuses outright (`docs/21-security.md` §6) regardless of how the
    // reference happens to have typed the credential — the reference stores a session cookie in
    // the `api_key` column, so checking the auth kind first would report a refusal as "not
    // implemented" and hide a deliberate decision behind an apparent gap.
    if REFUSED_WEB_SESSION.contains(&provider_key.as_str()) {
        return PlanDecision::Skip(
            "web-session authentication is never supported (project policy)".to_owned(),
        );
    }

    if !connection.auth_type.eq_ignore_ascii_case("oauth") {
        // Reported as what it is. `openrouter` and `deepinfra` are well known to this build; they
        // are simply API-key providers, and saying "no such platform" would send an operator
        // looking for a bug that is not there.
        return PlanDecision::Skip(
            "API-key connection; API-key import is not implemented yet".to_owned(),
        );
    }

    let Some(platform_id) = platform_for(&connection.provider) else {
        return PlanDecision::Skip(format!(
            "this build has no {} OAuth platform (not ported, or a different client identity)",
            connection.provider
        ));
    };

    // An account is only importable if something durable can be refreshed. For Copilot that
    // durable half is the GitHub token in the access column.
    if durable_credential(platform_id, connection).is_none() {
        return PlanDecision::Transfer(TransferAction::NeedsRelogin);
    }

    // Antigravity cannot serve a single request without a Cloud Code project: the relay injects it
    // into the request envelope, and the reference's `__REQUIRES_GCP_PROJECT__` placeholder is not
    // a project. Importing such an account would produce exactly the failure this migration exists
    // to avoid — configured, enabled, and guaranteed to fail — so it is reported instead. The
    // apply path tries to onboard one; this is the honest pre-flight verdict.
    if platform_id == "antigravity" && cloudcode_project_from(connection).is_none() {
        return PlanDecision::Transfer(TransferAction::NeedsProject);
    }

    // A GitHub token the vendor will not mint a Copilot session from cannot become a working
    // account, and every attempt is a wasted authenticated request. The distinction is visible in
    // the token itself, with no network call: `gho_` is an OAuth App token (what this build's own
    // device flow produces and Copilot accepts), while `ghu_` is a GitHub **App** user token —
    // which the mint endpoint rejects with `403 programmatic_token_generation, approved clients
    // only`. Checking the prefix is therefore a real pre-flight test rather than a baked-in
    // assumption about the vendor, and a reference holding a `gho_` token still imports.
    if platform_id == "github-copilot" {
        let token = durable_credential(platform_id, connection).unwrap_or_default();
        if !token.starts_with("gho_") {
            return PlanDecision::Transfer(TransferAction::NeedsRelogin);
        }
    }

    let identity = connection
        .email
        .clone()
        .unwrap_or_else(|| connection.id.clone());
    if existing_identities
        .get(platform_id)
        .is_some_and(|map| map.contains_key(&identity))
    {
        return PlanDecision::Transfer(TransferAction::AlreadyPresent);
    }

    PlanDecision::Transfer(TransferAction::Import)
}

/// The planner's verdict for one connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanDecision {
    /// Carry it across (or leave it alone, per the action).
    Transfer(TransferAction),
    /// Do not carry it across, for this reason.
    Skip(String),
}

/// A per-platform note explaining what an unimportable grant needs, or `None` when it is ready.
///
/// These strings are user-facing and deliberately name the action rather than the symptom.
fn blocker_reason(platform_id: &str, connection: &ReferenceConnection) -> Option<String> {
    match platform_id {
        // GitHub refuses to mint a Copilot session from a token issued to a client it no longer
        // approves: the reference's GitHub token is a `ghu_` App token, and the mint endpoint
        // answers 403 `programmatic_token_generation`. Nothing this migration can do changes that,
        // so it says so instead of importing an account that 403s on every request.
        "github-copilot"
            if durable_credential("github-copilot", connection)
                .is_some_and(|t| !t.starts_with("gho_")) =>
        {
            Some(
                "GitHub will not mint a Copilot session from this token (it is a GitHub App \
                 `ghu_` token, and the mint endpoint accepts approved clients only); log in again \
                 through the panel"
                    .to_owned(),
            )
        }
        // Antigravity needs a Cloud Code project in the request envelope. The reference's
        // placeholder is not one, so the grant must be refreshed and onboarded — which `--apply`
        // attempts.
        "antigravity" if cloudcode_project_from(connection).is_none() => Some(
            "no Cloud Code project recorded; --apply will try to onboard one, and the account is \
             only created if that succeeds"
                .to_owned(),
        ),
        _ => None,
    }
}

/// Build a migration plan from a reference database. Pure: reads only.
pub fn plan_migration(
    database: &Path,
    db: &ReferenceDb,
    key: Option<&ReferenceKey>,
    existing: &Config,
    only: &[String],
) -> Result<MigrationPlan> {
    let connections = db.connections(key)?;
    let mut existing_identities: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for provider in &existing.providers {
        let AuthKind::OAuth { platform } = &provider.auth else {
            continue;
        };
        let entry = existing_identities.entry(platform.clone()).or_default();
        for account in &provider.accounts {
            if let Some(identity) = account.identity.as_deref() {
                entry.insert(identity.to_owned(), account.id.as_str().to_owned());
            }
            if let Some(label) = account.label.as_deref() {
                entry.insert(label.to_owned(), account.id.as_str().to_owned());
            }
        }
    }

    let filter: Vec<String> = only.iter().map(|s| s.to_ascii_lowercase()).collect();
    let mut transfers = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    for connection in &connections {
        if !filter.is_empty() && !filter.contains(&connection.provider.to_ascii_lowercase()) {
            continue;
        }
        match plan_connection(connection, &existing_identities) {
            PlanDecision::Transfer(action) => {
                let platform_id = platform_for(&connection.provider)
                    .expect("a Transfer decision implies a resolved platform");
                transfers.push(PlannedTransfer {
                    reference_id: connection.id.clone(),
                    reference_provider: connection.provider.clone(),
                    platform: platform_id,
                    provider_id: platform_id.to_owned(),
                    label: connection
                        .email
                        .clone()
                        .unwrap_or_else(|| format!("{}/{}", connection.provider, connection.id)),
                    email: connection.email.clone(),
                    // "Refreshable" means a durable credential exists *for this platform's grant
                    // shape*, which for Copilot is the GitHub token in the access column — not
                    // literally a refresh-token column, which Copilot never has.
                    has_refresh: durable_credential(platform_id, connection).is_some(),
                    fingerprint: connection.credential_fingerprint(),
                    was_active: connection.is_active,
                    cloudcode_project: cloudcode_project_from(connection),
                    reason: blocker_reason(platform_id, connection),
                    action,
                });
            }
            PlanDecision::Skip(reason) => {
                // One row per provider, not per connection: an operator needs to know *which*
                // provider did not come across and why, not to read the same sentence six times.
                if !skipped.iter().any(|(p, _)| p == &connection.provider) {
                    skipped.push((connection.provider.clone(), reason));
                }
            }
        }
    }

    let refused: Vec<(String, String)> = skipped
        .iter()
        .filter(|(p, _)| REFUSED_WEB_SESSION.contains(&p.as_str()))
        .map(|(p, r)| (p.clone(), r.clone()))
        .collect();
    let skipped: Vec<(String, String)> = skipped
        .into_iter()
        .filter(|(p, _)| !REFUSED_WEB_SESSION.contains(&p.as_str()))
        .collect();

    Ok(MigrationPlan {
        source: database.to_path_buf(),
        source_hash: String::new(),
        source_changed: false,
        transfers,
        refused,
        skipped,
    })
}

/// Run a migration end to end.
///
/// When `options.apply` is false this only reads and plans, and is safe to run against a live
/// reference instance.
pub async fn run_migration(options: &MigrationOptions) -> Result<MigrationReport> {
    let paths = match &options.data_dir {
        Some(dir) => ReferencePaths {
            data_dir: dir.clone(),
            database: dir.join("storage.sqlite"),
            env_file: dir.join(".env"),
        },
        None => ReferencePaths::discover(),
    };
    let database = options
        .database
        .clone()
        .unwrap_or_else(|| paths.database.clone());
    let env_file = if database == paths.database {
        paths.env_file.clone()
    } else {
        // A `--db` pointing elsewhere still wants a key; look next to the database first.
        database
            .parent()
            .map(|p| p.join(".env"))
            .unwrap_or_else(|| paths.env_file.clone())
    };
    let key = reference_key(&ReferencePaths {
        data_dir: paths.data_dir.clone(),
        database: database.clone(),
        env_file,
    })?;

    let copy = copy_reference_database(&database)?;
    let db = ReferenceDb::open_read_only(copy.path())?;
    if !db.has_provider_connections()? {
        return Err(Error::InvalidState(format!(
            "{} is not a reference database: it has no provider_connections table",
            database.display()
        )));
    }

    let mut config = Config::load_default()?;
    let mut plan = plan_migration(&database, &db, key.as_ref(), &config, &options.only)?;
    plan.source_hash = copy.original_hash().to_owned();
    plan.source_changed = copy.original_changed();
    // The copy is dropped here: nothing below needs the reference database, and its removal is the
    // documented end of the read phase.
    drop(db);
    drop(copy);

    let config_path = crate::config::config_path();
    let snapshot_out = snapshot_path();
    if !options.apply {
        return Ok(MigrationReport {
            plan,
            applied: Vec::new(),
            models_updated: Vec::new(),
            failed: Vec::new(),
            config_path,
            snapshot_path: snapshot_out,
        });
    }

    let mut vault = Vault::open_default()?;
    let store = Store::open(&database_path())?;
    let mut applied = Vec::new();
    let mut models_updated = Vec::new();
    let mut failed = Vec::new();

    // Re-read the connections from a fresh copy so `--apply` uses exactly what it planned.
    let copy = copy_reference_database(&database)?;
    let db = ReferenceDb::open_read_only(copy.path())?;
    let connections = db.connections(key.as_ref())?;
    // The reference's saved model lists, keyed `provider:connectionId`. Carried across because a
    // Manarouter provider with no model entries resolves nothing: without this the migration would
    // report success and produce accounts that cannot be reached.
    let synced_models = db.synced_models()?;

    for transfer in plan.transfers.clone() {
        if !matches!(
            transfer.action,
            TransferAction::Import | TransferAction::NeedsProject | TransferAction::AlreadyPresent
        ) {
            continue;
        }
        let Some(connection) = connections.iter().find(|c| c.id == transfer.reference_id) else {
            failed.push((
                transfer.reference_id.clone(),
                "the connection disappeared from the reference between planning and import"
                    .to_owned(),
            ));
            continue;
        };
        // Model lists are merged for every account this run knows about, including one whose
        // credential is already present. Otherwise a second run — which is exactly what an
        // operator does after upgrading, or after the first run's model import was added — would
        // skip the account and leave the provider unreachable.
        let mut missing_models = false;
        if let Some(ids) = synced_models.get(&format!("{}:{}", connection.provider, connection.id))
        {
            missing_models = merge_provider_models(&mut config, &transfer.provider_id, ids);
        }

        // `AlreadyPresent` means the credential is already installed, so there is nothing to
        // refresh and nothing to write — but the model merge above may still have changed the
        // configuration, and an operator must be told that happened rather than seeing a silent
        // "kept local".
        if transfer.action == TransferAction::AlreadyPresent {
            if missing_models {
                models_updated.push(
                    transfer
                        .email
                        .clone()
                        .unwrap_or_else(|| transfer.label.clone()),
                );
            }
            continue;
        }

        match import_one(
            &mut config,
            &mut vault,
            &store,
            connection,
            &transfer,
            &synced_models,
            &mut applied,
        )
        .await
        {
            Ok(()) => {}
            Err(e) => failed.push((transfer.reference_id.clone(), e.to_string())),
        }
    }

    // Persist only when something actually changed, so a run that found everything already in
    // place does not rewrite the configuration and bump its mtime for no reason.
    if !applied.is_empty() || !models_updated.is_empty() {
        persist(&mut config, &config_path, &snapshot_out)?;
    }
    vault.flush()?;

    Ok(MigrationReport {
        plan,
        applied,
        models_updated,
        failed,
        config_path,
        snapshot_path: snapshot_out,
    })
}

/// Refresh one reference grant and install it as a native account.
#[allow(clippy::too_many_arguments)]
async fn import_one(
    config: &mut Config,
    vault: &mut Vault,
    store: &Store,
    connection: &ReferenceConnection,
    transfer: &PlannedTransfer,
    synced_models: &std::collections::HashMap<String, Vec<ReferenceModel>>,
    applied: &mut Vec<AppliedTransfer>,
) -> Result<()> {
    let platform = oauth_platform(transfer.platform).ok_or_else(|| {
        Error::InvalidState(format!(
            "the {} platform is not available in this build",
            transfer.platform
        ))
    })?;

    let previous = previous_tokens_for(transfer.platform, connection);
    let refresh_token = durable_credential(transfer.platform, connection)
        .map(str::to_owned)
        .ok_or_else(|| {
            Error::InvalidState("no refreshable credential is present in the reference row".into())
        })?;

    // The one network call: exchange the reference's credential for a token set that is genuinely
    // this installation's. Nothing is written unless this succeeds.
    let mut tokens = platform
        .refresh_with_context(&refresh_token, &previous)
        .await
        .map_err(|e| {
            Error::InvalidState(format!(
                "refresh failed, so nothing was imported for this account: {e}"
            ))
        })?;

    // Antigravity cannot serve a request without a Cloud Code project, and the reference may
    // record the placeholder instead of one. `complete_login` onboards one; when it cannot, the
    // account is refused rather than written, because a project-less Antigravity account fails
    // every request and would look like a Manarouter bug.
    if transfer.platform == "antigravity"
        && tokens.extra.get("cloudcode_project_id").is_none()
        && transfer.cloudcode_project.is_none()
    {
        platform.complete_login(&mut tokens).await.map_err(|e| {
            Error::InvalidState(format!(
                "this Antigravity grant has no Cloud Code project and onboarding one failed \
                 ({e}); log in through the panel to attach a project"
            ))
        })?;
    }
    // A project recorded by the reference is trusted over a freshly onboarded one only when
    // onboarding produced nothing — onboarding is authoritative when it runs.
    if transfer.platform == "antigravity" && tokens.extra.get("cloudcode_project_id").is_none() {
        if let Some(project) = transfer.cloudcode_project.as_deref() {
            tokens.extra.insert(
                "cloudcode_project_id".into(),
                serde_json::Value::String(project.to_owned()),
            );
        }
    }
    if transfer.platform == "antigravity" && tokens.extra.get("cloudcode_project_id").is_none() {
        return Err(Error::InvalidState(
            "no Cloud Code project is available for this Antigravity account; it would fail every \
             request, so nothing was imported"
                .into(),
        ));
    }

    // Identity comes from the platform when it can be derived without a network call, and falls
    // back to what the reference recorded. The fallback matters: an Antigravity identity lookup is
    // a live Google call, and a migration should not depend on one per account.
    let identity = match platform.account_identity(&tokens).await {
        Ok(identity) => identity,
        Err(_) => AccountIdentity {
            id: transfer.reference_id.clone(),
            label: transfer.label.clone(),
            email: transfer.email.clone(),
            plan: None,
        },
    };

    // Create the provider from the shipped catalogue if the operator has not added it yet, so a
    // migration onto a fresh install works in one step.
    let mut provider_created = false;
    if !config
        .providers
        .iter()
        .any(|p| p.id.as_str() == transfer.provider_id)
    {
        let template = catalog().get(&transfer.provider_id).ok_or_else(|| {
            Error::NotFound(format!(
                "no catalogue entry for {}; add the provider by hand first",
                transfer.provider_id
            ))
        })?;
        config.providers.push(template.to_provider());
        provider_created = true;
    }

    let oauth_ref = OAuthCredentialId::generate();
    let vault_ref = format!(
        "provider/{}/oauth/{}",
        transfer.provider_id,
        oauth_ref.as_str()
    );

    // Vault first: a database row pointing at a missing credential is a broken account, whereas a
    // credential with no row is invisible and harmless.
    let serialized = serde_json::to_string(&tokens)
        .map_err(|e| Error::InvalidState(format!("tokens could not be encoded: {e}")))?;
    vault.put(&vault_ref, &serialized)?;

    let mut record = OAuthCredentialRecord::new(transfer.platform, vault_ref.clone())
        .with_label(identity.label.clone())
        .with_scopes(tokens.scopes.clone());
    record.id = oauth_ref.clone();
    if tokens.expires_at.0 > 0 {
        record.expires_at = Some(tokens.expires_at);
    }
    if let Err(e) = store.put_oauth_credential(&record, Millis::now()) {
        let _ = vault.delete(&vault_ref);
        return Err(Error::Store(format!(
            "the credential metadata could not be saved: {e}"
        )));
    }

    let account_id = stable_account_id(&transfer.provider_id, &transfer.reference_id);

    // Carry the reference's model list across, so the account is reachable. Done through the same
    // helper the `AlreadyPresent` path uses, so a first and a second run behave identically, and
    // before the provider is borrowed below because the merge also writes `config.aliases`.
    if let Some(ids) = synced_models.get(&format!("{}:{}", connection.provider, connection.id)) {
        merge_provider_models(config, &transfer.provider_id, ids);
    }

    let provider = config
        .providers
        .iter_mut()
        .find(|p| p.id.as_str() == transfer.provider_id)
        .ok_or_else(|| Error::InvalidState("the provider vanished during import".into()))?;

    // Idempotency: a second run replaces the credential in place rather than adding a duplicate
    // account, which is what makes `mr migrate omniroute --apply` safe to repeat.
    let mut replaced = false;
    for account in provider.accounts.iter_mut() {
        if account.id == account_id {
            account.oauth_ref = Some(oauth_ref.clone());
            account.label = Some(identity.label.clone());
            account.identity = identity.email.clone();
            // Respect the reference's own enablement: a connection the operator disabled there
            // (an exhausted or restricted account) must not arrive here enabled and start serving
            // traffic it is not entitled to.
            account.enabled = transfer.was_active;
            replaced = true;
            break;
        }
    }
    if !replaced {
        provider.accounts.push(Account {
            id: account_id.clone(),
            label: Some(identity.label.clone()),
            identity: identity.email.clone(),
            secret_ref: None,
            oauth_ref: Some(oauth_ref.clone()),
            priority: 0,
            weight: 0,
            enabled: transfer.was_active,
            max_concurrent: 0,
            models_allow: Vec::new(),
            models_deny: Vec::new(),
            quota: Vec::new(),
            extra: Default::default(),
        });
    }

    applied.push(AppliedTransfer {
        provider_id: transfer.provider_id.clone(),
        account_id: account_id.as_str().to_owned(),
        label: identity.label.clone(),
        provider_created,
    });
    Ok(())
}

/// Merge one connection's reference model list into a provider, resolving the provider and the
/// alias list out of the same `Config` so both can be written in one pass.
fn merge_provider_models(
    config: &mut Config,
    provider_id: &str,
    entries: &[ReferenceModel],
) -> bool {
    let Config {
        providers, aliases, ..
    } = config;
    let Some(provider) = providers.iter_mut().find(|p| p.id.as_str() == provider_id) else {
        return false;
    };
    merge_models(provider, entries, aliases)
}

/// Add the models the provider is missing, and fill in label and output limit on ones it has, and
/// report whether anything changed.
///
/// Ids the provider already has are never removed or renamed, and an entry that already carries a
/// label or limit keeps it: this fills gaps rather than replacing a curated list. Without the
/// enrichment step a provider populated by an earlier version of this command (which imported ids
/// only) would keep its bare ids forever, because the id is already present and would be skipped.
/// New entries are marked [`ModelSource::Manual`] on purpose — the reference supplied them, and a
/// later `mr models sync` (which this build has no OAuth catalogue endpoint for) must not be
/// treated as authoritative over them.
fn merge_models(
    provider: &mut crate::model::Provider,
    entries: &[ReferenceModel],
    aliases: &mut Vec<crate::model::Alias>,
) -> bool {
    let mut changed = false;
    for entry in entries {
        // A synthetic effort variant is not a model the provider knows. It is recorded as an
        // alias onto the real model instead, so a client that has been calling the suffixed name
        // keeps resolving, while the id sent upstream is one the provider accepts.
        if let Some(base) = entry.effort_variant_of(entries) {
            // The target is the bare model id: `Config::validate` accepts an alias target that is
            // a reachable model id, and the qualified `provider/model` spelling is not one.
            let target = base.to_owned();
            // A model with this id may already be present, because an earlier version of this
            // command imported the variants verbatim. It cannot be called — the provider rejects
            // the id with a hard 400 — so it is retired in favour of the alias rather than left
            // beside it as a model that always fails. `Synced` entries are left alone: those came
            // from a real catalogue sync, not from this migration.
            let before = provider.models.len();
            provider
                .models
                .retain(|m| !(m.id == entry.id && m.source == ModelSource::Manual));
            if provider.models.len() != before {
                changed = true;
            }
            // Two spellings, because callers use both. The reference's own usage history shows
            // the bare name, while this project's documented convention is `provider/model`;
            // adding only one would make the migration look broken to half its callers.
            // The *variant's* two spellings, both pointing at the base model. Writing the base's
            // qualified name here instead would alias a model to itself.
            for from in [entry.id.clone(), format!("{}/{}", provider.id, entry.id)] {
                if aliases.iter().any(|a| a.from.eq_ignore_ascii_case(&from)) {
                    continue;
                }
                aliases.push(crate::model::Alias {
                    from,
                    to: target.clone(),
                    // Marked generated: it came from a naming convention in the reference rather
                    // than from an operator's intent, and the panel can say so.
                    generated: true,
                });
                changed = true;
            }
            continue;
        }
        match provider.models.iter_mut().find(|m| m.id == entry.id) {
            Some(existing) => {
                if existing.label.is_none() {
                    if let Some(label) = &entry.label {
                        existing.label = Some(label.clone());
                        changed = true;
                    }
                }
                if existing.max_output_tokens.is_none() {
                    if let Some(limit) = entry.output_token_limit {
                        existing.max_output_tokens = Some(limit);
                        changed = true;
                    }
                }
                if existing.context_length.is_none() {
                    if let Some(limit) = entry.input_token_limit {
                        existing.context_length = Some(limit);
                        changed = true;
                    }
                }
            }
            None => {
                provider.models.push(ModelEntry {
                    id: entry.id.clone(),
                    source: ModelSource::Manual,
                    label: entry.label.clone(),
                    // The reference's own figures, so context and truncation limits match what the
                    // account served before the migration rather than falling back to a guess.
                    context_length: entry.input_token_limit,
                    max_output_tokens: entry.output_token_limit,
                    capabilities: Vec::new(),
                    input_price_per_mtok: None,
                    output_price_per_mtok: None,
                    synced_at: None,
                    hidden: false,
                });
                changed = true;
            }
        }
    }
    changed
}

/// A deterministic account id, so re-running a migration updates one account instead of creating
/// a second.
///
/// Derived from a hash of the reference identity rather than from the reference's own UUID: the
/// reference id can be regenerated when a connection is re-imported there, and an account id that
/// changes would accumulate duplicates on every run.
pub fn stable_account_id(provider_id: &str, reference_id: &str) -> AccountId {
    let mut hasher = Sha256::new();
    hasher.update(b"manarouter-migrate-omniroute-account-v1");
    hasher.update(provider_id.as_bytes());
    hasher.update([0]);
    hasher.update(reference_id.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    AccountId::from_raw(format!("mig-{hex}"))
}

/// Validate, compile the routing snapshot, then write both — the same order `Ctx::save` uses, and
/// for the same reason: a configuration the router cannot compile must never reach disk.
fn persist(config: &mut Config, config_path: &Path, snapshot_out: &Path) -> Result<()> {
    let problems: Vec<String> = config
        .validate()
        .into_iter()
        .filter(|p| !p.contains("has no credential attached"))
        .collect();
    if !problems.is_empty() {
        return Err(Error::Config(format!(
            "the imported configuration is not valid, so nothing was written: {}",
            problems.join("; ")
        )));
    }
    let snapshot = RoutingSnapshot::compile(config)?;
    config.save(config_path)?;
    snapshot.write_to(snapshot_out)?;
    Ok(())
}

/// The config directory this run will write to, for the report.
pub fn target_config_dir() -> PathBuf {
    config_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection(provider: &str, email: Option<&str>) -> ReferenceConnection {
        ReferenceConnection {
            id: "ref-1".into(),
            provider: provider.into(),
            auth_type: "oauth".into(),
            is_active: true,
            email: email.map(str::to_owned),
            access_token: Some(crate::oauth::reference::DecryptedToken::new(
                "access".into(),
            )),
            refresh_token: Some(crate::oauth::reference::DecryptedToken::new(
                "refresh".into(),
            )),
            id_token: None,
            api_key: None,
            expires_at_ms: None,
            provider_data: serde_json::Value::Null,
        }
    }

    /// Build one reference model entry.
    fn rmodel(id: &str, efforts: &[&str]) -> ReferenceModel {
        ReferenceModel {
            id: id.to_owned(),
            label: None,
            output_token_limit: None,
            input_token_limit: None,
            supported_efforts: efforts.iter().map(|e| (*e).to_owned()).collect(),
        }
    }

    #[test]
    fn a_synthetic_effort_variant_becomes_an_alias_instead_of_a_model() {
        // The reference lists `gpt-6-luna` and then `gpt-6-luna-high`; only the base is a real
        // upstream id, so the suffixed one must not be imported as a model.
        let entries = vec![
            rmodel("gpt-6-luna", &["low", "high"]),
            rmodel("gpt-6-luna-high", &[]),
        ];
        let mut provider = crate::model::Provider::new(
            "openai-codex",
            crate::Dialect::OpenAi,
            "https://example.test",
        );
        let mut aliases = Vec::new();
        assert!(merge_models(&mut provider, &entries, &mut aliases));

        let ids: Vec<&str> = provider.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["gpt-6-luna"], "only the real model is imported");

        // Both spellings a caller might use resolve, and both point at the real model.
        let from: Vec<&str> = aliases.iter().map(|a| a.from.as_str()).collect();
        assert!(from.contains(&"gpt-6-luna-high"), "{from:?}");
        assert!(from.contains(&"openai-codex/gpt-6-luna-high"), "{from:?}");
        for a in &aliases {
            assert_eq!(a.to, "gpt-6-luna");
            assert!(a.generated);
        }
        // A model is never aliased to itself: `Config::validate` would accept it, but it would
        // mean `openai-codex/gpt-6-luna` had been mistaken for a variant.
        assert!(aliases.iter().all(|a| a.from != a.to));
    }

    #[test]
    fn a_model_id_already_imported_as_a_model_is_retired_when_it_turns_out_to_be_a_variant() {
        // An earlier version of this command imported the variant verbatim. The id is rejected
        // upstream with a hard 400, so leaving it beside the alias would leave a model that always
        // fails; re-running the migration must clean it up.
        let entries = vec![
            rmodel("gpt-6-luna", &["low", "high"]),
            rmodel("gpt-6-luna-high", &[]),
        ];
        let mut provider = crate::model::Provider::new(
            "openai-codex",
            crate::Dialect::OpenAi,
            "https://example.test",
        );
        provider.models.push(ModelEntry {
            id: "gpt-6-luna-high".into(),
            source: ModelSource::Manual,
            label: None,
            context_length: None,
            max_output_tokens: None,
            capabilities: Vec::new(),
            input_price_per_mtok: None,
            output_price_per_mtok: None,
            synced_at: None,
            hidden: false,
        });
        let mut aliases = Vec::new();
        assert!(merge_models(&mut provider, &entries, &mut aliases));
        let ids: Vec<&str> = provider.models.iter().map(|m| m.id.as_str()).collect();
        assert!(
            !ids.contains(&"gpt-6-luna-high"),
            "the unusable id must be retired, got {ids:?}"
        );
    }

    #[test]
    fn a_real_suffixed_model_is_imported_as_a_model_not_an_alias() {
        // Antigravity really does serve `gemini-3.8-flash-high` and `gpt-oss-120b-medium`.
        let entries = vec![
            rmodel("gemini-3.8-flash-high", &[]),
            rmodel("gpt-oss-120b-medium", &[]),
        ];
        let mut provider = crate::model::Provider::new(
            "antigravity",
            crate::Dialect::OpenAi,
            "https://example.test",
        );
        let mut aliases = Vec::new();
        merge_models(&mut provider, &entries, &mut aliases);
        let ids: Vec<&str> = provider.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["gemini-3.8-flash-high", "gpt-oss-120b-medium"]);
        assert!(
            aliases.is_empty(),
            "no alias should be invented: {aliases:?}"
        );
    }

    #[test]
    fn a_context_and_output_limit_from_the_reference_are_carried_across() {
        let mut entries = vec![rmodel("gpt-6-luna", &["low"])];
        entries[0].input_token_limit = Some(872_000);
        entries[0].output_token_limit = Some(128_000);
        entries[0].label = Some("GPT-6-Luna".into());
        let mut provider = crate::model::Provider::new(
            "openai-codex",
            crate::Dialect::OpenAi,
            "https://example.test",
        );
        let mut aliases = Vec::new();
        merge_models(&mut provider, &entries, &mut aliases);
        let m = &provider.models[0];
        assert_eq!(m.context_length, Some(872_000));
        assert_eq!(m.max_output_tokens, Some(128_000));
        assert_eq!(m.label.as_deref(), Some("GPT-6-Luna"));
    }

    #[test]
    fn merging_twice_changes_nothing_the_second_time() {
        let entries = vec![
            rmodel("gpt-6-luna", &["low", "high"]),
            rmodel("gpt-6-luna-high", &[]),
        ];
        let mut provider = crate::model::Provider::new(
            "openai-codex",
            crate::Dialect::OpenAi,
            "https://example.test",
        );
        let mut aliases = Vec::new();
        assert!(merge_models(&mut provider, &entries, &mut aliases));
        assert!(
            !merge_models(&mut provider, &entries, &mut aliases),
            "a second run must report no change, or every run would rewrite the configuration"
        );
        assert_eq!(aliases.len(), 2, "and must not duplicate the aliases");
    }

    #[test]
    fn the_platform_map_is_the_one_this_build_implements() {
        assert_eq!(platform_for("codex"), Some("openai-codex"));
        assert_eq!(platform_for("agy"), Some("antigravity"));
        assert_eq!(platform_for("github"), Some("github-copilot"));
        // Case and whitespace tolerate what a hand-edited database may contain.
        assert_eq!(platform_for(" CODEX "), Some("openai-codex"));
        // A platform this build does not implement resolves to nothing rather than to a guess.
        assert_eq!(platform_for("cline"), None);
        assert_eq!(platform_for("kiro"), None);
        assert_eq!(platform_for("openference"), None);
    }

    #[test]
    fn every_mapped_platform_actually_exists_in_the_registry() {
        for (from, to) in PROVIDER_MAP {
            assert!(
                oauth_platform(to).is_some(),
                "{from} maps to {to}, which is not a registered platform"
            );
            assert!(
                catalog().get(to).is_some(),
                "{from} maps to {to}, which has no catalogue entry to create a provider from"
            );
        }
    }

    #[test]
    fn copilot_moves_the_github_token_where_a_refresh_can_mint_from_it() {
        // The reference keeps the long-lived GitHub token in `access_token` and has no separate
        // refresh token; Manarouter mints from `refresh_token`.
        let mut c = connection("github", None);
        c.refresh_token = None;
        let tokens = previous_tokens_for("github-copilot", &c);
        assert_eq!(tokens.refresh_token.as_deref(), Some("access"));
    }

    #[test]
    fn codex_carries_its_id_token_across() {
        let mut c = connection("codex", Some("a@b.c"));
        c.id_token = Some(crate::oauth::reference::DecryptedToken::new(
            "header.payload.sig".into(),
        ));
        let tokens = previous_tokens_for("openai-codex", &c);
        assert_eq!(
            tokens.extra.get("id_token").and_then(|v| v.as_str()),
            Some("header.payload.sig")
        );
    }

    #[test]
    fn antigravity_carries_its_cloud_code_project() {
        let mut c = connection("agy", Some("a@b.c"));
        c.provider_data = serde_json::json!({"projectId": "aicode-consumers"});
        let tokens = previous_tokens_for("antigravity", &c);
        assert_eq!(
            tokens
                .extra
                .get("cloudcode_project_id")
                .and_then(|v| v.as_str()),
            Some("aicode-consumers")
        );
    }

    #[test]
    fn the_project_placeholder_is_never_treated_as_a_project() {
        let mut c = connection("agy", None);
        c.provider_data = serde_json::json!({"projectId": PROJECT_PLACEHOLDER});
        assert_eq!(cloudcode_project_from(&c), None);
        // And a malformed id is refused rather than injected into a request envelope.
        c.provider_data = serde_json::json!({"projectId": "has space"});
        assert_eq!(cloudcode_project_from(&c), None);
        c.provider_data = serde_json::json!({"projectId": "x".repeat(300)});
        assert_eq!(cloudcode_project_from(&c), None);
    }

    #[test]
    fn a_web_session_provider_is_refused_with_the_policy_reason() {
        let decision = plan_connection(&connection("claude-web", None), &BTreeMap::new());
        match decision {
            PlanDecision::Skip(reason) => assert!(reason.contains("never supported"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn an_unimplemented_provider_is_skipped_with_a_reason_naming_this_build() {
        let decision = plan_connection(&connection("kiro", None), &BTreeMap::new());
        match decision {
            PlanDecision::Skip(reason) => assert!(reason.contains("this build"), "{reason}"),
            other => panic!("expected a skip, got {other:?}"),
        }
    }

    #[test]
    fn a_connection_with_no_durable_credential_needs_a_fresh_login() {
        let mut c = connection("codex", Some("a@b.c"));
        c.refresh_token = None;
        assert_eq!(
            plan_connection(&c, &BTreeMap::new()),
            PlanDecision::Transfer(TransferAction::NeedsRelogin)
        );
    }

    #[test]
    fn a_github_app_token_is_refused_before_a_doomed_mint_call() {
        // `ghu_` is a GitHub App user token; Copilot's mint endpoint answers 403 for it. The
        // planner must not import an account that fails every request.
        let mut c = connection("github", None);
        c.refresh_token = None;
        c.access_token = Some(crate::oauth::reference::DecryptedToken::new(
            "ghu_01234567890123456789012345678901234567".into(),
        ));
        assert_eq!(
            plan_connection(&c, &BTreeMap::new()),
            PlanDecision::Transfer(TransferAction::NeedsRelogin)
        );
        // …while a real OAuth App token still imports, because the test is the prefix and not a
        // blanket refusal of the platform.
        c.access_token = Some(crate::oauth::reference::DecryptedToken::new(
            "gho_01234567890123456789012345678901234567".into(),
        ));
        assert_eq!(
            plan_connection(&c, &BTreeMap::new()),
            PlanDecision::Transfer(TransferAction::Import)
        );
    }

    #[test]
    fn the_copilot_blocker_explains_what_to_do_instead() {
        let mut c = connection("github", None);
        c.refresh_token = None;
        c.access_token = Some(crate::oauth::reference::DecryptedToken::new("ghu_x".into()));
        let reason = blocker_reason("github-copilot", &c).expect("a blocker");
        assert!(reason.contains("log in again"), "{reason}");
    }

    #[test]
    fn an_antigravity_account_without_a_project_is_flagged_not_silently_imported() {
        let mut c = connection("agy", Some("a@b.c"));
        c.provider_data = serde_json::json!({"projectId": PROJECT_PLACEHOLDER});
        assert_eq!(
            plan_connection(&c, &BTreeMap::new()),
            PlanDecision::Transfer(TransferAction::NeedsProject)
        );
        let reason = blocker_reason("antigravity", &c).expect("a blocker");
        assert!(reason.contains("onboard"), "{reason}");
        // With a real project it is a plain import and carries no caveat.
        c.provider_data = serde_json::json!({"projectId": "aicode-consumers"});
        assert_eq!(
            plan_connection(&c, &BTreeMap::new()),
            PlanDecision::Transfer(TransferAction::Import)
        );
        assert!(blocker_reason("antigravity", &c).is_none());
    }

    #[test]
    fn a_web_session_provider_is_refused_even_though_it_does_not_look_like_oauth() {
        // The reference stores these as `apikey`, so an auth-kind check placed first would report
        // a deliberate policy refusal as a missing platform.
        let mut c = connection("gemini-web", None);
        c.auth_type = "apikey".into();
        match plan_connection(&c, &BTreeMap::new()) {
            PlanDecision::Skip(reason) => assert!(reason.contains("never supported"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn an_api_key_provider_is_reported_as_such_not_as_a_missing_platform() {
        let mut c = connection("openrouter", None);
        c.auth_type = "apikey".into();
        match plan_connection(&c, &BTreeMap::new()) {
            PlanDecision::Skip(reason) => assert!(reason.contains("API-key"), "{reason}"),
            other => panic!("expected an API-key skip, got {other:?}"),
        }
    }

    #[test]
    fn an_existing_identity_is_left_alone_rather_than_duplicated() {
        let mut existing = BTreeMap::new();
        existing
            .entry("openai-codex".to_owned())
            .or_insert_with(BTreeMap::new)
            .insert("a@b.c".to_owned(), "local-1".to_owned());
        assert_eq!(
            plan_connection(&connection("codex", Some("a@b.c")), &existing),
            PlanDecision::Transfer(TransferAction::AlreadyPresent)
        );
        // A different identity on the same platform is still imported.
        assert_eq!(
            plan_connection(&connection("codex", Some("other@b.c")), &existing),
            PlanDecision::Transfer(TransferAction::Import)
        );
    }

    #[test]
    fn account_ids_are_deterministic_and_provider_scoped() {
        let a = stable_account_id("openai-codex", "conn-1");
        let b = stable_account_id("openai-codex", "conn-1");
        let c = stable_account_id("antigravity", "conn-1");
        assert_eq!(a, b, "the same input must produce the same id across runs");
        assert_ne!(a, c, "two platforms must not collide on one reference id");
        assert!(a.as_str().starts_with("mig-"));
    }

    #[test]
    fn a_copy_reads_a_database_without_touching_the_original() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("storage.sqlite");
        {
            let conn = rusqlite::Connection::open(&source).expect("create");
            conn.execute(
                "CREATE TABLE provider_connections (id TEXT, provider TEXT, auth_type TEXT, \
                 is_active INTEGER, email TEXT, access_token TEXT, refresh_token TEXT, \
                 id_token TEXT, api_key TEXT, token_expires_at TEXT, provider_specific_data TEXT)",
                [],
            )
            .expect("ddl");
            conn.execute(
                "INSERT INTO provider_connections VALUES \
                 ('c1','codex','oauth',1,'a@b.c','enc:v1:aa:bb:cc','enc:v1:aa:bb:cc',NULL,NULL,NULL,NULL)",
                [],
            )
            .expect("insert");
        }
        let before = hash_file(&source).expect("hash");
        let copy = copy_reference_database(&source).expect("copy");
        assert!(copy.path().exists());
        assert_eq!(copy.original_hash(), before);
        assert!(!copy.original_changed());
        let db = ReferenceDb::open_read_only(copy.path()).expect("open copy");
        assert!(db.has_provider_connections().expect("inspect"));
        // The original is byte-identical after the copy.
        assert_eq!(hash_file(&source).expect("rehash"), before);
    }

    #[test]
    fn a_missing_reference_database_names_the_path_it_looked_for() {
        let missing = std::env::temp_dir().join("mr-does-not-exist-reference.sqlite");
        let err = copy_reference_database(&missing).expect_err("must fail");
        assert!(
            err.to_string().contains("no reference database at"),
            "{err}"
        );
    }

    #[test]
    fn a_hash_change_is_reported_rather_than_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("storage.sqlite");
        std::fs::write(&source, b"first").expect("write");
        let copy = copy_reference_database(&source).expect("copy");
        assert!(!copy.original_changed());
        // Simulate the reference committing a page while we read.
        std::fs::write(&source, b"second").expect("rewrite");
        assert!(copy.original_changed());
    }
}
