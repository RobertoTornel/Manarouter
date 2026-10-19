//! The measurement rig: a fake upstream, a relay pointed at it, and the two request paths.
//!
//! # What is being compared
//!
//! Two series of the *same* request:
//!
//! * **baseline** — straight to the fake upstream, over a pooled HTTP/1.1 connection, which is
//!   what the relay's own upstream client uses, so the two paths share the same transport and
//!   the difference is not a difference in connection handling;
//! * **relay** — the same request through the relay's router, driven in-process with
//!   `tower::util::ServiceExt::oneshot`, so there is no client-side TCP hop in the measurement.
//!
//! The added overhead of a sample is `relay − baseline`. That is the only number the budget is
//! about, because the provider call is not the relay's cost: it is the provider's.
//!
//! # What is deliberately absent
//!
//! No network beyond `127.0.0.1`, no real credential, no cost. The vault lives in a temporary
//! directory that is deleted when the run ends, and holds a synthetic string that no provider
//! would accept.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body as AxumBody;
use axum::routing::any;
use axum::Router;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tower::ServiceExt;

use manarouter_relay::select::Health;
use manarouter_relay::server::{router, AppState};
use manarouter_relay::state::RelayState;
use manarouter_relay::sticky::StickySuccess;
use manarouter_relay::upstream::Upstream;
use mr_core::dialect::{Dialect, InputModality};
use mr_core::model::{Account, ModelEntry, ModelSource, Provider};
use mr_core::secret::Vault;
use mr_core::snapshot::RoutingSnapshot;
use mr_core::store::Store;

use crate::error::{Error, Result};

/// The provider id in the generated configuration.
const PROVIDER_ID: &str = "bench";
/// The model id in the generated configuration.
const MODEL_ID: &str = "bench-model";
/// The name a client asks for: `provider/model`, as every Manarouter model is named.
const CLIENT_MODEL: &str = "bench/bench-model";
/// The vault entry the account's credential is stored under.
const SECRET_ID: &str = "bench-credential";
/// The synthetic credential. It is not a key to anything.
const SECRET_VALUE: &str = "sk-bench-not-a-real-key";
/// A marker from the canned completion, checked in every response so that a short circuit cannot
/// masquerade as a proxied answer.
const FAKE_MARKER: &[u8] = b"chatcmpl-bench";

/// The canned completion the fake upstream answers with.
const FAKE_COMPLETION: &str = r#"{"id":"chatcmpl-bench","object":"chat.completion","created":0,"model":"bench-model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;

/// The client used for the baseline series.
type BaselineClient = Client<HttpConnector, Full<Bytes>>;

/// The rig: a relay, a fake upstream and a baseline client, all local.
pub struct Harness {
    /// The relay's router, driven in-process.
    router: Router,
    /// The client that talks to the fake upstream directly.
    client: BaselineClient,
    /// The URL the baseline client posts to, which is the URL the relay posts to as well.
    upstream_url: String,
    /// The request body, identical for both series.
    request_body: String,
    /// How the relay's upstream pool was built, reported because the relay's own documentation
    /// says this is one of the first things to check when a request is mysteriously slow.
    pool_idle_secs: u64,
    /// How the relay's upstream pool was built.
    pool_max_idle_per_host: usize,
    /// The temporary directories, kept alive so they are removed when the run ends.
    _dirs: Vec<tempfile::TempDir>,
}

