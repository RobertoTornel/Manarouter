//! The GPU-job lane: agents submit custom containerized jobs to rented GPUs.
//!
//! The request path is for one interactive call: chat, embeddings, images. A custom job is a different
//! shape — a container, a command, a GPU budget, and an artifact to hand back. This lane is the relay's
//! answer to that shape: `POST /v1/jobs` accepts a job, the lane drives it on a backend, and
//! `GET /v1/jobs/{id}` reports progress, logs and artifacts. It is deliberately a *lane* like the batch
//! lane, not a pass-through: the job is executed by a backend the lane owns, and nothing here claims a
//! platform did something the backend did not do.
//!
//! # Backends
//!
//! A backend implements [`JobBackend`]. Two ship with the relay:
//!
//! - `demo` — a deterministic local simulator. It costs nothing, needs no credential, and exercises the
//!   whole API: queued → running → succeeded with an artifact, or failed when the command contains
//!   `fail`. This is the backend the live verification below uses.
//! - `vast` — a rental backend against the Vast.ai REST API (`console.vast.ai/api/v0`). It creates an
//!   instance from the job's image/command, polls the instance status, and terminates it on cancel. It
//!   needs the operator's `VAST_API_KEY` environment variable, and its API contract is implemented from
//!   Vast's published endpoints; it has not been verified against a live rental (no key in this
//!   environment). See `docs/27-gpu-jobs.md`.
//!
//! # Where the money goes
//!
//! A job may carry `max_seconds`; the lane enforces it as a wall-clock deadline and fails the job when
//! it is exceeded. Per-second rental pricing is a provider fact this lane does not know, so no cost is
//! invented: the job view reports `max_seconds` and lets the operator's billing ledger do the rest.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::body::Body as AxumBody;
use axum::extract::{Path as RoutePath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::batch::Refusal;
use crate::server::AppState;

/// The most bytes an accepted job body may occupy.
pub const MAX_JOB_BODY_BYTES: usize = 1 << 20;

/// The largest readable artifact, in bytes.
pub const MAX_ARTIFACT_BYTES: usize = 64 * 1024 * 1024;

/// The ceiling for `max_seconds` (six hours).
pub const MAX_MAX_SECONDS: u64 = 6 * 3600;

/// The default GPU count for a job that does not name one.
fn default_gpu_count() -> u32 {
    1
}

/// The default wall-clock deadline for a job that does not name one.
fn default_max_seconds() -> u64 {
    3600
}

/// Milliseconds since the Unix epoch.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Truncate a string for an error message.
fn truncate(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push_str("…");
    }
    out
}

/// Lifecycle of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    /// Accepted but not yet launched by the backend.
    Queued,
    /// Launched; the backend is working on it.
    Running,
    /// Finished successfully; artifacts are readable.
    Succeeded,
    /// Finished unsuccessfully; `error` explains why.
    Failed,
    /// Cancelled by the operator before reaching a terminal state.
    Cancelled,
}

/// What `POST /v1/jobs` accepts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    /// Backend name: `demo` or `vast`.
    pub backend: String,
    /// Container image to run (e.g. `python:3.12-slim`).
    pub image: String,
    /// Command line inside the container. Empty means the image's default command.
    #[serde(default)]
    pub command: Vec<String>,
    /// Environment variables to pass to the container.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Preferred GPU model name (backend-dependent).
    #[serde(default)]
    pub gpu: Option<String>,
    /// Number of GPUs to request.
    #[serde(default = "default_gpu_count")]
    pub gpu_count: u32,
    /// Container disk size in gigabytes.
    #[serde(default)]
    pub disk_gb: Option<u32>,
    /// Wall-clock deadline in seconds; the lane fails the job when exceeded.
    #[serde(default = "default_max_seconds")]
    pub max_seconds: u64,
}

/// The persisted state of one job, under `state_dir()/jobs/<id>/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: String,
    pub spec: JobSpec,
    pub status: JobStatus,
    pub created_at_ms: u64,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    /// Append-only log lines, capped by the driver.
    pub logs: Vec<String>,
    pub error: Option<String>,
    /// File names under the job directory that are downloadable.
    pub artifacts: Vec<String>,
    /// Backend-specific detail (e.g. the Vast instance id).
    #[serde(default)]
    pub backend_detail: Option<Value>,
    #[serde(default)]
    pub cancelled: bool,
}

