//! `mr jobs` — submit and drive custom GPU jobs against the relay's job lane (`/v1/jobs`).
//!
//! The lane (`crates/manarouter-relay/src/jobs.rs`, documented in `docs/27-gpu-jobs.md`) rents a
//! card — or runs the local `demo` simulator — for a container image and a command. This module
//! is the agent-facing CLI: one command tree, the same client key that calls a model, and JSON
//! that round-trips the relay's own view.
//!
//! Design notes:
//!
//! * **Auth is the client key, and it never touches disk here.** The key comes from
//!   `MR_CLIENT_KEY`, or stdin (`mr jobs … --key-stdin` styling is deliberately absent: a key
//!   on the command line lands in shell history, so the environment or a pipe is the way).
//! * **`wait` is a first-class subcommand**, not a flag: an agent babysitting a render for an
//!   hour wants `mr jobs wait <id> --timeout 1h`, with the terminal state carried by the exit
//!   code and the final view on stdout, not a hand-rolled poll loop.
//! * **Tables for humans, JSON for agents.** `--json` prints the relay's response verbatim so an
//!   agent can parse exactly what the lane said; the fallback human view is the same document.
//! * The HTTP stack is the same bounded hyper/rustls client `provider test` uses.

use std::time::Duration;

use clap::Subcommand;
use serde_json::{json, Value};

use crate::output::{say, CliError, Exit};
use crate::provider::net;
use crate::Ctx;

/// How long one HTTP call to the relay may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How often `wait` polls while a job runs.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Subcommand, Debug)]
pub enum JobsCmd {
    /// Submit a custom containerized job to a rental GPU (or the `demo` simulator).
    Submit {
        /// Job backend: `demo` (local simulator, no cost) or `vast` (Vast.ai rental).
        #[arg(long)]
        backend: String,
        /// Container image to run, e.g. `python:3.12-slim`.
        #[arg(long)]
        image: String,
        /// Command to run inside the container (repeatable; one quoted string splits on spaces).
        #[arg(long, value_name = "ARG")]
        command: Vec<String>,
        /// Environment variables as NAME=VALUE (repeatable).
        #[arg(long = "env", value_name = "NAME=VALUE")]
        env: Vec<String>,
        /// GPU model to request (e.g. `RTX4090`); backend-dependent.
        #[arg(long)]
        gpu: Option<String>,
        /// How many GPUs (1-8).
        #[arg(long, default_value_t = 1)]
        gpu_count: u32,
        /// Disk to request, in GB.
        #[arg(long, default_value_t = 16)]
        disk_gb: u32,
        /// Kill the job after this many seconds (a card bills whether it progresses or not).
        #[arg(long, default_value_t = 3600)]
        max_seconds: u64,
        /// Read the job body from a JSON file instead of the flags above.
        #[arg(long, value_name = "FILE")]
        body: Option<String>,
    },
    /// List jobs, newest first.
    List {
        /// How many to show.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Only jobs submitted after this job id (cursor from a previous page).
        #[arg(long)]
        after: Option<String>,
    },
    /// Show one job: status, logs, artifacts.
    Show { id: String },
    /// Cancel a queued or running job.
    Cancel { id: String },
    /// Poll a job until it reaches a terminal state, then print the final view.
    Wait {
        id: String,
        /// Give up after this long (`30s`, `10m`, `1h`...); the job itself keeps running.
        #[arg(long, default_value = "10m")]
        timeout: String,
    },
}

/// Run a jobs subcommand.
pub async fn run(cmd: JobsCmd, ctx: &Ctx) -> Result<(), CliError> {
    match cmd {
            JobsCmd::Submit {
                backend,
                image,
                command,
                env,
                gpu,
                gpu_count,
                disk_gb,
                max_seconds,
                body,
            } => {
                let payload = build_body(
                    backend.as_str(),
                    image.as_str(),
                    &command,
                    &env,
                    gpu.as_deref(),
                    gpu_count,
                    disk_gb,
                    max_seconds,
                    body.as_deref(),
                )?;
                let view = request(ctx, "POST", "/v1/jobs", Some(&payload)).await?;
                report(ctx, &view);
                Ok(())
            }
            JobsCmd::List { limit, after } => {
                let mut path = format!("/v1/jobs?limit={limit}");
                if let Some(after) = after {
                    path.push_str(&format!("&after={}", net::url_encode(after.as_str())));
                }
                let view = request(ctx, "GET", &path, None).await?;
                report(ctx, &view);
                Ok(())
            }
            JobsCmd::Show { id } => {
                let view = request(ctx, "GET", &format!("/v1/jobs/{id}"), None).await?;
                report(ctx, &view);
                Ok(())
            }
            JobsCmd::Cancel { id } => {
                let view = request(ctx, "POST", &format!("/v1/jobs/{id}/cancel"), None).await?;
                report(ctx, &view);
                Ok(())
            }
            JobsCmd::Wait { id, timeout } => wait(ctx, id.as_str(), timeout.as_str()).await,
    }
}