impl Harness {
    /// Build the rig: a fake upstream, a relay pointed at it, and a baseline client.
    pub async fn start() -> Result<Self> {
        let upstream_addr = start_fake_upstream().await?;
        let upstream_base = format!("http://{upstream_addr}");
        let upstream_url = format!("{upstream_base}/v1/chat/completions");

        // The configuration: one provider, one account, one model.
        let config = build_config(&upstream_base);

        // The snapshot the relay reads instead of interpreting the configuration per request.
        let snapshot = RoutingSnapshot::compile(&config)
            .map_err(|e| Error::new(format!("the routing snapshot did not compile: {e}")))?;

        let upstream = Upstream::from_latency(&config.latency)
            .map_err(|e| Error::new(format!("the upstream client did not build: {e}")))?;
        let (pool_idle, pool_max_idle_per_host) = upstream.pool_settings();
        let relay = Arc::new(RelayState::new(&config, snapshot, upstream));

        // A vault in a private temporary directory, holding the one credential the account names.
        let vault_dir = tempfile::tempdir()
            .map_err(|e| Error::new(format!("no temporary directory for the vault: {e}")))?;
        let mut vault = Vault::open(vault_dir.path())
            .map_err(|e| Error::new(format!("the vault did not open: {e}")))?;
        vault
            .put(SECRET_ID, SECRET_VALUE)
            .map_err(|e| Error::new(format!("the vault did not accept the credential: {e}")))?;

        let oauth_store = Arc::new(
            Store::open_in_memory()
                .map_err(|e| Error::new(format!("the metadata store did not open: {e}")))?,
        );

        let state = AppState {
            relay,
            no_key_required: true,
            health: Arc::new(Health::new()),
            vault: Arc::new(vault),
            oauth_refreshers: Arc::new(HashMap::new()),
            oauth_store: Arc::clone(&oauth_store),
            quota: Arc::new(manarouter_relay::quota::QuotaManager::new(oauth_store)),
            sticky_success: Arc::new(StickySuccess::default()),
        };

        let router = router(state);

        // The baseline client mirrors the relay's own upstream client: a pooled HTTP/1.1
        // connection with Nagle disabled, so the two paths differ only by the relay.
        let mut connector = HttpConnector::new();
        connector.set_nodelay(true);
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(config.latency.pool_idle_secs))
            .pool_max_idle_per_host(config.latency.pool_max_idle_per_host)
            .build(connector);

        let request_body = serde_json::json!({
            "model": CLIENT_MODEL,
            "messages": [{ "role": "user", "content": "ping" }],
        })
        .to_string();

        Ok(Self {
            router,
            client,
            upstream_url,
            request_body,
            pool_idle_secs: pool_idle.as_secs(),
            pool_max_idle_per_host,
            _dirs: vec![vault_dir],
        })
    }

    /// The URL both series post to.
    pub fn upstream_url(&self) -> &str {
        &self.upstream_url
    }

    /// The relay's upstream pool settings: the idle timeout in seconds and the number of idle
    /// connections kept per host.
    pub fn pool_settings(&self) -> (u64, usize) {
        (self.pool_idle_secs, self.pool_max_idle_per_host)
    }

    /// One request straight to the fake upstream, timed end to end.
    pub async fn measure_baseline(&self) -> Result<f64> {
        let request = http::Request::builder()
            .method(http::Method::POST)
            .uri(&self.upstream_url)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(self.request_body.clone())))
            .map_err(|e| Error::new(format!("the baseline request did not build: {e}")))?;

        let started = Instant::now();
        let response = self
            .client
            .request(request)
            .await
            .map_err(|e| Error::new(format!("the baseline request failed: {e}")))?;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| Error::new(format!("the baseline response failed: {e}")))?;
        let elapsed = started.elapsed();

        if !status.is_success() {
            return Err(Error::new(format!(
                "the fake upstream answered {status} to the baseline request: {}",
                String::from_utf8_lossy(&body.to_bytes())
            )));
        }
        if !body
            .to_bytes()
            .windows(FAKE_MARKER.len())
            .any(|w| w == FAKE_MARKER)
        {
            return Err(Error::new(
                "the fake upstream did not answer with its canned completion",
            ));
        }
        Ok(micros(elapsed))
    }

    /// One request through the relay's router, timed end to end.
    ///
    /// The router is driven in-process: the request never crosses a socket to a client, so what
    /// is measured is the relay's own work and not the client's.
    pub async fn measure_relay(&self) -> Result<f64> {
        let request = axum::http::Request::builder()
            .method(axum::http::Method::POST)
            .uri("/v1/chat/completions")
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(AxumBody::from(self.request_body.clone()))
            .map_err(|e| Error::new(format!("the relay request did not build: {e}")))?;

        let started = Instant::now();
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .map_err(|e| Error::new(format!("the relay request failed: {e}")))?;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| Error::new(format!("the relay response failed: {e}")))?;
        let elapsed = started.elapsed();

        if !status.is_success() {
            return Err(Error::new(format!(
                "the relay answered {status}: {}",
                String::from_utf8_lossy(&body.to_bytes())
            )));
        }
        // The relay must have proxied the upstream's answer, not answered on its own: a short
        // circuit would still be a 200, and would measure nothing.
        if !body
            .to_bytes()
            .windows(FAKE_MARKER.len())
            .any(|w| w == FAKE_MARKER)
        {
            return Err(Error::new(
                "the relay did not return the fake upstream's completion",
            ));
        }
        Ok(micros(elapsed))
    }
}

