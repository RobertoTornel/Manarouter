//! `mr doctor` — read-only checks for the local installation.
//!
//! This command deliberately avoids opening `Vault`: `Vault::open` creates a directory and may
//! create a key on first use. Doctor inspects only paths and metadata, and decodes existing routing
//! snapshots without repairing them. It never contacts providers or prints credential material.

use std::net::IpAddr;
use std::path::Path;

use crate::output::{print_json, say, CliError};
use crate::Ctx;
use mr_core::config::Config;
use mr_core::snapshot::RoutingSnapshot;
use serde::Serialize;

#[derive(Debug, Serialize)]
struct Check {
    name: &'static str,
    status: &'static str,
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl Check {
    fn new(name: &'static str, status: &'static str, summary: impl Into<String>) -> Self {
        Self {
            name,
            status,
            summary: summary.into(),
            detail: None,
        }
    }

    fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    status: &'static str,
    config: Check,
    vault: Check,
    snapshot: Check,
    client_keys: Check,
    listeners: Vec<Check>,
}

impl DoctorReport {
    fn has_failures(&self) -> bool {
        [&self.config, &self.vault, &self.snapshot, &self.client_keys]
            .iter()
            .any(|check| check.status == "fail")
            || self.listeners.iter().any(|check| check.status == "fail")
    }
}

/// Run `mr doctor`. Read-only: no bind probes, vault opening, provider calls, or file repairs.
pub async fn run(ctx: &Ctx) -> Result<(), CliError> {
    let config_dir = ctx.config_dir();
    let config_path = ctx.config_path();
    let state_dir = mr_core::config::state_dir();
    let snapshot_path = mr_core::config::snapshot_path();
    // --config-dir intentionally does not imply a state directory. The conductor/runtime resolve
    // these independently; expose both effective paths instead of guessing a parallel XDG layout.
    let report = inspect(&config_path, &config_dir, &snapshot_path, &state_dir);
    let failed = report.has_failures();

    if ctx.json {
        print_json(&report)?;
    } else {
        say(format!(
            "Manarouter doctor: {}",
            report.status.to_ascii_uppercase()
        ));
        for check in [
            &report.config,
            &report.vault,
            &report.snapshot,
            &report.client_keys,
        ]
        .into_iter()
        .chain(report.listeners.iter())
        {
            say(format!(
                "{}: {} — {}",
                check.status.to_ascii_uppercase(),
                check.name,
                check.summary
            ));
            if let Some(detail) = &check.detail {
                say(format!("  {detail}"));
            }
        }
    }

    if failed {
        Err(CliError::validation(
            "doctor found one or more installation problems",
        ))
    } else {
        Ok(())
    }
}

fn inspect(
    config_path: &Path,
    config_dir: &Path,
    snapshot_path: &Path,
    state_dir: &Path,
) -> DoctorReport {
    let (config_check, config) = match Config::load(config_path) {
        Ok(config) => {
            let issues = config.validate();
            if issues.is_empty() {
                let state = if config_path.is_file() { "ok" } else { "warn" };
                let summary = if config_path.is_file() {
                    "configuration parsed and validation passed".to_owned()
                } else {
                    "configuration file is absent; using first-run defaults".to_owned()
                };
                (
                    Check::new("configuration", state, summary)
                        .detail(config_path.display().to_string()),
                    Some(config),
                )
            } else {
                (
                    Check::new(
                        "configuration",
                        "fail",
                        "configuration validation reported problems",
                    )
                    .detail(issues.join("; ")),
                    Some(config),
                )
            }
        }
        Err(_error) => (
            Check::new("configuration", "fail", "configuration could not be loaded").detail(
                format!(
                "read or parse failed for {} (details omitted to avoid displaying config contents)",
                config_path.display()
            ),
            ),
            None,
        ),
    };

    let vault_check = inspect_vault(config_dir);
    let snapshot_check = inspect_snapshot(snapshot_path, state_dir, config.as_ref());
    let (client_keys, listeners) = config.as_ref().map_or_else(
        || (Check::new("client keys", "warn", "not checked because configuration did not load"), Vec::new()),
        |config| {
            let enabled = config
                .client_keys
                .iter()
                .filter(|key| key.enabled && valid_key_hash(&key.key_hash))
                .count();
            let exposed = !matches!(classify_host(&config.relay.host), HostExposure::Loopback);
            let keys = if enabled > 0 {
                Check::new("client keys", "ok", format!("{enabled} enabled client key(s) configured"))
            } else if exposed {
                Check::new("client keys", "fail", "no enabled client keys are configured while the relay is not confirmed loopback-only; anonymous access may be possible. No key was created.")
            } else {
                Check::new("client keys", "warn", "no enabled client keys are configured; relay requests are unauthenticated on loopback. No key was created.")
            };
            (keys, listener_checks(config))
        },
    );

    let mut report = DoctorReport {
        status: "ok",
        config: config_check,
        vault: vault_check,
        snapshot: snapshot_check,
        client_keys,
        listeners,
    };
    if report.has_failures() {
        report.status = "fail";
    } else if [
        &report.config,
        &report.vault,
        &report.snapshot,
        &report.client_keys,
    ]
    .into_iter()
    .chain(report.listeners.iter())
    .any(|check| check.status == "warn")
    {
        report.status = "warn";
    }
    report
}

fn inspect_vault(config_dir: &Path) -> Check {
    let key_path = config_dir.join("vault.key");
    let encrypted_path = config_dir.join("vault.enc");
    let (key_status, key_detail) = match std::fs::metadata(&key_path) {
        Ok(metadata) => {
            let valid_length = metadata.len() == 32;
            #[cfg(unix)]
            let private_permissions = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o077 == 0
            };
            #[cfg(not(unix))]
            let private_permissions = true;
            let status = if valid_length && private_permissions {
                "ok"
            } else {
                "fail"
            };
            let detail = format!(
                "key file present; 32-byte length {}; restrictive permissions {}",
                if valid_length { "valid" } else { "invalid" },
                if private_permissions {
                    "verified"
                } else {
                    "not verified"
                }
            );
            (status, detail)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (
            if encrypted_path.is_file() {
                "fail"
            } else {
                "warn"
            },
            if encrypted_path.is_file() {
                "vault key file is absent while encrypted vault data exists; restore the matching key from backup before using stored credentials".to_owned()
            } else {
                "vault key file is absent and no encrypted data was found; first-use setup may create one".to_owned()
            },
        ),
        Err(error) => (
            "fail",
            format!("cannot inspect vault key metadata: {error}"),
        ),
    };
    let (data_status, data_detail) = match std::fs::metadata(&encrypted_path) {
        Ok(metadata) if metadata.is_file() => ("ok", "encrypted vault file is present".to_owned()),
        Ok(_) => ("fail", "vault data path is not a regular file".to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (
            "warn",
            "encrypted vault file is absent; no stored credentials detected".to_owned(),
        ),
        Err(error) => (
            "fail",
            format!("cannot inspect encrypted vault metadata: {error}"),
        ),
    };
    let status = if key_status == "fail" || data_status == "fail" {
        "fail"
    } else if key_status == "warn" || data_status == "warn" {
        "warn"
    } else {
        "ok"
    };
    Check::new(
        "vault",
        status,
        "read-only metadata check; credential contents were not opened",
    )
    .detail(format!(
        "{}; {}; paths: {} and {}",
        key_detail,
        data_detail,
        key_path.display(),
        encrypted_path.display()
    ))
}

fn inspect_snapshot(snapshot_path: &Path, state_dir: &Path, config: Option<&Config>) -> Check {
    match RoutingSnapshot::load_from(snapshot_path) {
        Ok(snapshot) => match config {
            Some(config) if snapshot.digest() == config.routing_digest() => Check::new("routing snapshot", "ok", "snapshot parses and matches current configuration digest")
                .detail(snapshot_path.display().to_string()),
            Some(config) => Check::new("routing snapshot", "warn", "snapshot parses but is stale relative to current configuration")
                .detail(format!("snapshot digest {:#x}; config digest {:#x}; {}", snapshot.digest(), config.routing_digest(), snapshot_path.display())),
            None => Check::new("routing snapshot", "warn", "snapshot parses; freshness could not be checked because configuration did not load")
                .detail(snapshot_path.display().to_string()),
        },
        Err(mr_core::Error::NotFound(_)) => Check::new("routing snapshot", "warn", "snapshot is absent; the conductor may not have compiled configuration yet")
            .detail(snapshot_path.display().to_string()),
        Err(_error) => Check::new("routing snapshot", "fail", "snapshot is unreadable or invalid")
            .detail(format!(
                "snapshot decode/read failed; state directory {}",
                state_dir.display()
            )),

    }
}

fn valid_key_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn listener_checks(config: &Config) -> Vec<Check> {
    vec![
        listener_check(
            "relay listener",
            &config.relay.host,
            config.relay.port,
            false,
        ),
        listener_check(
            "control/panel listener",
            &config.control.host,
            config.control.port,
            config.control.password_hash.is_some() || config.control.allow_remote_without_password,
        ),
    ]
}

fn listener_check(
    name: &'static str,
    host: &str,
    port: u16,
    remote_policy_explicit: bool,
) -> Check {
    match classify_host(host) {
        HostExposure::Loopback => Check::new(name, "ok", format!("binds to loopback {host}:{port}; not directly exposed to the LAN")),
        HostExposure::Wildcard => {
            let status = if name == "control/panel listener" && !remote_policy_explicit { "fail" } else { "warn" };
            let advice = if name == "control/panel listener" && !remote_policy_explicit { "panel binds broadly without a password/explicit remote opt-out" } else { "listener binds on all interfaces; check firewall and authentication" };
            Check::new(name, status, format!("{advice} ({host}:{port})"))
        }
        HostExposure::NonLoopback => Check::new(name, "warn", format!("binds to non-loopback address {host}:{port}; remote/LAN exposure is possible")),
        HostExposure::Unknown => Check::new(name, "warn", format!("host {host:?} is not a literal IP or localhost; exposure cannot be determined without DNS resolution")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostExposure {
    Loopback,
    Wildcard,
    NonLoopback,
    Unknown,
}

fn classify_host(host: &str) -> HostExposure {
    if host.eq_ignore_ascii_case("localhost") {
        return HostExposure::Loopback;
    }
    match host.parse::<IpAddr>() {
        Ok(ip) if ip.is_loopback() => HostExposure::Loopback,
        Ok(ip) if ip.is_unspecified() => HostExposure::Wildcard,
        Ok(_) => HostExposure::NonLoopback,
        Err(_) => HostExposure::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::testutil::temp_dir;

    #[test]
    fn reports_missing_config_vault_and_snapshot_without_creating_files() {
        let root = temp_dir("doctor-readonly");
        std::fs::create_dir_all(&root).unwrap();
        let config_dir = root.join("config");
        let state_dir = root.join("state");
        let report = inspect(
            &config_dir.join("config.json"),
            &config_dir,
            &state_dir.join("routes.snapshot"),
            &state_dir,
        );
        assert_eq!(report.config.status, "warn");
        assert_eq!(report.snapshot.status, "warn");
        assert_eq!(report.vault.status, "warn");
        assert_eq!(report.client_keys.status, "warn");
        assert!(
            !config_dir.exists(),
            "doctor must not create the config dir or vault key"
        );
        assert!(
            !state_dir.exists(),
            "doctor must not create the snapshot dir"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reports_current_and_stale_snapshots_and_missing_key_warning() {
        let root = temp_dir("doctor-snapshot");
        std::fs::create_dir_all(&root).unwrap();
        let cfg_path = root.join("config.json");
        let cfg = Config::default();
        cfg.save(&cfg_path).unwrap();
        let snapshot_path = root.join("routes.snapshot");
        RoutingSnapshot::compile(&cfg)
            .unwrap()
            .write_to(&snapshot_path)
            .unwrap();
        let report = inspect(&cfg_path, &root, &snapshot_path, &root);
        assert_eq!(report.snapshot.status, "ok");
        assert_eq!(report.client_keys.status, "warn");

        let mut changed = cfg.clone();
        changed.providers.push(mr_core::model::Provider::new(
            "doctor-test-provider",
            mr_core::dialect::Dialect::OpenAi,
            "http://127.0.0.1:9000/v1",
        ));
        changed.save(&cfg_path).unwrap();
        let stale = inspect(&cfg_path, &root, &snapshot_path, &root);
        assert_eq!(stale.snapshot.status, "warn");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn detects_corrupt_snapshot_and_never_prints_credentials() {
        let root = temp_dir("doctor-corrupt");
        std::fs::create_dir_all(&root).unwrap();
        let cfg = Config::default();
        let cfg_path = root.join("config.json");
        cfg.save(&cfg_path).unwrap();
        let snapshot_path = root.join("routes.snapshot");
        std::fs::write(&snapshot_path, b"secret-like-payload").unwrap();
        let report = inspect(&cfg_path, &root, &snapshot_path, &root);
        assert_eq!(report.snapshot.status, "fail");
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("secret-like-payload"));
        assert!(!json.contains("key_hash"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn listener_exposure_classifies_common_binds_conservatively() {
        assert!(valid_key_hash(&"ab".repeat(32)));
        assert!(!valid_key_hash("not-a-hash"));
        assert_eq!(classify_host("127.0.0.1"), HostExposure::Loopback);
        assert_eq!(classify_host("::1"), HostExposure::Loopback);
        assert_eq!(classify_host("localhost"), HostExposure::Loopback);
        assert_eq!(classify_host("0.0.0.0"), HostExposure::Wildcard);
        assert_eq!(classify_host("::"), HostExposure::Wildcard);
        assert_eq!(classify_host("192.168.1.4"), HostExposure::NonLoopback);
        assert_eq!(classify_host("router.local"), HostExposure::Unknown);
    }

    #[test]
    fn no_client_keys_on_non_loopback_relay_is_a_failure() {
        let root = temp_dir("doctor-exposed-relay");
        std::fs::create_dir_all(&root).unwrap();
        let mut config = Config::default();
        config.relay.host = "0.0.0.0".to_owned();
        let path = root.join("config.json");
        config.save(&path).unwrap();
        let report = inspect(&path, &root, &root.join("routes.snapshot"), &root);
        assert_eq!(report.client_keys.status, "fail");
        assert!(report.has_failures());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_vault_key_with_encrypted_data_is_a_failure() {
        let root = temp_dir("doctor-missing-vault-key");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("vault.enc"), "{}").unwrap();
        let check = inspect_vault(&root);
        assert_eq!(check.status, "fail");
        assert!(check.summary.contains("metadata"));
        let detail = check.detail.unwrap();
        assert!(detail.contains("restore the matching key"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_config_is_a_failure_without_attempting_repairs() {
        let root = temp_dir("doctor-invalid-config");
        std::fs::create_dir_all(&root).unwrap();
        let cfg_path = root.join("config.json");
        std::fs::write(&cfg_path, "{ invalid json }").unwrap();
        let report = inspect(&cfg_path, &root, &root.join("routes.snapshot"), &root);
        assert_eq!(report.config.status, "fail");
        assert!(cfg_path.is_file());
        assert!(!root.join("vault.key").exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
