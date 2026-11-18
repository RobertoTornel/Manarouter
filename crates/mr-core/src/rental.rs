//! The two rental entities: a platform **account**, and a deployment — one model on one GPU.
//!
//! ## Why two entities and not one
//!
//! Account creation is separate from model setup: a [`RentalAccount`] is a workspace credential
//! with no model attached, and choosing the credential and choosing the model are two different
//! acts, made at two different times. The types keep them apart.
//!
//! The relation runs in both directions, and neither side is capped. One account may host several
//! deployments, and one model-on-a-GPU may be recorded under several accounts: each pairing is its
//! own provider with its own id, its own URL and its own bill, so the same checkpoint can be run
//! twice on two accounts (a paid one and a free one, say) without either record overwriting the
//! other. The account owns the platform credential and the vault entry; each deployment names the
//! account it bills through, so usage stays attributable per provider while the credential is
//! entered once. Creating an account and setting up a model are therefore two separate acts, at
//! two separate times, and a second model on the same account does not need a second workspace
//! credential.
//!
//! ## HuggingFace tokens are their own entity
//!
//! A HuggingFace token is not part of an account. It authenticates a *repository fetch*, not a
//! rental: the same token may be needed by deployments on five different accounts, and one account
//! may host a public model that needs no token at all. It therefore lives in
//! [`crate::config::Config::huggingface_tokens`], with its own id and its own vault entry
//! ([`hf_token_vault_id`]), and a [`RentalDeployment`] names the token it uses with
//! [`RentalDeployment::hf_token`]. The relation is many-to-many for the same reason the account one
//! is: one token serves many deployments, and a deployment that needs none records none. Nothing
//! here is fixed to a particular count — three models on three cards, six accounts and two tokens
//! is an ordinary configuration, and so is one of each.
//!
//! ## Why an account is not a [`crate::model::Provider`]
//!
//! A provider requires a `base_url`. A Modal account has none: each app gets its own
//! `https://<workspace>--<app>-<fn>.modal.run` URL, and there is no workspace-level base URL to
//! put in the field. An account modelled as a provider would therefore have to carry a lie in a
//! mandatory field. [`crate::config::Config::rental_accounts`] is the honest home instead, and it
//! is the seam the later GPU-rental issues plug into.
//!
//! ## Why a deployment *is* a provider
//!
//! The inverse reasoning applies. A deployment — one model, reachable at a URL, with a KV-token
//! budget, a per-second price and a cold-start tolerance — is exactly what
//! [`crate::model::Provider`] already is, and the relay already enforces those budget fields on
//! the request path. Re-modelling it would duplicate all of that. So a deployment is **stored in
//! `Provider.extra["rental"]`** and parsed into [`RentalDeployment`] on demand.
//!
//! That choice is not merely convenient, it is required: `RoutingSnapshot` builds its own
//! `ProviderPlan` struct, so `Provider.extra` is not part of the compiled snapshot layout and
//! adding keys to it cannot shift a byte in the file the relay reads. A new `Provider` field
//! would force a [`crate::SNAPSHOT_VERSION`] bump for no runtime benefit, because the relay never
//! reads a GPU name. See `docs/research/ARCHITECTURE-gpu-rentals.md` §3.2. **[facts]**
//!
//! ## Storage is flexible; the code that reads it is not
//!
//! `Provider.extra` is an untyped map, so it will happily hold `"gpu": 42` or a `rental` object
//! with the GPU name misspelled. [`RentalDeployment::from_provider_extra`] therefore returns a
//! [`Result`] and a malformed value is a validation *error* the panel can report against the
//! offending field — never a silent default that quietly rents the wrong card.
//!
//! ## Scheduling concurrent GPU launches
//!
//! A deployment may request more than one GPU, and operators need to bring up deployments
//! sequentially (one card alive at a time) or in batches.  [`RentalSchedule`] expresses the
//! launch policy — sequential or batched with a ceiling — and [`plan_launch`] turns a list of
//! deployments into waves respecting that ceiling.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::gpu::arch::ArchFacts;
use crate::gpu::sizing::SizingResult;

/// The key a [`RentalDeployment`] is stored under inside `Provider.extra`.
pub const RENTAL_EXTRA_KEY: &str = "rental";

/// What distinguishes two deployments of the same repository on the same account.
///
/// One account may host the same checkpoint several times — a streaming endpoint on one card and
/// a batch endpoint on another is the ordinary case — so the identity of a deployment is the
/// account, the repository, the mode and the card. Everything derived from that identity (the
/// provider id, the `modal.App` name, the endpoint URL) carries the same suffix, so two of them can
/// never overwrite each other's record or deploy over each other's app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeploymentShape<'a> {
    /// The account the deployment bills through.
    pub account_id: &'a str,
    /// The HuggingFace repository.
    pub repo: &'a str,
    /// `streaming` or `batch`.
    pub mode: RentalMode,
    /// Catalog GPU id, e.g. `RTX-PRO-6000`.
    pub gpu: &'a str,
    /// How many of that card.
    pub gpu_count: u32,
}

impl<'a> DeploymentShape<'a> {
    /// The identity from the parts a deployment already stores.
    pub fn of(deployment: &'a RentalDeployment) -> Self {
        Self {
            account_id: &deployment.account,
            repo: &deployment.repo,
            mode: deployment.mode,
            gpu: &deployment.gpu,
            gpu_count: deployment.gpu_count,
        }
    }

    /// The trailing `-<mode>-<card>[-x<n>]` half of every derived name.
    fn suffix(&self) -> String {
        let mut suffix = format!("-{}", self.mode.as_str());
        let card = slugify(self.gpu);
        if !card.is_empty() {
            suffix.push('-');
            suffix.push_str(&card);
        }
        if self.gpu_count > 1 {
            suffix.push_str(&format!("-x{}", self.gpu_count));
        }
        suffix
    }
}

/// The `modal.App` name a deployment's generated bundle uses.
///
/// Derived from the deployment's identity, so the same deployment always gets the same app name —
/// and therefore the same URL — while two deployments that differ in mode or card get different
/// ones. Shared with the panel model, which mirrors this function, so the URL the panel previews is
/// the URL the bundle is deployed under.
pub fn modal_app_name(shape: DeploymentShape<'_>) -> String {
    format!("manarouter-{}", app_slug(shape))
}

/// The prefix [`modal_app_name`] puts in front of the slug.
pub const APP_NAME_PREFIX: &str = "manarouter-";

/// Modal's own limit: a name must be shorter than 64 characters. The margin is deliberate — a
/// name of exactly 64 is rejected, and the limit is measured on the finished app name, not the
/// slug. A long account and repository once produced a 79-character name and `modal deploy`
/// refused it.
pub const APP_NAME_MAX: usize = 63;

/// The stable part of the app name: `rental-<account>-<repo-name>-<mode>-<card>`, short enough
/// that [`modal_app_name`] stays inside [`APP_NAME_MAX`].
///
/// Provider ids use this shape too (`Provider::id` is built from it in the conductor), so one
/// string identifies both the provider and the Modal app. Truncation eats the account/repository
/// half, never the suffix: the suffix is the only part that makes two deployments distinguishable.
pub fn app_slug(shape: DeploymentShape<'_>) -> String {
    let name = shape.repo.rsplit('/').next().unwrap_or(shape.repo);
    let head = format!("rental-{}-{}", slugify(shape.account_id), slugify(name));
    let suffix = shape.suffix();
    let room = APP_NAME_MAX.saturating_sub(APP_NAME_PREFIX.len() + suffix.len());
    let mut slug = head;
    if slug.len() > room {
        slug.truncate(room);
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug.push_str(&suffix);
    slug
}

/// The endpoint URL Modal is expected to publish for this deployment.
///
/// Modal's shape is `https://<workspace>--<app-name>-<function-name>.modal.run`, with dots in the
/// app name replaced by dashes. The generated bundle exposes one web function named `serve`, so
/// that is the function half.
///
/// `None` when a part is missing: an account with no recorded workspace, or a name that would be
/// empty. The caller must then ask for an explicit URL rather than invent one.
// UNCERTAIN: the URL shape and the `serve` function half are reverse-engineered from Modal's
// documented app naming; `modal deploy` prints the real URL, which is authoritative if it differs.
pub fn modal_base_url(workspace: &str, shape: DeploymentShape<'_>) -> Option<String> {
    let workspace = slugify(workspace);
    let account = slugify(shape.account_id);
    let name = slugify(shape.repo.rsplit('/').next().unwrap_or(shape.repo));
    let card = slugify(shape.gpu);
    if workspace.is_empty() || account.is_empty() || name.is_empty() || card.is_empty() {
        return None;
    }
    Some(format!(
        "https://{workspace}--{}-serve.modal.run",
        modal_app_name(shape).replace('.', "-")
    ))
}

/// Lowercase ASCII slug: runs of anything else become one dash, ends trimmed.
fn slugify(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_owned()
}

/// Which platform a rental account bills against.
///
/// The known platforms are variants rather than plain strings so the panel can offer them, but the
/// set is deliberately open: a cheap GPU host appears every few months, and refusing to record one
/// until this crate ships a new release would make the feature useless for exactly the operators
/// it is for. [`RentalPlatform::Custom`] carries the platform's own identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RentalPlatform {
    /// Modal — the platform this feature was researched against.
    Modal,
    /// Kaggle notebooks.
    Kaggle,
    /// Any other platform, by its own slug.
    #[serde(untagged)]
    Custom(String),
}