/// Build the submit body from flags, or read it from a JSON file with `--body`.
///
/// `--command "python render.py"` (one quoted flag) splits on whitespace the way a shell would
/// have; explicit repeats stay verbatim, so a path with spaces survives via repeated flags.
fn build_body(
    backend: &str,
    image: &str,
    command: &[String],
    env: &[String],
    gpu: Option<&str>,
    gpu_count: u32,
    disk_gb: u32,
    max_seconds: u64,
    body_file: Option<&str>,
) -> Result<String, CliError> {
    if let Some(path) = body_file {
        let text = std::fs::read_to_string(path)
            .map_err(|e| CliError::failure(format!("cannot read {path}: {e}")))?;
        serde_json::from_str::<Value>(&text)
            .map_err(|e| CliError::usage(format!("{path} is not valid JSON: {e}")))?;
        return Ok(text);
    }
    let mut cmd: Vec<String> = command.to_vec();
    if cmd.len() == 1 && cmd[0].contains(' ') {
        cmd = cmd[0].split_whitespace().map(str::to_owned).collect();
    }
    let mut object = json!({
        "backend": backend,
        "image": image,
        "command": cmd,
    });
    if !env.is_empty() {
        let mut map = serde_json::Map::new();
        for entry in env {
            let Some((name, value)) = entry.split_once('=') else {
                return Err(CliError::usage(format!(
                    "--env expects NAME=VALUE, got `{entry}`"
                )));
            };
            map.insert(name.to_owned(), Value::String(value.to_owned()));
        }
        object["env"] = Value::Object(map);
    }
    if let Some(gpu) = gpu {
        object["gpu"] = Value::String(gpu.to_owned());
    }
    object["gpu_count"] = json!(gpu_count);
    object["disk_gb"] = json!(disk_gb);
    object["max_seconds"] = json!(max_seconds);
    Ok(object.to_string())
}

/// A terminal job state: `wait` stops here.
fn terminal(status: &str) -> bool {
    matches!(status, "succeeded" | "failed" | "cancelled")
}