impl JobRecord {
    /// The JSON shape returned to API callers.
    fn view(&self) -> Value {
        json!({
          "id": self.id,
          "backend": self.spec.backend,
          "status": self.status,
          "created_at": self.created_at_ms,
          "started_at": self.started_at_ms,
          "finished_at": self.finished_at_ms,
          "logs": self.logs,
          "error": self.error,
          "artifacts": self.artifacts,
          "backend_detail": self.backend_detail,
          "image": self.spec.image,
          "command": self.spec.command,
          "env": self.spec.env,
          "gpu": self.spec.gpu,
          "gpu_count": self.spec.gpu_count,
          "max_seconds": self.spec.max_seconds,
        })
    }
}

/// The job lane: a directory of job records plus the driver that advances them.
pub struct JobLane {
    root: PathBuf,
}

impl JobLane {
    /// Open a lane rooted at `root` (normally `state_dir()/jobs`).
    pub fn open(root: PathBuf) -> Self {
        Self { root }
    }

    /// A lane that can never read or write anything, for tests and inert state.
    pub fn inert() -> Self {
        Self {
            root: PathBuf::from("/nonexistent-manarouter-jobs"),
        }
    }

    fn job_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.job_dir(id).join("job.json")
    }

    fn load(&self, id: &str) -> Result<JobRecord, Refusal> {
        let bytes =
            std::fs::read(self.record_path(id)).map_err(|_| Refusal::not_found("job", id))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| Refusal::internal(format!("job record {id} is unreadable: {e}")))
    }

    fn save(&self, rec: &JobRecord) -> Result<(), Refusal> {
        let dir = self.job_dir(&rec.id);
        std::fs::create_dir_all(&dir)
            .map_err(|e| Refusal::internal(format!("could not create job directory: {e}")))?;
        let bytes = serde_json::to_vec_pretty(rec)
            .map_err(|e| Refusal::internal(format!("could not encode job record: {e}")))?;
        std::fs::write(self.record_path(&rec.id), bytes)
            .map_err(|e| Refusal::internal(format!("could not write job record: {e}")))
    }

    /// Accept a job: persist it, launch the driver, and return the view.
    pub async fn submit(self: &Arc<Self>, spec: JobSpec) -> Result<Value, Refusal> {
        validate_submission(&spec)?;
        let id = mr_core::ids::new_id();
        let rec = JobRecord {
            id: id.clone(),
            spec,
            status: JobStatus::Queued,
            created_at_ms: now_ms(),
            started_at_ms: None,
            finished_at_ms: None,
            logs: vec!["queued".to_owned()],
            error: None,
            artifacts: vec![],
            backend_detail: None,
            cancelled: false,
        };
        self.save(&rec)?;
        let me = Arc::clone(self);
        tokio::spawn(async move {
            driver(me, id).await;
        });
        Ok(rec.view())
    }

    /// The view of one job.
    pub fn view(&self, id: &str) -> Result<Value, Refusal> {
        Ok(self.load(id)?.view())
    }

    /// Cancel a queued or running job. The local state is authoritative; a backend terminate is
    /// best effort so a stuck remote does not leave the lane lying about the job's state.
    pub async fn cancel(&self, id: &str) -> Result<Value, Refusal> {
        let mut rec = self.load(id)?;
        match rec.status {
            JobStatus::Queued | JobStatus::Running => {}
            _ => {
                return Err(Refusal::bad_request(
                    "invalid_request_error",
                    "only queued or running jobs can be cancelled",
                ));
            }
        }
        rec.cancelled = true;
        if let Ok(backend) = backend_for(&rec.spec.backend) {
            let _ = backend.cancel(&rec).await;
        }
        rec.status = JobStatus::Cancelled;
        rec.finished_at_ms = Some(now_ms());
        rec.logs.push("cancelled by operator".to_owned());
        self.save(&rec)?;
        Ok(rec.view())
    }

    /// List job views, newest first, with `after` paging by id.
    pub fn list(&self, limit: usize, after: Option<&str>) -> Vec<Value> {
        let mut ids: Vec<String> = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect(),
            Err(_) => return Vec::new(),
        };
        ids.sort_by(|a, b| b.cmp(a));
        let skip = match after {
            Some(a) => ids
                .iter()
                .position(|i| i == a)
                .map(|i| i + 1)
                .unwrap_or(usize::MAX),
            None => 0,
        };
        ids.into_iter()
            .skip(skip)
            .take(limit)
            .filter_map(|id| self.load(&id).ok())
            .map(|rec| rec.view())
            .collect()
    }

    /// Read one artifact file. Names must be plain file names; traversal is refused.
    pub fn artifact_bytes(&self, id: &str, name: &str) -> Result<(Vec<u8>, &'static str), Refusal> {
        if name.is_empty()
            || name.chars().any(|c| c == '/' || c == '\\')
            || name == "."
            || name == ".."
        {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                "artifact names must be plain file names",
            ));
        }
        let rec = self.load(id)?;
        if !rec.artifacts.iter().any(|a| a == name) {
            return Err(Refusal::not_found("artifact", name));
        }
        let bytes = std::fs::read(self.job_dir(id).join(name))
            .map_err(|_| Refusal::not_found("artifact", name))?;
        if bytes.len() > MAX_ARTIFACT_BYTES {
            return Err(Refusal::with_status(
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_request_error",
                "artifact exceeds the readable size limit",
            ));
        }
        let mime = if name.ends_with(".png") {
            "image/png"
        } else {
            "application/octet-stream"
        };
        Ok((bytes, mime))
    }
}