impl RentalPlatform {
    /// The stable identifier written to config and shown in the panel.
    pub fn as_str(&self) -> String {
        match self {
            Self::Modal => "modal".to_string(),
            Self::Kaggle => "kaggle".to_string(),
            Self::Custom(name) => format!("custom:{name}"),
        }
    }

    /// Parse a platform identifier, accepting the `custom:<x>` spelling written by [`Self::as_str`].
    pub fn parse(s: &str) -> Self {
        let trimmed = s.trim();
        let lower = trimmed.to_ascii_lowercase();
        match lower.as_str() {
            "modal" => Self::Modal,
            "kaggle" => Self::Kaggle,
            _ => match lower.strip_prefix("custom:") {
                Some(rest) => Self::Custom(rest.to_string()),
                None => Self::Custom(trimmed.to_string()),
            },
        }
    }

    /// Every platform this build knows by name, for a panel's dropdown.
    pub const KNOWN: [&'static str; 2] = ["modal", "kaggle"];
}

/// What an account is expected to cost.
///
/// [`RentalPlan::Unknown`] is a **real state and must not be rendered as `Free`**. The distinction
/// is the whole reason requirement 10's `(Paid)` marker can be trusted: a `(Paid)` marker is only
/// truthful when the plan is known *and* the GPU is known to be gated. A UI that collapses
/// `unknown` into either answer is guessing with the operator's money.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RentalPlan {
    /// A documented free tier.
    Free,
    /// A metered plan.
    Paid,
    /// Not known. The default, because it is the truthful answer before anyone checks.
    #[default]
    Unknown,
}

impl RentalPlan {
    /// The stable identifier written to config.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Paid => "paid",
            Self::Unknown => "unknown",
        }
    }

    /// Parse a plan identifier.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "free" => Some(Self::Free),
            "paid" => Some(Self::Paid),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// A `"free" | "paid" | "unknown"` triple, serialised as one of those three words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FreePlan {
    /// Documented as usable on a free plan.
    Available,
    /// Documented as not usable on a free plan.
    Unavailable,
    /// No documentation either way. The default, and the honest answer for almost every card.
    #[default]
    Unknown,
}

impl FreePlan {
    /// The stable identifier shown in the panel and carried in `GET /api/gpu/catalog`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Unavailable => "unavailable",
            Self::Unknown => "unknown",
        }
    }
}

/// How well a catalog fact is established.
///
/// Three values, not two, and the middle one is not decoration. A number the operator measured on
/// their own hardware is not the same kind of claim as a number read off a vendor's pricing page,
/// and neither is the same as a number nobody has checked. The panel is allowed to display all
/// three *without re-deriving them*, which only works if the number arrives with its own
/// confidence attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FactSource {
    /// Verified against primary documentation.
    Documented,
    /// Measured by the operator.
    Observed,
    /// Recorded but not verified. Never presented as fact.
    Unverified,
}

impl FactSource {
    /// The stable identifier written into the catalog.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Documented => "documented",
            Self::Observed => "observed",
            Self::Unverified => "unverified",
        }
    }
}

/// A platform credential for renting GPUs, with no model attached.
///
/// Secrets are never stored here. `secret_ref` is a **vault entry id** ([`crate::secret`]) pointing
/// at the platform's one combined credential string. A config file is meant to be readable and
/// diffable; a credential in it is not.
///
/// [`RentalAccount::hf_secret_ref`] is kept only to read accounts written before HuggingFace
/// tokens became their own entity; new code records a [`HuggingFaceToken`] and a deployment names
/// it. Nothing writes the account field any more.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RentalAccount {
    /// Slug, operator-visible, unique among accounts.
    pub id: String,
    /// Human label, for example "Mana on Modal".
    #[serde(default)]
    pub label: String,
    /// Which platform bills for this credential.
    #[serde(default = "default_platform")]
    pub platform: RentalPlatform,
    /// What the credential is expected to cost.
    ///
    /// Defaults to [`RentalPlan::Unknown`] rather than `Free`: an account whose plan nobody has
    /// recorded yet must not read as free, because that is the reading that produces a surprise
    /// invoice.
    #[serde(default)]
    pub plan: RentalPlan,
    /// The Modal workspace the account's apps are deployed into, for example `mana`.
    ///
    /// Not a secret, and not a credential: it is the half of the app URL that Manarouter can know
    /// in advance. Optional because accounts written before the field existed, and accounts on a
    /// platform with no workspace concept, must still load — a deployment built on such an account
    /// then needs an explicit `base_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Vault entry id holding the platform's one combined credential string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<String>,
    /// Vault entry id holding the Modal API token pair (`ak-<id>.as-<secret>`), when the account
    /// also carries one.
    ///
    /// This is a *second* credential, separate from the proxy token pair in [`Self::secret_ref`]:
    /// the proxy pair authenticates the relay's inference requests, while the API token pair is
    /// what the Modal CLI / API uses. Both may coexist on one account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_secret_ref: Option<String>,
    /// **Legacy.** Vault entry id of a HuggingFace token recorded on the account before tokens
    /// became independent.
    ///
    /// New accounts do not set this: a token is a [`HuggingFaceToken`] with its own id, and a
    /// deployment names the one it uses. Kept so an older config keeps probing gated repositories;
    /// it is never written and never required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hf_secret_ref: Option<String>,
    /// The platform-side ceiling on concurrent containers.
    ///
    /// Modal Starter documents **10 GPU concurrency** [facts]; other platforms document nothing, so
    /// this is `None` rather than a guess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_containers: Option<u32>,
    /// Operator note, shown in the panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Fields this binary does not know about, preserved verbatim on round-trip.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn default_platform() -> RentalPlatform {
    RentalPlatform::Modal
}

impl RentalAccount {
    /// An account with the given id, everything else defaulted.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: String::new(),
            platform: RentalPlatform::Modal,
            plan: RentalPlan::Unknown,
            workspace: None,
            secret_ref: None,
            api_secret_ref: None,
            hf_secret_ref: None,
            max_concurrent_containers: None,
            note: None,
            extra: Map::new(),
        }
    }

    /// Whether this account holds a credential reference at all.
    ///
    /// A GPU account with no credential cannot deploy anything; the panel says so before the
    /// operator fills in the rest of a wizard.
    pub fn has_credential(&self) -> bool {
        self.secret_ref
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
    }

    /// The single, non-negotiable rule: an account's plan is never inferred.
    ///
    /// `Unknown` stays `Unknown`. This function exists so that the rule has one home, rather than
    /// being re-decided (and eventually mis-decided) in the panel, the CLI and the API.
    pub fn effective_plan(&self) -> RentalPlan {
        self.plan
    }

    /// Whether a `(Paid)` marker may be drawn for this account at all.
    ///
    /// Requirement 10's marker is truthful only when the plan is known. Combined with the
    /// per-GPU [`FreePlan`], this is the whole rule: no marker from `Unknown` on either side.
    pub fn plan_is_known(&self) -> bool {
        self.plan != RentalPlan::Unknown
    }

    /// Reason the account is not usable yet, if any.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.id.trim().is_empty() {
            out.push("rental account id is empty".to_string());
        }
        if !self.has_credential() {
            out.push(format!(
                "rental account {} has no secret_ref: it cannot deploy anything",
                self.id
            ));
        }
        if let Some(n) = self.max_concurrent_containers {
            if n == 0 {
                out.push(format!(
                    "rental account {} allows zero concurrent containers",
                    self.id
                ));
            }
        }
        out
    }
}