/// The measurement: both series, and the difference between them.
#[derive(Debug, Clone, Default)]
pub struct Measurement {
    /// The baseline series, in microseconds.
    pub baseline_us: Vec<f64>,
    /// The relay series, in microseconds.
    pub relay_us: Vec<f64>,
    /// `relay − baseline` per sample, in microseconds.
    pub added_us: Vec<f64>,
}

/// Run the measurement.
///
/// The two series are interleaved and their order within a sample is randomised, so that a drift
/// in machine load — a background build, a thermal excursion — cannot systematically favour one
/// path over the other. The warmup samples are measured exactly the same way and then thrown
/// away: the first request through a fresh connection pays for the connection, and that cost
/// belongs to the setup, not to the relay.
pub async fn measure(harness: &Harness, warmups: usize, samples: usize) -> Result<Measurement> {
    let mut rng = Rng::seeded();

    for _ in 0..warmups {
        if rng.next_bool() {
            harness.measure_baseline().await?;
            harness.measure_relay().await?;
        } else {
            harness.measure_relay().await?;
            harness.measure_baseline().await?;
        }
    }

    let mut measurement = Measurement::default();
    for _ in 0..samples {
        let (baseline, relay) = if rng.next_bool() {
            let baseline = harness.measure_baseline().await?;
            let relay = harness.measure_relay().await?;
            (baseline, relay)
        } else {
            let relay = harness.measure_relay().await?;
            let baseline = harness.measure_baseline().await?;
            (baseline, relay)
        };
        measurement.baseline_us.push(baseline);
        measurement.relay_us.push(relay);
        measurement.added_us.push(relay - baseline);
    }

    Ok(measurement)
}

/// A configuration with exactly one provider, one account and one model.
fn build_config(upstream_base: &str) -> mr_core::config::Config {
    let mut config = mr_core::config::Config {
        // The bench has no client keys, so the relay is asked to accept an unauthenticated request.
        no_key_required: true,
        ..Default::default()
    };

    let mut provider = Provider::new(PROVIDER_ID, Dialect::OpenAi, upstream_base);
    let mut account = Account::new("bench-account");
    // The credential is never in the configuration; it is in the vault, under this reference.
    account.secret_ref = Some(SECRET_ID.to_owned());
    provider.accounts = vec![account];
    provider.models = vec![ModelEntry {
        id: MODEL_ID.to_owned(),
        source: ModelSource::Manual,
        label: None,
        context_length: None,
        max_output_tokens: None,
        capabilities: Vec::new(),
        input_modalities: vec![InputModality::Text],
        reasoning_levels: Vec::new(),
        default_reasoning_level: None,
        reasoning_wire: None,
        input_price_per_mtok: None,
        output_price_per_mtok: None,
        pricing: None,
        synced_at: None,
        hidden: false,
    }];
    config.providers = vec![provider];
    config
}

/// Start the fake upstream on a loopback port and return its address.
///
/// It is a real HTTP server, because the relay's upstream client speaks real HTTP: a stubbed
/// transport would measure a different relay.
async fn start_fake_upstream() -> Result<SocketAddr> {
    let app = Router::new().fallback(any(|| async {
        (
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            FAKE_COMPLETION,
        )
    }));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| Error::new(format!("the fake upstream did not bind: {e}")))?;
    let addr = listener
        .local_addr()
        .map_err(|e| Error::new(format!("the fake upstream has no address: {e}")))?;

    tokio::spawn(async move {
        // The server runs for the lifetime of the process; the task ends when the runtime does.
        let _ = axum::serve(listener, app).await;
    });

    Ok(addr)
}

/// A duration in whole microseconds.
fn micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000_000.0
}

/// A tiny xorshift generator, used only to decide which path goes first in a sample.
///
/// It is seeded from the clock, so two runs do not share an order, and it is not a cryptographic
/// generator because it is deciding the order of two measurements.
struct Rng(u64);

impl Rng {
    /// Seed from the wall clock, mixed so that two runs in the same nanosecond still differ.
    fn seeded() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x2545_F491_4F6C_DD1D);
        Self(nanos ^ 0x9E37_79B9_7F4A_7C15)
    }

    /// The next value.
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A coin toss.
    fn next_bool(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }
}