/// Reject a submission that cannot possibly run, with the reason.
fn validate_submission(spec: &JobSpec) -> Result<(), Refusal> {
    if backend_for(&spec.backend).is_err() {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            format!(
                "unknown job backend {:?} (supported: \"demo\", \"vast\")",
                spec.backend
            ),
        ));
    }
    if spec.image.trim().is_empty() {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            "\"image\" must name a container image",
        ));
    }
    if spec.gpu_count == 0 || spec.gpu_count > 8 {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            "\"gpu_count\" must be between 1 and 8",
        ));
    }
    if spec.max_seconds == 0 || spec.max_seconds > MAX_MAX_SECONDS {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            format!("\"max_seconds\" must be between 1 and {MAX_MAX_SECONDS}"),
        ));
    }
    Ok(())
}

/// What a backend reports after launching a job.
pub struct LaunchOutcome {
    pub backend_detail: Value,
}

/// What a backend reports on each poll.
pub struct PollOutcome {
    pub status: JobStatus,
    pub logs: Vec<String>,
    pub artifacts: Vec<String>,
    pub error: Option<String>,
    pub backend_detail: Value,
}

/// A backend executes jobs for the lane. Implementations must be honest: a backend that cannot
/// know a result must say so instead of guessing (e.g. the Vast backend maps instance states to
/// job states and documents the mapping).
#[async_trait]
pub trait JobBackend: Send + Sync {
    fn id(&self) -> &'static str;
    async fn launch(&self, job: &JobRecord) -> Result<LaunchOutcome, String>;
    async fn poll(&self, job: &JobRecord, job_dir: &Path) -> Result<PollOutcome, String>;
    async fn cancel(&self, job: &JobRecord) -> Result<(), String>;
}

fn backend_for(name: &str) -> Result<Box<dyn JobBackend>, String> {
    match name {
        "demo" => Ok(Box::new(DemoBackend)),
        "vast" => Ok(Box::new(VastBackend)),
        other => Err(format!(
            "unknown job backend \"{other}\" (supported: demo, vast)"
        )),
    }
}