/// How a deployment is meant to be used.
///
/// One choice, on the deployment's **one** card: either a live endpoint an operator can reach, or a
/// queue drained in bulk. There is no second card to choose, because a deployment is one model on
/// one GPU; the mode changes how that card is fed, not which card it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RentalMode {
    /// A live endpoint a request can reach while it waits.
    ///
    /// `interactive` is an alias, not a second variant: deployments written before the rename
    /// stored that spelling, and a stored config must keep loading after an upgrade.
    #[default]
    #[serde(alias = "interactive")]
    Streaming,
    /// A queue that is filled and then drained in bulk.
    Batch,
}

impl RentalMode {
    /// The stable identifier written to config.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Streaming => "streaming",
            Self::Batch => "batch",
        }
    }

    /// Parse a mode identifier.
    ///
    /// `interactive` is accepted as the legacy spelling of [`Self::Streaming`]: configs and API
    /// bodies written before the mode was renamed to describe one card still say it, and a value
    /// that used to mean "a live endpoint" must keep meaning exactly that. It is never emitted.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "streaming" | "interactive" => Some(Self::Streaming),
            "batch" => Some(Self::Batch),
            _ => None,
        }
    }
}

/// When a `batch` deployment stops waiting and sends the batch it has.
///
/// A batch endpoint collects requests until it has [`BatchShape::size`] of them. What it does when
/// the queue stays short is the operator's decision, because the two answers cost different money:
/// [`BatchFill::Full`] holds every request until the batch is complete (the cheapest per request,
/// and a request can wait a long time for the last seat), while [`BatchFill::Timeout`] sends a
/// half-full batch once [`BatchShape::timeout_secs`] elapse, which keeps latency bounded and pays
/// for the empty seats. Neither is a default the software may pick for the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BatchFill {
    /// Wait for `size` requests, however long that takes.
    #[default]
    Full,
    /// Send whatever has arrived once `timeout_secs` have passed.
    Timeout,
}

impl BatchFill {
    /// The stable identifier written to config.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Timeout => "timeout",
        }
    }

    /// Parse a fill identifier.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" | "wait" | "wait-until-full" => Some(Self::Full),
            "timeout" | "timed" | "timeout-partial" => Some(Self::Timeout),
            _ => None,
        }
    }
}

/// How a `batch` deployment fills one batch: how many requests it holds, and when it gives up
/// waiting for more.
///
/// Only meaningful on [`RentalMode::Batch`]; [`RentalDeployment::problems`] refuses it elsewhere
/// rather than storing a setting nothing reads. This is a *record of intent* — the generated bundle
/// is a plain vLLM server, and the dispatcher that actually groups requests is the serving layer's
/// job — so the shape is what the operator decided and what admission control sizes against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchShape {
    /// Requests per batch. The `N` in "batching N".
    pub size: u32,
    /// Whether the batch waits for `size`, or ships early on a timeout.
    #[serde(default)]
    pub fill: BatchFill,
    /// Seconds to wait before shipping a short batch. Required by [`BatchFill::Timeout`], refused
    /// by [`BatchFill::Full`] (where it would be a number nothing reads).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u32>,
}

impl BatchShape {
    /// A shape that waits for a full batch.
    pub const fn full(size: u32) -> Self {
        Self {
            size,
            fill: BatchFill::Full,
            timeout_secs: None,
        }
    }

    /// A shape that ships a short batch after `timeout_secs`.
    pub const fn with_timeout(size: u32, timeout_secs: u32) -> Self {
        Self {
            size,
            fill: BatchFill::Timeout,
            timeout_secs: Some(timeout_secs),
        }
    }

    /// Problems with this shape on its own, as `(field, message)` pairs.
    pub fn problems(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if self.size == 0 {
            out.push((
                "batch.size".to_string(),
                "a batch must hold at least one request".to_string(),
            ));
        }
        match self.fill {
            BatchFill::Full => {
                if self.timeout_secs.is_some() {
                    out.push((
                        "batch.timeout_secs".to_string(),
                        "a batch that waits until it is full has no timeout to record; set \
                         `batch.fill` to `timeout` to ship a short batch"
                            .to_string(),
                    ));
                }
            }
            BatchFill::Timeout => match self.timeout_secs {
                None | Some(0) => out.push((
                    "batch.timeout_secs".to_string(),
                    "a batch that ships early needs a positive `batch.timeout_secs`".to_string(),
                )),
                Some(secs) if secs > 3600 => out.push((
                    "batch.timeout_secs".to_string(),
                    format!(
                        "`batch.timeout_secs` is {secs}; a batch that waits over an hour is \
                             `batch.fill: full`"
                    ),
                )),
                Some(_) => {}
            },
        }
        out
    }
}

/// A recorded intent to re-quantise a larger checkpoint for a smaller card.
///
/// **Planned and recorded, not executed.** Requantisation needs a GPU, a calibration dataset and an
/// offline tool; a wizard cannot do it, and pretending otherwise would be the same class of lie as a
/// fake `modal deploy`. The plan itself lives in [`crate::gpu::requantize`], where the arithmetic,
/// the feasibility gates and the "never synthesise `--quantization`" rule are unit-tested; this
/// alias is what a deployment stores next to the deployment's own fields.
pub use crate::gpu::requantize::RequantizePlan;

/// A HuggingFace token, independent of every rental account.
///
/// A token authenticates a repository fetch, not a rental, so it is recorded once and named by the
/// deployments that need it. Several deployments on several accounts may name the same token, and
/// a deployment whose repository is public names none. [`Self::secret_ref`] is a **vault entry
/// id**; the token value itself is never stored in the config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HuggingFaceToken {
    /// Slug, operator-visible, unique among tokens.
    pub id: String,
    /// Human label, for example "Mana on the Hub".
    #[serde(default)]
    pub label: String,
    /// Vault entry id holding the token value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<String>,
    /// Operator note, shown in the panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Fields this binary does not know about, preserved verbatim on round-trip.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The vault id a [`HuggingFaceToken`] is written to.
///
/// Derived from the token id rather than accepted from a client, exactly as the platform
/// credential's id is derived from the account id, so the panel, the CLI and the relay cannot
/// disagree about where a token lives.
pub fn hf_token_vault_id(token_id: &str) -> String {
    format!("hf:{token_id}")
}

impl HuggingFaceToken {
    /// A token with the given id, everything else defaulted.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: String::new(),
            secret_ref: None,
            note: None,
            extra: Map::new(),
        }
    }

    /// Whether this token holds a vault reference at all.
    pub fn has_secret(&self) -> bool {
        self.secret_ref
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
    }

    /// Reason the token is not usable yet, if any.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.id.trim().is_empty() {
            out.push("HuggingFace token id is empty".to_string());
        }
        if !self.has_secret() {
            out.push(format!(
                "HuggingFace token {} has no secret_ref: it cannot authenticate a fetch",
                self.id
            ));
        }
        out
    }
}