/// Poll until the job is done or the budget is spent.
///
/// A failed or cancelled job is not a failure of the request — the request worked — but an agent
/// scripting `mr jobs wait … && next-step` needs the distinction, so the exit code carries it
/// and the final view is still printed.
async fn wait(ctx: &Ctx, id: &str, timeout: &str) -> Result<(), CliError> {
    let budget = humantime::parse_duration(timeout)
        .map_err(|_| CliError::usage(format!("--timeout is not a duration: {timeout}")))?;
    let started = std::time::Instant::now();
    loop {
        let view = request(ctx, "GET", &format!("/v1/jobs/{id}"), None).await?;
        let status = view
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        if terminal(&status) {
            report(ctx, &view);
            if status == "succeeded" {
                return Ok(());
            }
            return Err(CliError {
                exit: Exit::Failure,
                message: format!("job {id} ended `{status}`"),
                details: Vec::new(),
            });
        }
        if started.elapsed() >= budget {
            return Err(CliError::failure(format!(
                "job {id} is still `{status}` after {timeout}"
            )));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// The relay origin job requests go to: the configured relay listener.
///
/// TLS is not attempted here: the relay's own surface is the loopback data plane, and a remote
/// data plane is a deliberate deployment the operator reaches with their own tooling.
fn relay_base(ctx: &Ctx) -> Result<String, CliError> {
    let cfg = ctx.load()?;
    Ok(format!("http://{}:{}", cfg.relay.host, cfg.relay.port))
}

/// The client key: the `MR_CLIENT_KEY` environment variable, else stdin.
///
/// A key on the command line would land in shell history and `ps`, the same reason
/// `mr account add --secret` is discouraged; the pipe (`mr jobs list <<< "$KEY"` or
/// `echo "$KEY" | mr jobs list`) is the documented shape.
fn client_key() -> Result<String, CliError> {
    if let Some(key) = std::env::var_os("MR_CLIENT_KEY") {
        let key = key.to_string_lossy().trim().to_owned();
        if !key.is_empty() {
            return Ok(key);
        }
    }
    let mut text = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
        .map_err(|e| CliError::failure(format!("cannot read the client key from stdin: {e}")))?;
    let key = text.trim().to_owned();
    if key.is_empty() {
        return Err(CliError::usage(
            "no client key: set MR_CLIENT_KEY, or pipe one on stdin",
        ));
    }
    Ok(key)
}

/// One authenticated call to the relay's job lane; the reply is parsed as the relay's JSON view.
async fn request(
    ctx: &Ctx,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Result<Value, CliError> {
    let base = relay_base(ctx)?;
    let url = format!("{base}{path}");
    let client = net::client();
    let headers = vec![(
        "authorization".to_owned(),
        format!("Bearer {}", client_key()?),
    )];
    let method = http::Method::from_bytes(method.as_bytes())
        .map_err(|_| CliError::usage(format!("not an HTTP method: {method}")))?;
    let reply = net::request(
        &client,
        method,
        &url,
        &headers,
        body.map(str::to_owned),
        REQUEST_TIMEOUT,
    )
    .await?;
    if !reply.status.is_success() {
        let mut e = CliError::failure(format!(
            "relay answered HTTP {} for {path}",
            reply.status.as_u16()
        ));
        let text: String = crate::output::redact_text(&reply.body).chars().take(300).collect();
        if !text.trim().is_empty() {
            e = e.with_detail(text);
        }
        if matches!(reply.status.as_u16(), 401 | 403) {
            e = e.with_detail(
                "the client key was rejected; jobs use the same key that calls a model".to_owned(),
            );
        }
        return Err(e);
    }
    serde_json::from_str(&reply.body)
        .map_err(|e| CliError::failure(format!("the relay's reply for {path} is not JSON: {e}")))
}

/// Human output is the same document the agent gets, indented: one renderer, no drift.
fn report(ctx: &Ctx, view: &Value) {
    if ctx.json {
        let _ = crate::output::print_json(view);
        return;
    }
    say(serde_json::to_string_pretty(view).unwrap_or_else(|_| view.to_string()));
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_quoted_command_splits_like_a_shell() {
        let body = build_body(
            "demo",
            "python:3.12-slim",
            &["python render.py".to_owned()],
            &[],
            None,
            1,
            16,
            60,
            None,
        )
        .unwrap();
        let value: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            value["command"],
            json!(["python", "render.py"]),
            "one quoted flag splits on whitespace"
        );
    }

    #[test]
    fn repeated_flags_stay_verbatim() {
        let body = build_body(
            "demo",
            "img",
            &["python".to_owned(), "my script.py".to_owned()],
            &["A=1".to_owned(), "B=two words".to_owned()],
            Some("RTX4090"),
            2,
            32,
            120,
            None,
        )
        .unwrap();
        let value: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["command"], json!(["python", "my script.py"]));
        assert_eq!(value["env"], json!({"A": "1", "B": "two words"}));
        assert_eq!(value["gpu"], json!("RTX4090"));
        assert_eq!(value["gpu_count"], json!(2));
        assert_eq!(value["disk_gb"], json!(32));
        assert_eq!(value["max_seconds"], json!(120));
    }

    #[test]
    fn env_without_equals_is_a_usage_error() {
        let err = build_body("demo", "img", &[], &["BROKEN".to_owned()], None, 1, 16, 60, None)
            .expect_err("NAME=VALUE is required");
        assert_eq!(err.exit, Exit::Usage);
    }

    #[test]
    fn body_file_wins_and_is_passed_through_parsed_and_rejected() {
        // A well-formed file is handed to the relay verbatim.
        let path = std::env::temp_dir().join(format!("mr-jobs-body-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"backend":"demo","image":"i"}"#).unwrap();
        let body = build_body("demo", "img", &[], &[], None, 1, 16, 60, Some(path.to_str().unwrap()))
            .unwrap();
        let value: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["backend"], json!("demo"));
        std::fs::remove_file(&path).ok();
        // A malformed file is a usage error, not a submission.
        let bad = std::env::temp_dir().join(format!("mr-jobs-bad-{}.json", std::process::id()));
        std::fs::write(&bad, "not json").unwrap();
        let err = build_body("demo", "img", &[], &[], None, 1, 16, 60, Some(bad.to_str().unwrap()))
            .expect_err("invalid JSON refuses");
        assert_eq!(err.exit, Exit::Usage);
        std::fs::remove_file(&bad).ok();
    }

    #[test]
    fn terminal_states_are_exactly_the_finished_ones() {
        assert!(terminal("succeeded"));
        assert!(terminal("failed"));
        assert!(terminal("cancelled"));
        assert!(!terminal("running"));
        assert!(!terminal("queued"));
        assert!(!terminal("unknown"));
    }
}