/// Drive one job on its backend until a terminal state, persisting after each change.
async fn driver(lane: Arc<JobLane>, id: String) {
    let Ok(mut rec) = lane.load(&id) else { return };
    let backend = match backend_for(&rec.spec.backend) {
        Ok(b) => b,
        Err(e) => {
            rec.status = JobStatus::Failed;
            rec.error = Some(e);
            rec.finished_at_ms = Some(now_ms());
            let _ = lane.save(&rec);
            return;
        }
    };
    match backend.launch(&rec).await {
        Ok(outcome) => {
            rec.backend_detail = Some(outcome.backend_detail);
            rec.status = JobStatus::Running;
            rec.started_at_ms = Some(now_ms());
            rec.logs.push("launched on backend".to_owned());
        }
        Err(e) => {
            rec.status = JobStatus::Failed;
            rec.error = Some(e);
            rec.finished_at_ms = Some(now_ms());
            let _ = lane.save(&rec);
            return;
        }
    }
    let job_dir = lane.job_dir(&id);
    let deadline = now_ms() + rec.spec.max_seconds.saturating_mul(1000);

    loop {
        let _ = lane.save(&rec);
        match lane.load(&id) {
            Ok(current) if current.cancelled => {
                rec = current;
                break;
            }
            _ => {}
        }
        if now_ms() >= deadline {
            rec.status = JobStatus::Failed;
            rec.error = Some("the job exceeded its max_seconds budget".to_owned());
            rec.finished_at_ms = Some(now_ms());
            break;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        match backend.poll(&rec, &job_dir).await {
            Ok(outcome) => {
                rec.status = outcome.status;
                rec.logs.extend(outcome.logs);
                rec.artifacts = outcome.artifacts;
                rec.backend_detail = Some(outcome.backend_detail);
                rec.error = outcome.error;
                if matches!(rec.status, JobStatus::Succeeded | JobStatus::Failed) {
                    rec.finished_at_ms = Some(now_ms());
                    break;
                }
            }
            Err(e) => {
                rec.status = JobStatus::Failed;
                rec.error = Some(e);
                rec.finished_at_ms = Some(now_ms());
                break;
            }
        }
    }
    if !rec.cancelled {
        let _ = lane.save(&rec);
    }
}

/// The deterministic local simulator. No network, no credential; used for tests and for
/// exercising the whole API without a rental account.
struct DemoBackend;

#[async_trait]
impl JobBackend for DemoBackend {
    fn id(&self) -> &'static str {
        "demo"
    }

    async fn launch(&self, _job: &JobRecord) -> Result<LaunchOutcome, String> {
        Ok(LaunchOutcome {
            backend_detail: json!({"provider": "local-simulator"}),
        })
    }

    async fn poll(&self, job: &JobRecord, job_dir: &Path) -> Result<PollOutcome, String> {
        let ms = job
            .spec
            .env
            .get("DEMO_MS")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(500);
        let elapsed = job
            .started_at_ms
            .map(|s| now_ms().saturating_sub(s))
            .unwrap_or(0);
        let fail = job.spec.command.iter().any(|c| c.contains("fail"));
        if fail && elapsed >= ms {
            return Ok(PollOutcome {
                status: JobStatus::Failed,
                logs: vec!["simulated failure triggered by the command line".to_owned()],
                artifacts: vec![],
                error: Some(
                    "the demo backend failed because the command contained \"fail\"".to_owned(),
                ),
                backend_detail: json!({"provider": "local-simulator"}),
            });
        }
        if elapsed < ms {
            return Ok(PollOutcome {
                status: JobStatus::Running,
                logs: vec![format!("simulating GPU pulse at +{elapsed}ms")],
                artifacts: vec![],
                error: None,
                backend_detail: json!({"provider": "local-simulator"}),
            });
        }

        let artifact = job_dir.join("output.txt");
        if !artifact.exists() {
            std::fs::write(
                &artifact,
                format!(
                    "demo job {} finished
simulated render artifact
",
                    job.id
                ),
            )
            .map_err(|e| format!("could not write demo artifact: {e}"))?;
        }
        Ok(PollOutcome {
            status: JobStatus::Succeeded,
            logs: vec!["demo job completed".to_owned()],
            artifacts: vec!["output.txt".to_owned()],
            error: None,
            backend_detail: json!({"provider": "local-simulator"}),
        })
    }

    async fn cancel(&self, _job: &JobRecord) -> Result<(), String> {
        Ok(())
    }
}

/// The shared HTTP client for rental-backend calls.
type JobClient = hyper_util::client::legacy::Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    http_body_util::Full<Bytes>,
>;

fn build_job_client() -> Result<JobClient, String> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let mut tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let mut connector = hyper_util::client::legacy::connect::HttpConnector::new();
    connector.enforce_http(false);
    connector.set_nodelay(true);
    let https = hyper_rustls::HttpsConnector::from((connector, Arc::new(tls)));
    Ok(
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(30))
            .build(https),
    )
}

fn job_client() -> Result<&'static JobClient, String> {
    static CLIENT: OnceLock<Result<JobClient, String>> = OnceLock::new();
    match CLIENT.get_or_init(|| build_job_client()) {
        Ok(c) => Ok(c),
        Err(e) => Err(e.clone()),
    }
}