/// One model on one GPU, as stored in `Provider.extra["rental"]`.
///
/// Field-for-field this is the JSON contract in `docs/research/ARCHITECTURE-gpu-rentals.md` §3.2.
/// Every optional field stays optional: a deployment is authored over several steps in a wizard,
/// and a half-filled deployment must be recordable (and resumable) rather than rejected because
/// the operator has not chosen a `scaledown_window` yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RentalDeployment {
    /// [`RentalAccount::id`] this deployment bills against.
    pub account: String,
    /// [`HuggingFaceToken::id`] this deployment uses when its repository is gated.
    ///
    /// `None` is the ordinary case for a public checkpoint. The token is *not* reached through the
    /// account: the same token may serve deployments on several accounts, and one account may host
    /// deployments that need different tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hf_token: Option<String>,
    /// HuggingFace repo id, for example `orcarouter/Qwen3.8-27B-Uncensored-FP8`.
    pub repo: String,
    /// Commit, tag or branch. `None` means the repo's default revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Catalog GPU id, e.g. `RTX-PRO-6000`. Hyphenated, as the platform spells it.
    pub gpu: String,
    /// How many of that card. Defaults to 1.
    #[serde(default = "default_gpu_count")]
    pub gpu_count: u32,
    /// Catalog quantisation id: `fp8`, `awq`, … or `none` for unquantised weights.
    pub weight_quant: String,
    /// vLLM `--kv-cache-dtype` value: `auto` | `fp8` | `fp8_e4m3` | `fp8_e5m2` | `bfloat16`.
    pub kv_cache_dtype: String,
    /// vLLM `--max-model-len`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_model_len: Option<u64>,
    /// vLLM `--max-num-seqs`: sequences per scheduling step, **not** simultaneous sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_num_seqs: Option<u32>,
    /// Modal `scaledown_window` in seconds.
    ///
    /// Recorded because it is the difference between paying for a boot once and paying for it every
    /// time the operator looks away. Modal documents a default of **60 s** and a range of
    /// **2–1200 s** [facts]; the catalog carries those bounds so the wizard can refuse a value the
    /// platform would reject at deploy time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown_cooldown_secs: Option<u32>,
    /// `streaming` for a live endpoint, `batch` for a queue drained in bulk.
    #[serde(default)]
    pub mode: RentalMode,
    /// How a `batch` deployment fills and ships a batch.
    ///
    /// `None` on a streaming deployment, and `None` on a batch deployment written before this field
    /// existed — an old record keeps loading unchanged rather than being given a batch size nobody
    /// chose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<BatchShape>,
    /// Identifier of the generated deployment bundle, once one has been produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    /// Recorded intent to requantise, when the chosen card cannot execute the repo's weights.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requantize: Option<RequantizePlan>,
    /// The architecture facts read from the repo's `config.json`, when a probe has run.
    ///
    /// Stored rather than re-fetched because it is the *evidence* for the sizing numbers next to
    /// it: a panel that shows 1.8 M KV tokens must be able to show which layer count and head dim
    /// produced that figure, and an operator re-reading the deployment a week later must not need
    /// the network to see it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub architecture: Option<ArchFacts>,
    /// The sizing result the budget fields were derived from, kept for the same reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sizing: Option<Box<SizingResult>>,
    /// Extra vLLM startup flags, one per command-line argument, passed through to the generated
    /// app's launch command (for example `--enable-prefix-caching`, `--gpu-memory-utilization`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub startup_flags: Vec<String>,
    /// Fields this binary does not know about, preserved verbatim on round-trip.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn default_gpu_count() -> u32 {
    1
}

impl RentalDeployment {
    /// The smallest coherent deployment: an account, a repo and a card.
    pub fn new(
        account: impl Into<String>,
        repo: impl Into<String>,
        gpu: impl Into<String>,
    ) -> Self {
        Self {
            account: account.into(),
            hf_token: None,
            repo: repo.into(),
            revision: None,
            gpu: gpu.into(),
            gpu_count: 1,
            weight_quant: "none".to_string(),
            kv_cache_dtype: "auto".to_string(),
            max_model_len: None,
            max_num_seqs: None,
            shutdown_cooldown_secs: None,
            mode: RentalMode::Streaming,
            batch: None,
            bundle_id: None,
            requantize: None,
            architecture: None,
            sizing: None,
            startup_flags: Vec::new(),
            extra: Map::new(),
        }
    }

    /// Whether more than one card is rented, which multiplies both VRAM and the hourly bill.
    pub fn is_multi_gpu(&self) -> bool {
        self.gpu_count > 1
    }

    /// The total VRAM of the container, in bytes, given the catalog.
    ///
    /// `None` when the GPU id is not in the catalog — an unknown card has no VRAM to multiply, and
    /// substituting a plausible one is how a sizing figure becomes fiction.
    pub fn total_vram_bytes(&self) -> Option<u64> {
        let gpu = crate::gpu::catalog::gpu_by_id(&self.gpu)?;
        let count = u64::from(self.gpu_count.max(1));
        gpu.vram_bytes?.checked_mul(count)
    }

    /// The container's billed rate in USD per hour, given the catalog.
    pub fn price_usd_per_hour(&self) -> Option<f64> {
        let gpu = crate::gpu::catalog::gpu_by_id(&self.gpu)?;
        let count = f64::from(self.gpu_count.max(1));
        Some(gpu.price_usd_per_hour * count)
    }

    /// Problems that make this deployment unusable, as leaf field names plus a sentence.
    ///
    /// The panel maps each entry back to the field named first, so the operator sees the problem on
    /// the input that caused it rather than in a banner.
    pub fn problems(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if self.account.trim().is_empty() {
            out.push((
                "account".to_string(),
                "a rental deployment must name the account that pays for it".to_string(),
            ));
        }
        if self.repo.trim().is_empty() {
            out.push((
                "repo".to_string(),
                "a rental deployment must name a HuggingFace repo".to_string(),
            ));
        }
        if crate::gpu::catalog::gpu_by_id(&self.gpu).is_none() {
            out.push((
                "gpu".to_string(),
                format!("unknown GPU id {:?}; see GET /api/gpu/catalog", self.gpu),
            ));
        }
        if self.gpu_count == 0 {
            out.push((
                "gpu_count".to_string(),
                "gpu_count must be at least 1".to_string(),
            ));
        }
        if crate::gpu::catalog::quant_method_by_id(&self.weight_quant).is_none() {
            out.push((
                "weight_quant".to_string(),
                format!(
                    "unknown quantisation {:?}; see the `quants` array in GET /api/gpu/catalog",
                    self.weight_quant
                ),
            ));
        }
        if crate::gpu::catalog::kv_cache_dtype_by_id(&self.kv_cache_dtype).is_none() {
            out.push((
                "kv_cache_dtype".to_string(),
                format!(
                    "unknown KV cache dtype {:?}; see the `kv_dtypes` array in GET /api/gpu/catalog",
                    self.kv_cache_dtype
                ),
            ));
        }
        if let Some(len) = self.max_model_len {
            if len == 0 {
                out.push((
                    "max_model_len".to_string(),
                    "max_model_len must be greater than zero".to_string(),
                ));
            }
        }
        if let Some(seqs) = self.max_num_seqs {
            if seqs == 0 {
                // vLLM refuses `--max-num-seqs 0` at argument validation
                // (`Input should be greater than or equal to 1`) and the container then never
                // binds its port, so the deployment pays for a boot that serves nothing.
                out.push((
                    "max_num_seqs".to_string(),
                    "max_num_seqs must be at least 1; omit it to let vLLM choose".to_string(),
                ));
            }
        }
        if let Some(cooldown) = self.shutdown_cooldown_secs {
            let range = crate::gpu::catalog::modal_scaledown_window();
            if cooldown < range.min_seconds || cooldown > range.max_seconds {
                out.push((
                    "shutdown_cooldown_secs".to_string(),
                    format!(
                        "scaledown_window must be between {} and {} seconds; Modal rejects \
                         anything outside that range",
                        range.min_seconds, range.max_seconds
                    ),
                ));
            }
        }
        match (self.mode, self.batch) {
            (RentalMode::Streaming, Some(_)) => out.push((
                "batch".to_string(),
                "a streaming deployment has no batch to fill; drop `batch`, or set `mode: batch`"
                    .to_string(),
            )),
            (RentalMode::Batch, None) => out.push((
                "batch".to_string(),
                "a batch deployment must record `batch.size` and whether it waits for a full \
                 batch (`batch.fill`: `full`) or ships a short one (`timeout`)"
                    .to_string(),
            )),
            (RentalMode::Batch, Some(batch)) => {
                out.extend(batch.problems());
                if let (Some(seqs), true) = (self.max_num_seqs, batch.size > 0) {
                    if batch.size > seqs {
                        out.push((
                            "batch.size".to_string(),
                            format!(
                                "a batch of {} cannot be scheduled by a server limited to \
                                 `max_num_seqs` {seqs}; raise `max_num_seqs` or lower the batch \
                                 size",
                                batch.size
                            ),
                        ));
                    }
                }
            }
            (RentalMode::Streaming, None) => {}
        }
        out
    }

    /// Problems that exist only because a *specific card* was chosen.
    ///
    /// Kept separate from [`Self::problems`] because these depend on the hardware legality matrix
    /// rather than on the shape of the request, and the panel renders them differently: a missing
    /// repo is a form error, whereas "this card cannot execute FP8" is a fact about the world the
    /// operator has to make a decision about.
    pub fn hardware_problems(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some(gpu) = crate::gpu::catalog::gpu_by_id(&self.gpu) {
            let method = crate::gpu::catalog::quant_method_by_id(&self.weight_quant);
            let verdict = match method {
                Some(method) => crate::gpu::catalog::resolve_quant_for(gpu, method.id),
                None => None,
            };
            if let Some(verdict) = verdict {
                if !verdict.runnable() {
                    out.push((
                        "weight_quant".to_string(),
                        format!(
                            "{}: {} on {} — {}",
                            verdict.support.as_str(),
                            method
                                .map(|m| m.label)
                                .unwrap_or(self.weight_quant.as_str()),
                            gpu.label,
                            verdict.reason
                        ),
                    ));
                }
            }
        }
        out
    }
}

/// The wrapper stored under `Provider.extra["rental"]`.
///
/// The extra map is flat, so the deployment needs one key to live under; this type is what makes
/// that key explicit in one place instead of in every caller that parses it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RentalExtra {
    /// The deployment itself.
    pub rental: RentalDeployment,
}

/// One deployment attached to a provider: the provider id, the parsed deployment, and whether the
/// provider is enabled.
///
/// `GET /api/gpu/rentals` returns these alongside the accounts. They are built from config plus the
/// usage store, and this type carries only the config half.
#[derive(Debug, Clone, PartialEq)]
pub struct RentalDeploymentView {
    /// The [`crate::model::Provider`] id this deployment is stored on.
    pub provider_id: String,
    /// Whether that provider is enabled.
    pub enabled: bool,
    /// The parsed deployment.
    pub deployment: RentalDeployment,
}

/// Parse the `rental` object out of a provider's `extra` map.
///
/// The provider is handed in whole rather than as a raw map so the error message can name it: "the
/// GPU on provider `modal-qwen27b` is not a card I know" is actionable, "unknown GPU id" is not.
///
/// A provider with no `rental` key at all is simply not a rental deployment, which is `Ok(None)`,
/// **not** an error. A provider *with* a malformed `rental` key is an error, because a value that
/// is present but unreadable means the feature is misconfigured, and defaulting it would hide that.
pub fn parse_deployment(
    provider_id: &str,
    extra: &BTreeMap<String, Value>,
) -> Result<Option<RentalDeployment>> {
    let Some(raw) = extra.get(RENTAL_EXTRA_KEY) else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    let parsed: RentalDeployment = serde_json::from_value(raw.clone()).map_err(|e| {
        Error::Config(format!(
            "provider {provider_id} has a malformed `{RENTAL_EXTRA_KEY}` object: {e}"
        ))
    })?;
    Ok(Some(parsed))
}

impl RentalDeployment {
    /// Serialise this deployment into a `Provider.extra` entry.
    ///
    /// The inverse of [`parse_deployment`], kept next to it so the storage shape has exactly one
    /// definition.
    pub fn to_extra_value(&self) -> Result<Value> {
        serde_json::to_value(self).map_err(|e| {
            Error::Config(format!(
                "could not serialise rental deployment for {}: {e}",
                self.repo
            ))
        })
    }
}

/// Which launch policy the operator chose for bringing up deployments.
///
/// The requirement is explicit: *"gpu rentals should be able to be done in sequence so 2 gpus
/// are never running or as batches of multiple GPUs."* This type expresses that choice.
///
/// The kebab-case identifiers — `"sequential"` and `"batched"` — are the **kinds**; they do not
/// carry the batch size. `RentalSchedule::as_str` returns only the kind because a number cannot be
/// part of a stable serialisable identifier the way a platform id is: the size is a separate
/// field (`RentalPlan::waves[].cards`).  This is the same shape [`RentalPlatform`] uses for
/// platform identifiers — the string says *what*, a numeric field says *how many*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RentalSchedule {
    /// At most one card is live at any moment.
    #[default]
    Sequential,
    /// Up to `max_live` cards are live together.
    Batched { max_live: u32 },
}

impl RentalSchedule {
    /// The stable identifier written to config or config-like strings.
    ///
    /// Returns `"sequential"` or `"batched"`.  Because `Batched` carries a number, the string
    /// is only the kind — a round-trip from serialised form always produces `Batched { max_live:
    /// 1 }` when the size is absent, which is why [`Self::parse`] returns `None` for a bare
    /// `"batched"` rather than guessing.  The size is always provided separately.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sequential => "sequential",
            Self::Batched { .. } => "batched",
        }
    }

    /// Parse a schedule kind, accepting case-insensitive trimmed input.
    ///
    /// Returns `Some(Sequential)` for `"sequential"`.  Returns `Some(Self::Batched { max_live:
    /// N })` for `"batched:N"` where `N` is a non-negative decimal integer.
    ///
    /// Returns `None` for a bare `"batched"` (no size).  A batch without a size cannot be
    /// planned — silently choosing 1 would make `"batched"` indistinguishable from `"sequential"`,
    /// and the operator's intent is unknown.  The caller should present a prompt or reject the
    /// input.
    pub fn parse(s: &str) -> Option<Self> {
        let trimmed = s.trim();
        let lower = trimmed.to_ascii_lowercase();
        if lower == "sequential" {
            return Some(Self::Sequential);
        }
        if lower == "batched" {
            // No size → cannot plan; return None rather than guessing.
            return None;
        }
        if let Some(rest) = lower.strip_prefix("batched:") {
            return rest
                .trim()
                .parse::<u32>()
                .ok()
                .map(|n| Self::Batched { max_live: n });
        }
        None
    }

    /// Construct a batched schedule with an explicit size.
    pub fn batched(max_live: u32) -> Self {
        Self::Batched { max_live }
    }

    /// The maximum number of cards that may be live simultaneously under this schedule.
    pub const fn max_live(self) -> u32 {
        match self {
            Self::Sequential => 1,
            Self::Batched { max_live } => max_live,
        }
    }
}

/// A wave: a set of deployments that may start simultaneously.
///
/// The `deployments` field holds indices into the original slice passed to [`plan_launch`], in
/// the order they were encountered during the greedy fill.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RentalWave {
    /// Indices into the slice that was planned, in the original order.
    pub deployments: Vec<usize>,
    /// Cards live simultaneously in this wave.
    pub cards: u32,
}

/// A plan for launching a set of GPU deployments.
///
/// The plan may contain **both** waves and problems — a deployment that exceeds the ceiling is
/// reported as a problem and simply excluded from waves; the other deployments still get scheduled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaunchPlan {
    /// The schedule that produced this plan.
    pub schedule: RentalSchedule,
    /// Waves to launch, in order.
    pub waves: Vec<RentalWave>,
    /// Problems that made a deployment unschedulable, as (field, message), matching the
    /// existing `problems()` convention in this file.
    #[serde(default)]
    pub problems: Vec<(String, String)>,
}