/// One HTTP round-trip to a rental backend.
async fn job_http(
    method: &'static str,
    url: &str,
    body: Option<String>,
) -> Result<(u16, String), String> {
    let client = job_client()?;
    let mut builder = hyper::Request::builder().method(method).uri(url);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let req = match body {
        Some(b) => builder
            .body(http_body_util::Full::new(Bytes::from(b)))
            .map_err(|e| format!("bad request: {e}"))?,
        None => builder
            .body(http_body_util::Full::new(Bytes::new()))
            .map_err(|e| format!("bad request: {e}"))?,
    };
    let resp = client
        .request(req)
        .await
        .map_err(|e| format!("job backend request failed: {e}"))?;
    let status = resp.status().as_u16();
    let collected = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .map_err(|e| format!("job backend response read failed: {e}"))?;
    let bytes = collected.to_bytes();
    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
}

/// The Vast.ai REST API root.
const VAST_API_BASE: &str = "https://console.vast.ai/api/v0";

fn vast_api_key() -> Result<String, String> {
    std::env::var("VAST_API_KEY").map_err(|_| {
        "VAST_API_KEY is not set; Vast.ai rental jobs need the operator API key in the environment"
            .to_owned()
    })
}

/// The `onstart` script Vast runs when the instance boots. It logs the boundaries of the job so
/// an operator can see in the Vast console that the container ran the requested command.
fn onstart_script(job: &JobRecord) -> String {
    let cmd = if job.spec.command.is_empty() {
        "echo started".to_owned()
    } else {
        job.spec.command.join(" ")
    };
    format!(
        "echo 'manarouter job {} started'; {}; echo 'manarouter job {} finished'",
        job.id, cmd, job.id
    )
}

/// Parse the Vast instance list into `(id, actual_status)` pairs.
fn parse_instances(body: &str) -> Result<Vec<(String, String)>, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| format!("Vast instances response is not JSON: {e}"))?;
    let arr = value
        .get("instances")
        .and_then(Value::as_array)
        .ok_or_else(|| "Vast instances response has no \"instances\" array".to_owned())?;
    let mut out = Vec::new();
    for item in arr {
        let id = item
            .get("id")
            .and_then(Value::as_i64)
            .map(|i| i.to_string());
        let status = item
            .get("actual_status")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let (Some(id), Some(status)) = (id, status) {
            out.push((id, status));
        }
    }
    Ok(out)
}

/// Rental backend against Vast.ai. Implemented against the published `console.vast.ai/api/v0`
/// contract; not yet verified against a live rental.
struct VastBackend;

fn vast_instance_id(job: &JobRecord) -> Result<String, String> {
    job.backend_detail
        .as_ref()
        .and_then(|d| d.get("instance_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "Vast job has no recorded instance id".to_owned())
}

#[async_trait]
impl JobBackend for VastBackend {
    fn id(&self) -> &'static str {
        "vast"
    }

    async fn launch(&self, job: &JobRecord) -> Result<LaunchOutcome, String> {
        let key = vast_api_key()?;
        let payload = json!({
            "api_key": key,
            "num_gpus": job.spec.gpu_count,
            "gpu_name": job.spec.gpu.clone().unwrap_or_else(|| "RTX4090".to_owned()),
            "image": job.spec.image,
            "disk_size": job.spec.disk_gb.unwrap_or(16),
            "onstart": onstart_script(job),
        });
        let (status, body) = job_http(
            "POST",
            &format!("{VAST_API_BASE}/instances/create/"),
            Some(payload.to_string()),
        )
        .await?;
        if status != 200 {
            return Err(format!(
                "Vast instance create returned HTTP {status}: {}",
                truncate(&body, 300)
            ));
        }
        let value: Value = serde_json::from_str(&body)
            .map_err(|e| format!("Vast create response is not JSON: {e}"))?;
        let instance = value
            .get("new_contract")
            .and_then(Value::as_i64)
            .ok_or_else(|| {
                format!(
                    "Vast create response had no new_contract: {}",
                    truncate(&body, 200)
                )
            })?;
        Ok(LaunchOutcome {
            backend_detail: json!({"instance_id": instance.to_string(), "provider": "vast.ai"}),
        })
    }

    async fn poll(&self, job: &JobRecord, _job_dir: &Path) -> Result<PollOutcome, String> {
        let key = vast_api_key()?;
        let instance_id = vast_instance_id(job)?;
        let url = format!("{VAST_API_BASE}/instances/?api_key={key}");
        let (status, body) = job_http("GET", &url, None).await?;
        if status != 200 {
            return Err(format!(
                "Vast instance list returned HTTP {status}: {}",
                truncate(&body, 300)
            ));
        }
        let instances = parse_instances(&body)?;
        let state = instances
            .iter()
            .find(|(id, _)| *id == instance_id)
            .map(|(_, s)| s.clone());
        let Some(state) = state else {
            return Ok(PollOutcome {
                status: JobStatus::Running,
                logs: vec!["instance not yet visible in the Vast list; still waiting".to_owned()],
                artifacts: vec![],
                error: None,
                backend_detail: json!({"instance_id": instance_id, "provider": "vast.ai"}),
            });
        };
        let (status, error) = match state.as_str() {
            "running" | "waiting" | "pending" => (JobStatus::Running, None),
            "stopped" => (JobStatus::Succeeded, None),
            "error" => (
                JobStatus::Failed,
                Some("Vast reported the instance in an error state".to_owned()),
            ),
            _ => (JobStatus::Running, None),
        };
        Ok(PollOutcome {
            status,
            logs: vec![format!("Vast actual_status = {state}")],
            artifacts: vec![],
            error,
            backend_detail: json!({"instance_id": instance_id, "provider": "vast.ai", "actual_status": state}),
        })
    }

    async fn cancel(&self, job: &JobRecord) -> Result<(), String> {
        let key = vast_api_key()?;
        let instance_id = vast_instance_id(job)?;
        let url = format!("{VAST_API_BASE}/instances/{instance_id}/?api_key={key}");
        let (status, body) = job_http("DELETE", &url, None).await?;
        if !(200..300).contains(&status) {
            return Err(format!(
                "Vast terminate returned HTTP {status}: {}",
                truncate(&body, 300)
            ));
        }
        Ok(())
    }
}

/// Query parameters for job listing.
#[derive(Debug, Deserialize)]
pub struct JobListParams {
    limit: Option<usize>,
    after: Option<String>,
}

/// `POST /v1/jobs`.
pub async fn create_job(
    State(app): State<AppState>,
    headers: HeaderMap,
    body: AxumBody,
) -> Response {
    if let Some(denied) = crate::server::authorize_batch_request(&app, &headers) {
        return denied;
    }
    let bytes = match crate::server::read_body(body, MAX_JOB_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(error) => return error.into_response(),
    };
    let spec: JobSpec = match serde_json::from_slice(&bytes) {
        Ok(spec) => spec,
        Err(error) => {
            return Refusal::bad_request(
                "invalid_request_error",
                format!("the request body is not valid JSON: {error}"),
            )
            .into_response();
        }
    };
    match app.jobs.submit(spec).await {
        Ok(view) => crate::server::json_response(StatusCode::OK, view),
        Err(refusal) => refusal.into_response(),
    }
}

/// `GET /v1/jobs`.
pub async fn list_jobs(
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<JobListParams>,
) -> Response {
    if let Some(denied) = crate::server::authorize_batch_request(&app, &headers) {
        return denied;
    }
    let limit = params.limit.unwrap_or(20).min(200);
    let jobs = app.jobs.list(limit, params.after.as_deref());
    crate::server::json_response(StatusCode::OK, json!({"jobs": jobs}))
}

/// `GET /v1/jobs/{id}`.
pub async fn retrieve_job(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
) -> Response {
    if let Some(denied) = crate::server::authorize_batch_request(&app, &headers) {
        return denied;
    }
    match app.jobs.view(&id) {
        Ok(view) => crate::server::json_response(StatusCode::OK, view),
        Err(refusal) => refusal.into_response(),
    }
}

/// `POST /v1/jobs/{id}/cancel`.
pub async fn cancel_job(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
) -> Response {
    if let Some(denied) = crate::server::authorize_batch_request(&app, &headers) {
        return denied;
    }
    match app.jobs.cancel(&id).await {
        Ok(view) => crate::server::json_response(StatusCode::OK, view),
        Err(refusal) => refusal.into_response(),
    }
}

/// `GET /v1/jobs/{id}/artifacts/{name}`.
pub async fn artifact_download(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath((id, name)): RoutePath<(String, String)>,
) -> Response {
    if let Some(denied) = crate::server::authorize_batch_request(&app, &headers) {
        return denied;
    }
    match app.jobs.artifact_bytes(&id, &name) {
        Ok((bytes, mime)) => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", mime)
            .body(AxumBody::from(Bytes::from(bytes)))
            .unwrap_or_else(|_| Refusal::internal("response construction failed").into_response()),
        Err(refusal) => refusal.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn lane() -> (TempDir, Arc<JobLane>) {
        let dir = TempDir::new().unwrap();
        let lane = Arc::new(JobLane::open(dir.path().join("jobs")));
        (dir, lane)
    }

    fn spec(ms: &str, command: Vec<String>) -> JobSpec {
        JobSpec {
            backend: "demo".to_string(),
            image: "busybox".to_string(),
            command,
            env: BTreeMap::from([("DEMO_MS".to_string(), ms.to_string())]),
            gpu: None,
            gpu_count: 1,
            disk_gb: None,
            max_seconds: 120,
        }
    }

    async fn wait_terminal(lane: &JobLane, id: &str) -> JobStatus {
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Ok(view) = lane.view(id) {
                let status: JobStatus =
                    serde_json::from_value(view.get("status").cloned().unwrap()).unwrap();
                if matches!(
                    status,
                    JobStatus::Succeeded | JobStatus::Failed | JobStatus::Cancelled
                ) {
                    return status;
                }
            }
        }
        panic!("job did not reach a terminal state");
    }

    #[tokio::test]
    async fn a_demo_job_succeeds_and_exposes_its_artifact() {
        let (_tmp, lane) = lane();
        let view = lane
            .submit(spec("50", vec!["echo".into(), "hello".into()]))
            .await
            .unwrap();
        let id = view.get("id").unwrap().as_str().unwrap().to_string();
        assert_eq!(wait_terminal(&lane, &id).await, JobStatus::Succeeded);
        let done = lane.view(&id).unwrap();
        assert_eq!(done.get("artifacts").unwrap(), &json!(["output.txt"]));
        let (bytes, mime) = lane.artifact_bytes(&id, "output.txt").unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(mime, "application/octet-stream");
        assert!(lane.artifact_bytes(&id, "../evil").is_err());
    }

    #[tokio::test]
    async fn a_demo_job_fails_when_the_command_asks_it_to() {
        let (_tmp, lane) = lane();
        let view = lane
            .submit(spec("50", vec!["run".into(), "fail".into()]))
            .await
            .unwrap();
        let id = view.get("id").unwrap().as_str().unwrap().to_string();
        assert_eq!(wait_terminal(&lane, &id).await, JobStatus::Failed);
        let done = lane.view(&id).unwrap();
        assert!(done
            .get("error")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("fail"));
    }

    #[tokio::test]
    async fn a_running_job_can_be_cancelled() {
        let (_tmp, lane) = lane();
        let view = lane
            .submit(spec("300000", vec!["render".into()]))
            .await
            .unwrap();
        let id = view.get("id").unwrap().as_str().unwrap().to_string();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let cancelled = lane.cancel(&id).await.unwrap();
        assert_eq!(cancelled.get("status").unwrap(), &json!("cancelled"));
        assert_eq!(wait_terminal(&lane, &id).await, JobStatus::Cancelled);
    }

    #[tokio::test]
    async fn unknown_backends_and_empty_images_are_refused() {
        let (_tmp, lane) = lane();
        let mut bad = spec("50", vec![]);
        bad.backend = "roboxes".to_string();
        assert!(lane.submit(bad).await.is_err());
        let mut bare = spec("50", vec![]);
        bare.image = "  ".to_string();
        assert!(lane.submit(bare).await.is_err());
    }

    #[test]
    fn vast_instance_list_parses_ids_and_statuses() {
        let body = r#"{"instances":[{"id":123,"actual_status":"running"},{"id":124,"actual_status":"stopped"}]}"#;
        let parsed = parse_instances(body).unwrap();
        assert_eq!(
            parsed,
            vec![
                ("123".to_string(), "running".to_string()),
                ("124".to_string(), "stopped".to_string())
            ]
        );
    }
}