/// Plan launch waves for a set of deployments under the given schedule.
///
/// The function walks the input in original order, filling waves greedily up to
/// `schedule.max_live()` cards per wave.  Deployments whose `gpu_count` alone exceeds the
/// ceiling are reported as problems and excluded.
///
/// ## Greedy fill and order preservation
///
/// The walk preserves the order of `deployments`: the first deployment that fits goes into the
/// first wave, the next into the same wave if room, or a new wave if not.  The operator can
/// therefore control launch order simply by ordering the config.
///
/// ## Account ceilings are reported, not enforced
///
/// After waves are built the function counts deployments per account and reports a problem
/// whenever a `RentalAccount::max_concurrent_containers` ceiling is exceeded.  It does **not**
/// re-plan the waves to satisfy the ceiling because `max_concurrent_containers` is documented
/// only for one platform (Modal) and is `None` for all others — re-planning on an unverified
/// number would move the operator's work for no reason.  An account id with no matching
/// `RentalAccount` is also not a problem: validation of unknown accounts belongs to whoever
/// validates the deployment, not to the launcher.
pub fn plan_launch(
    schedule: RentalSchedule,
    deployments: &[RentalDeployment],
    accounts: &[RentalAccount],
) -> LaunchPlan {
    let max_live = schedule.max_live();

    // Rule 1: zero is never valid.
    if max_live == 0 {
        return LaunchPlan {
            schedule,
            waves: Vec::new(),
            problems: vec![(
                "schedule".to_string(),
                "batch size of 0 is not valid; a schedule with zero live cards can never deploy anything"
                    .to_owned(),
            )],
        };
    }

    let mut problems: Vec<(String, String)> = Vec::new();
    let mut waves: Vec<RentalWave> = Vec::new();
    let mut current_wave: Vec<usize> = Vec::new();
    let mut current_cards: u32 = 0;

    // Rule 2 + 3: walk in order, skip deployments that cannot fit, fill greedily.
    for (i, dep) in deployments.iter().enumerate() {
        let cards = dep.gpu_count;
        if cards > max_live {
            // Rule 2: report and skip; do NOT clamp, split, or silently drop.
            problems.push((
                "gpu_count".to_string(),
                format!(
                    "deployment {} (repo {}) requests {} card(s) but the schedule allows at most {} live card(s) simultaneously",
                    i, dep.repo, cards, max_live
                ),
            ));
            continue;
        }
        if current_cards + cards > max_live {
            // Flush current wave and start a new one.
            waves.push(RentalWave {
                deployments: std::mem::take(&mut current_wave),
                cards: current_cards,
            });
            current_cards = 0;
        }
        current_wave.push(i);
        current_cards += cards;
    }
    // Flush the last wave.
    if !current_wave.is_empty() {
        waves.push(RentalWave {
            deployments: current_wave,
            cards: current_cards,
        });
    }

    // Rule 4: report account ceilings, do not re-plan.
    for wave in &waves {
        let mut account_counts: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        for &idx in &wave.deployments {
            let acc = &deployments[idx].account;
            *account_counts.entry(acc.as_str()).or_default() += 1;
        }
        for (acc_id, count) in &account_counts {
            if let Some(account) = accounts.iter().find(|a| &a.id == acc_id) {
                if let Some(ceiling) = account.max_concurrent_containers {
                    if *count as u32 > ceiling {
                        problems.push((
                            "account".to_string(),
                            format!(
                                "account {} has {} deployment(s) in this wave but its max_concurrent_containers is {} (reported, not enforced)",
                                acc_id, count, ceiling
                            ),
                        ));
                    }
                }
                // If None, no ceiling is documented for this platform → ignore.
                // If no account matches, validation of unknown accounts is not our job → ignore.
            }
        }
    }

    LaunchPlan {
        schedule,
        waves,
        problems,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_defaults_to_unknown_never_to_free() {
        // The whole point: an unrecorded plan must not read as free. If this ever flips, some
        // operator gets a surprise invoice and the panel was the one that said it was free.
        let account = RentalAccount::new("mana-modal");
        assert_eq!(account.plan, RentalPlan::Unknown);
        assert_eq!(account.effective_plan(), RentalPlan::Unknown);
        assert!(!account.plan_is_known());

        let from_json: RentalAccount = serde_json::from_str(r#"{"id":"x"}"#).unwrap();
        assert_eq!(from_json.plan, RentalPlan::Unknown);
    }

    #[test]
    fn a_plan_round_trips_through_its_config_spelling() {
        for plan in [RentalPlan::Free, RentalPlan::Paid, RentalPlan::Unknown] {
            assert_eq!(RentalPlan::parse(plan.as_str()), Some(plan));
            let json = serde_json::to_string(&plan).unwrap();
            assert_eq!(json, format!("{:?}", plan.as_str()));
        }
        assert_eq!(RentalPlan::parse("PAID"), Some(RentalPlan::Paid));
        assert_eq!(RentalPlan::parse("enterprise"), None);
    }

    #[test]
    fn custom_platforms_round_trip_through_the_custom_prefix() {
        for platform in [
            RentalPlatform::Modal,
            RentalPlatform::Kaggle,
            RentalPlatform::Custom("runpod".to_string()),
        ] {
            assert_eq!(RentalPlatform::parse(&platform.as_str()), platform);
        }
        // A bare unknown word is kept as the platform's own slug rather than silently rewritten
        // to Modal.
        assert_eq!(
            RentalPlatform::parse("vast.ai"),
            RentalPlatform::Custom("vast.ai".to_string())
        );
        assert_eq!(
            RentalPlatform::parse("  Custom:RunPod "),
            RentalPlatform::Custom("runpod".to_string())
        );
    }

    #[test]
    fn platform_and_plan_serialise_to_their_documented_spellings() {
        assert_eq!(
            serde_json::to_string(&RentalPlatform::Modal).unwrap(),
            "\"modal\""
        );
        assert_eq!(
            serde_json::to_string(&RentalPlan::Unknown).unwrap(),
            "\"unknown\""
        );
        // kebab-case, so a multi-word platform id does not come back with capitals.
        assert_eq!(
            serde_json::to_string(&RentalPlatform::Kaggle).unwrap(),
            "\"kaggle\""
        );
    }

    #[test]
    fn an_account_without_a_credential_reports_a_problem() {
        let mut account = RentalAccount::new("mana-modal");
        let problems = account.problems();
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("no secret_ref"));

        account.secret_ref = Some("rental:mana-modal".to_string());
        assert!(account.has_credential());
        assert!(account.problems().is_empty());

        // Whitespace is not a credential.
        account.secret_ref = Some("   ".to_string());
        assert!(!account.has_credential());
    }

    #[test]
    fn a_zero_container_ceiling_is_a_problem_not_a_default() {
        let mut account = RentalAccount::new("a");
        account.secret_ref = Some("s".to_string());
        account.max_concurrent_containers = Some(0);
        assert!(account
            .problems()
            .iter()
            .any(|p| p.contains("zero concurrent")));
    }

    #[test]
    fn unknown_account_keys_are_preserved_on_round_trip() {
        let raw = r#"{"id":"a","label":"A","platform":"modal","future_field":{"x":1}}"#;
        let account: RentalAccount = serde_json::from_str(raw).unwrap();
        assert_eq!(account.extra["future_field"]["x"], 1);
        let back = serde_json::to_string(&account).unwrap();
        assert!(back.contains("future_field"));
    }

    #[test]
    fn deployment_defaults_are_single_card_unquantised_streaming() {
        let d = RentalDeployment::new("a", "org/repo", "L40S");
        assert_eq!(d.gpu_count, 1);
        assert!(!d.is_multi_gpu());
        assert_eq!(d.mode, RentalMode::Streaming);
        assert_eq!(d.weight_quant, "none");
        assert_eq!(d.kv_cache_dtype, "auto");
        assert!(d.problems().is_empty(), "{:?}", d.problems());
    }

    #[test]
    fn a_deployment_with_a_missing_gpu_count_still_deserialises_to_one_card() {
        // Hand-edited config and older files both omit it; a missing `gpu_count` must not mean
        // "zero cards", which would price and size as nothing.
        let d: RentalDeployment = serde_json::from_str(
            r#"{"account":"a","repo":"org/repo","gpu":"L40S","weight_quant":"fp8",
                "kv_cache_dtype":"fp8"}"#,
        )
        .unwrap();
        assert_eq!(d.gpu_count, 1);
    }

    #[test]
    fn an_unknown_gpu_is_named_in_the_problem() {
        let mut d = RentalDeployment::new("a", "org/repo", "A100-40");
        let problems = d.problems();
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].0, "gpu");
        assert!(problems[0].1.contains("A100-40"));

        // The spelled-and-cased wrong id is still wrong: the catalog is exact.
        d.gpu = "a100-40gb".to_string();
        assert_eq!(d.problems().len(), 1);
        d.gpu = "A100-40GB".to_string();
        assert!(d.problems().is_empty());
    }

    #[test]
    fn scaledown_window_outside_the_platform_range_is_a_problem() {
        let mut d = RentalDeployment::new("a", "org/repo", "L40S");
        d.shutdown_cooldown_secs = Some(1);
        let problems = d.problems();
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].0, "shutdown_cooldown_secs");
        assert!(problems[0].1.contains("2 and 1200"));

        d.shutdown_cooldown_secs = Some(1200);
        assert!(d.problems().is_empty());
        d.shutdown_cooldown_secs = Some(1201);
        assert_eq!(d.problems().len(), 1);
    }

    #[test]
    fn zero_max_num_seqs_is_a_problem_because_vllm_refuses_it_at_startup() {
        // Recorded in the wild: `--max-num-seqs 0` fails vLLM's own argument validation
        // ("Input should be greater than or equal to 1"), so the container never binds its port
        // and Modal reports a startup timeout on a paid GPU. None means "let vLLM choose".
        let mut d = RentalDeployment::new("a", "org/repo", "L40S");
        d.max_num_seqs = Some(1);
        assert!(d.problems().is_empty(), "{:?}", d.problems());
        d.max_num_seqs = Some(0);
        let problems = d.problems();
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].0, "max_num_seqs");
        assert!(problems[0].1.contains("at least 1"), "{:?}", problems[0]);
        d.max_num_seqs = None;
        assert!(d.problems().is_empty());
    }

    #[test]
    fn a_card_that_cannot_run_the_quant_is_a_hardware_problem_not_a_form_error() {
        // T4 tops out at compute capability 7.5, below the 8.9 that native FP8 GEMM needs.
        let mut d = RentalDeployment::new("a", "org/repo", "T4");
        d.weight_quant = "fp8".to_string();
        assert!(d.problems().is_empty(), "the form itself is fine");
        let hardware = d.hardware_problems();
        assert_eq!(hardware.len(), 1);
        assert_eq!(hardware[0].0, "weight_quant");
        assert!(hardware[0].1.contains("T4"), "{:?}", hardware[0]);
    }

    #[test]
    fn total_vram_and_price_scale_with_the_card_count() {
        let mut d = RentalDeployment::new("a", "org/repo", "H100");
        let one = d.total_vram_bytes().unwrap();
        let hour = d.price_usd_per_hour().unwrap();
        d.gpu_count = 4;
        assert!(d.is_multi_gpu());
        assert_eq!(d.total_vram_bytes().unwrap(), one * 4);
        assert!((d.price_usd_per_hour().unwrap() - hour * 4.0).abs() < 1e-9);

        d.gpu = "not-a-card".to_string();
        assert_eq!(d.total_vram_bytes(), None);
        assert_eq!(d.price_usd_per_hour(), None);
    }

    #[test]
    fn a_provider_with_no_rental_key_is_not_a_rental_deployment() {
        let extra = BTreeMap::new();
        assert_eq!(parse_deployment("p", &extra).unwrap(), None);

        let mut extra = BTreeMap::new();
        extra.insert(RENTAL_EXTRA_KEY.to_string(), Value::Null);
        assert_eq!(parse_deployment("p", &extra).unwrap(), None);
    }

    #[test]
    fn a_malformed_rental_object_is_an_error_that_names_the_provider() {
        let mut extra = BTreeMap::new();
        // `gpu` is required and absent.
        extra.insert(
            RENTAL_EXTRA_KEY.to_string(),
            serde_json::json!({"account": "a", "repo": "r"}),
        );
        let err = parse_deployment("modal-qwen27b", &extra).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("modal-qwen27b"), "{text}");
        assert!(text.contains("gpu"), "{text}");
    }

    #[test]
    fn a_deployment_round_trips_through_provider_extra() {
        let mut d = RentalDeployment::new("mana-modal", "orcarouter/x-FP8", "RTX-PRO-6000");
        d.revision = Some("b32584cc".to_string());
        d.weight_quant = "fp8".to_string();
        d.kv_cache_dtype = "fp8".to_string();
        d.max_model_len = Some(262_144);
        d.max_num_seqs = Some(64);
        d.shutdown_cooldown_secs = Some(60);
        d.mode = RentalMode::Batch;
        d.requantize = Some(crate::gpu::requantize::plan_requantize(
            &crate::gpu::requantize::RecipeRequest::default(),
        ));

        let mut extra = BTreeMap::new();
        extra.insert(RENTAL_EXTRA_KEY.to_string(), d.to_extra_value().unwrap());
        assert_eq!(parse_deployment("p", &extra).unwrap(), Some(d));
    }

    #[test]
    fn an_unknown_key_inside_a_deployment_survives_a_round_trip() {
        // A v2 panel may add a deployment field this binary has never heard of; loading and saving
        // must not delete it.
        let raw = r#"{"account":"a","repo":"r","gpu":"L40S","weight_quant":"fp8",
                      "kv_cache_dtype":"fp8","future_knob":true}"#;
        let d: RentalDeployment = serde_json::from_str(raw).unwrap();
        assert_eq!(d.extra["future_knob"], true);
        let value = d.to_extra_value().unwrap();
        assert_eq!(value["future_knob"], true);
    }

    #[test]
    fn a_mode_round_trips_through_its_config_spelling() {
        for mode in [RentalMode::Streaming, RentalMode::Batch] {
            assert_eq!(RentalMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(RentalMode::parse("batch"), Some(RentalMode::Batch));
        assert_eq!(RentalMode::Streaming.as_str(), "streaming");
        assert_eq!(
            serde_json::to_string(&RentalMode::Streaming).unwrap(),
            "\"streaming\""
        );
        // The old spelling parses to the same single mode, and is never emitted.
        assert_eq!(
            RentalMode::parse("interactive"),
            Some(RentalMode::Streaming)
        );
        // Parsing is not the only way a stored mode arrives: `parse_deployment` reads it through
        // serde, so the alias has to sit on the enum or an upgraded config fails to load.
        assert_eq!(
            serde_json::from_str::<RentalMode>("\"interactive\"").unwrap(),
            RentalMode::Streaming
        );
        let deployment = RentalDeployment::new("mana-modal", "Qwen/Qwen3-8B", "rtx-pro-6000");
        let mut stored = deployment.to_extra_value().unwrap();
        stored["mode"] = serde_json::json!("interactive");
        assert_eq!(
            parse_deployment(
                "modal-qwen3-8b",
                &BTreeMap::from([(RENTAL_EXTRA_KEY.to_owned(), stored)])
            )
            .unwrap()
            .unwrap()
            .mode,
            RentalMode::Streaming
        );
    }

    fn shape<'a>(account: &'a str, repo: &'a str, gpu: &'a str) -> DeploymentShape<'a> {
        DeploymentShape {
            account_id: account,
            repo,
            mode: RentalMode::Streaming,
            gpu,
            gpu_count: 1,
        }
    }

    #[test]
    fn a_workspace_derives_one_shared_modal_url() {
        // Golden value: the panel model mirrors this function, so both sides preview and record the
        // same string for the same deployment.
        assert_eq!(
            modal_base_url("mana", shape("mana-modal", "Qwen/Qwen3-8B", "RTX-PRO-6000")).as_deref(),
            Some(
                "https://mana--manarouter-rental-mana-modal-qwen3-8b-streaming-rtx-pro-6000-\
                 serve.modal.run"
            )
        );
        assert_eq!(
            modal_app_name(shape("mana-modal", "Qwen/Qwen3-8B", "RTX-PRO-6000")),
            "manarouter-rental-mana-modal-qwen3-8b-streaming-rtx-pro-6000"
        );
        // The workspace and the account id are normalised, not trusted verbatim.
        assert_eq!(
            modal_base_url("Mana_Team", shape("Mana Modal", "org/Qwen3.8", "L40S")).as_deref(),
            Some(
                "https://mana-team--manarouter-rental-mana-modal-qwen3-8-streaming-l40s-serve.\
                 modal.run"
            )
        );
        // No workspace means no invented URL.
        assert_eq!(modal_base_url("", shape("a", "org/repo", "L40S")), None);
        assert_eq!(modal_base_url("   ", shape("a", "org/repo", "L40S")), None);
        assert_eq!(modal_base_url("mana", shape("", "org/repo", "L40S")), None);
        assert_eq!(modal_base_url("mana", shape("a", "", "L40S")), None);
    }

    #[test]
    fn one_repository_on_one_account_gets_a_name_per_mode_and_card() {
        // The requirement this encodes: stream on one card, then batch with a timeout on another,
        // then batch that waits for a full batch — three deployments of one checkpoint under one
        // account, and none of them may share a provider id, an app name or an endpoint.
        let base = shape("mana-modal", "Qwen/Qwen3-8B", "RTX-PRO-6000");
        let batch_full = DeploymentShape {
            mode: RentalMode::Batch,
            ..base
        };
        let other_card = DeploymentShape {
            gpu: "L40S",
            ..base
        };
        let two_cards = DeploymentShape {
            gpu_count: 2,
            ..base
        };
        let slugs = [
            app_slug(base),
            app_slug(batch_full),
            app_slug(other_card),
            app_slug(two_cards),
        ];
        assert_eq!(
            slugs[0],
            "rental-mana-modal-qwen3-8b-streaming-rtx-pro-6000"
        );
        assert_eq!(slugs[1], "rental-mana-modal-qwen3-8b-batch-rtx-pro-6000");
        assert_eq!(slugs[2], "rental-mana-modal-qwen3-8b-streaming-l40s");
        assert_eq!(
            slugs[3],
            "rental-mana-modal-qwen3-8b-streaming-rtx-pro-6000-x2"
        );
        let mut seen = std::collections::BTreeSet::new();
        for slug in &slugs {
            assert!(seen.insert(slug.clone()), "two deployments share `{slug}`");
        }
    }

    #[test]
    fn a_long_name_is_truncated_from_the_front_so_the_suffix_survives() {
        // Truncating the tail would collapse every deployment of a long repository onto one name,
        // which is the collision the suffix exists to prevent.
        let long = "org/".to_owned() + &"very-long-model-name-".repeat(6);
        let shape = DeploymentShape {
            account_id: "an-account-with-a-long-id",
            repo: &long,
            mode: RentalMode::Batch,
            gpu: "RTX-PRO-6000",
            gpu_count: 4,
        };
        let slug = app_slug(shape);
        let app = modal_app_name(shape);
        // Modal measures the limit on the finished name, prefix included: a 64-character name is
        // rejected outright, and a long account and repository once produced a 79-character one.
        assert!(
            app.len() < 64,
            "`{app}` is {} characters and Modal will refuse it",
            app.len()
        );
        assert!(app.starts_with(APP_NAME_PREFIX), "got `{app}`");
        assert!(slug.ends_with("-batch-rtx-pro-6000-x4"), "got `{slug}`");
        assert!(!slug.contains("--"));
    }

    #[test]
    fn an_account_carries_a_workspace_and_older_files_still_load() {
        let mut account = RentalAccount::new("a");
        assert_eq!(account.workspace, None);
        account.workspace = Some("mana".to_string());
        let back: RentalAccount =
            serde_json::from_str(&serde_json::to_string(&account).unwrap()).unwrap();
        assert_eq!(back.workspace.as_deref(), Some("mana"));

        let old: RentalAccount = serde_json::from_str(r#"{"id":"old","label":"Old"}"#).unwrap();
        assert_eq!(old.workspace, None);
    }

    #[test]
    fn fact_sources_use_the_spellings_the_catalog_contract_names() {
        // §5 of the architecture doc fixes these three words on the wire.
        assert_eq!(FactSource::Documented.as_str(), "documented");
        assert_eq!(FactSource::Observed.as_str(), "observed");
        assert_eq!(FactSource::Unverified.as_str(), "unverified");
        assert_eq!(
            serde_json::to_string(&FactSource::Observed).unwrap(),
            "\"observed\""
        );
    }

    #[test]
    fn free_plan_uses_the_spellings_the_catalog_contract_names() {
        assert_eq!(FreePlan::Available.as_str(), "available");
        assert_eq!(FreePlan::Unavailable.as_str(), "unavailable");
        assert_eq!(FreePlan::Unknown.as_str(), "unknown");
        assert_eq!(FreePlan::default(), FreePlan::Unknown);
        assert_eq!(
            serde_json::to_string(&FreePlan::Unavailable).unwrap(),
            "\"unavailable\""
        );
    }

    // ---------------------------------------------------------------------------
    // Launch-schedule tests
    // ---------------------------------------------------------------------------

    #[test]
    fn sequential_puts_every_deployment_in_its_own_wave() {
        let deps = [
            RentalDeployment::new("a", "repo/1", "L40S"),
            RentalDeployment::new("a", "repo/2", "L40S"),
            RentalDeployment::new("a", "repo/3", "L40S"),
        ];
        let plan = plan_launch(RentalSchedule::Sequential, &deps, &[]);
        assert_eq!(plan.waves.len(), 3);
        assert_eq!(plan.waves[0].cards, 1);
        assert_eq!(plan.waves[1].cards, 1);
        assert_eq!(plan.waves[2].cards, 1);
        assert!(plan.problems.is_empty());
    }

    #[test]
    fn sequential_refuses_a_two_card_deployment() {
        let mut d = RentalDeployment::new("a", "multi-gpu-repo", "L40S");
        d.gpu_count = 2;
        let deps = [d];
        let plan = plan_launch(RentalSchedule::Sequential, &deps, &[]);
        assert!(plan.waves.is_empty());
        assert_eq!(plan.problems.len(), 1);
        assert_eq!(plan.problems[0].0, "gpu_count");
        assert!(plan.problems[0].1.contains("multi-gpu-repo"));
        assert!(plan.problems[0].1.contains("2"));
        assert!(plan.problems[0].1.contains("1"));
    }

    #[test]
    fn batched_fills_a_wave_up_to_the_ceiling() {
        let mut deps = [
            RentalDeployment::new("a", "repo/1", "L40S"),
            RentalDeployment::new("a", "repo/2", "L40S"),
            RentalDeployment::new("a", "repo/3", "L40S"),
            RentalDeployment::new("a", "repo/4", "L40S"),
        ];
        for d in &mut deps {
            d.gpu_count = 2;
        }
        let plan = plan_launch(RentalSchedule::batched(4), &deps, &[]);
        assert_eq!(plan.waves.len(), 2);
        assert_eq!(plan.waves[0].deployments.len(), 2);
        assert_eq!(plan.waves[0].cards, 4);
        assert_eq!(plan.waves[1].deployments.len(), 2);
        assert_eq!(plan.waves[1].cards, 4);
    }

    #[test]
    fn batched_that_cannot_fit_one_deployment_reports_it() {
        let mut d = RentalDeployment::new("a", "big-repo", "L40S");
        d.gpu_count = 3;
        let deps = [d];
        let plan = plan_launch(RentalSchedule::batched(2), &deps, &[]);
        assert!(plan.waves.is_empty());
        assert_eq!(plan.problems.len(), 1);
        assert_eq!(plan.problems[0].0, "gpu_count");
    }

    #[test]
    fn a_zero_ceiling_is_a_problem_not_a_clamp() {
        let deps = [RentalDeployment::new("a", "repo/1", "L40S")];
        let plan = plan_launch(RentalSchedule::batched(0), &deps, &[]);
        assert!(plan.waves.is_empty());
        assert_eq!(plan.problems.len(), 1);
        assert_eq!(plan.problems[0].0, "schedule");
        assert!(plan.problems[0].1.contains("0"));
    }

    #[test]
    fn order_is_preserved() {
        let deps = [
            RentalDeployment::new("a", "repo/0", "L40S"),
            RentalDeployment::new("a", "repo/1", "L40S"),
            RentalDeployment::new("a", "repo/2", "L40S"),
            RentalDeployment::new("a", "repo/3", "L40S"),
        ];
        let plan = plan_launch(RentalSchedule::batched(3), &deps, &[]);
        assert_eq!(plan.waves[0].deployments, vec![0, 1, 2]);
        assert_eq!(plan.waves[1].deployments, vec![3]);
    }

    #[test]
    fn an_account_ceiling_is_reported_not_replanned() {
        let mut acc = RentalAccount::new("mana");
        acc.max_concurrent_containers = Some(1);
        let deps = [
            RentalDeployment::new("mana", "repo/1", "L40S"),
            RentalDeployment::new("mana", "repo/2", "L40S"),
        ];
        let plan = plan_launch(RentalSchedule::batched(4), &deps, &[acc]);
        // Still one wave — proving no re-planning happened.
        assert_eq!(plan.waves.len(), 1);
        assert_eq!(plan.waves[0].deployments.len(), 2);
        // But one problem about the account ceiling.
        assert!(plan
            .problems
            .iter()
            .any(|(f, m)| f == "account" && m.contains("mana") && m.contains("1")));
    }

    #[test]
    fn an_unknown_account_is_not_a_problem() {
        let deps = [RentalDeployment::new("unknown-account", "repo/1", "L40S")];
        let plan = plan_launch(RentalSchedule::batched(4), &deps, &[]);
        assert_eq!(plan.waves.len(), 1);
        assert!(plan.problems.is_empty());
    }

    #[test]
    fn parse_rejects_a_bare_batched() {
        assert_eq!(RentalSchedule::parse("batched"), None);
        assert_eq!(
            RentalSchedule::parse("sequential"),
            Some(RentalSchedule::Sequential)
        );
        assert_eq!(
            RentalSchedule::parse("  SEQUENTIAL "),
            Some(RentalSchedule::Sequential)
        );
        assert_eq!(
            RentalSchedule::parse("Batched:4"),
            Some(RentalSchedule::Batched { max_live: 4 })
        );
    }

    #[test]
    fn an_empty_set_plans_nothing() {
        let plan = plan_launch(RentalSchedule::Sequential, &[], &[]);
        assert!(plan.waves.is_empty());
        assert!(plan.problems.is_empty());
    }
}
