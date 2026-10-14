//! Presentation shared by every `mr` subcommand: exit codes, error reporting, tables, colour,
//! and the credential rule that governs all of them.
//!
//! **The credential rule.** No command prints a stored credential in full, ever. The only
//! deliberate exception is `key create`, which generates a brand-new credential and must print it
//! exactly once so the operator can copy it before it is hashed into `config.json`; nothing that
//! was ever stored can be printed back. Everything else shows a *fingerprint* — `mask()` below,
//! which for a secret of eight characters or fewer reveals nothing at all — and response bodies
//! fetched from the network go through [`redact_text`] before they reach a terminal.

use std::io::IsTerminal;

use mr_core::secret::looks_like_a_credential;
use serde::Serialize;

/// Process exit codes, documented in `mr --help`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Everything the command was asked to do happened.
    Success = 0,
    /// A general failure: I/O, network, vault, or a child process that died.
    Failure = 1,
    /// The command line could not be understood: unknown command, missing required argument,
    /// invalid enum value, out-of-range index.
    Usage = 2,
    /// The command understood everything and refused anyway because saving the result would put an
    /// invalid configuration on disk (or the configuration on disk is already invalid).
    Validation = 3,
    /// The named provider, combo, account, key or alias does not exist.
    NotFound = 4,
}

impl Exit {
    /// The numeric code, for [`std::process::ExitCode`].
    pub const fn code(self) -> u8 {
        self as u8
    }
}

/// A command failure carrying the exit code it must produce.
#[derive(Debug, Clone)]
pub struct CliError {
    pub exit: Exit,
    pub message: String,
    pub details: Vec<String>,
}

impl CliError {
    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            exit: Exit::Failure,
            message: message.into(),
            details: Vec::new(),
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            exit: Exit::Usage,
            message: message.into(),
            details: Vec::new(),
        }
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self {
            exit: Exit::Validation,
            message: message.into(),
            details: Vec::new(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            exit: Exit::NotFound,
            message: message.into(),
            details: Vec::new(),
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.details.push(detail.into());
        self
    }

    pub fn with_details(mut self, details: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.details.extend(details.into_iter().map(Into::into));
        self
    }

    /// Render the error the way `main` prints it: one `error:` line, then indented details.
    pub fn render(&self) -> String {
        let mut out = format!("error: {}", self.message);
        for detail in &self.details {
            out.push_str("\n  ");
            out.push_str(detail);
        }
        out
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.render())
    }
}

impl From<mr_core::Error> for CliError {
    fn from(e: mr_core::Error) -> Self {
        use mr_core::Error;
        let (exit, message) = match &e {
            Error::NotFound(what) => (Exit::NotFound, what.clone()),
            Error::Config(_) => (Exit::Validation, bare(e.to_string(), "config error: ")),
            Error::Json(_) | Error::Toml(_) => (Exit::Validation, e.to_string()),
            other => (Exit::Failure, other.to_string()),
        };
        CliError {
            exit,
            message,
            details: Vec::new(),
        }
    }
}

/// Strip a known `Display` prefix so the message the operator reads is the message itself.
fn bare(full: String, prefix: &str) -> String {
    full.strip_prefix(prefix).map(str::to_owned).unwrap_or(full)
}

// ---------------------------------------------------------------------------
// Colour
// ---------------------------------------------------------------------------

/// Whether ANSI colour is appropriate for stdout right now.
///
/// Tests run piped, so colour is off in every assertion below unless a test opts in explicitly.
pub fn use_color() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

/// Wrap `text` in an ANSI colour when `enabled`.
pub fn paint(enabled: bool, code: &str, text: &str) -> String {
    if enabled {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

// ---------------------------------------------------------------------------
// Credential masking
// ---------------------------------------------------------------------------

/// Fingerprint a secret for display: never the secret itself.
///
/// A secret of eight characters or fewer is shown as `••••` in full — there is no safe prefix and
/// no safe suffix at that length, and short secrets are common (someone's test token). Longer
/// secrets keep three leading characters and the trailing four, which is enough for an operator to
/// tell two keys apart and not enough to use one.
pub fn mask(secret: &str) -> String {
    const MASKED: &str = "••••";
    if secret.is_empty() {
        return MASKED.to_owned();
    }
    if !secret.is_ascii() || secret.len() <= 8 {
        // Non-ASCII input may split inside a character at any boundary; mask the whole thing.
        return MASKED.to_owned();
    }
    format!("{}…{}", &secret[..3], &secret[secret.len() - 4..])
}

/// Print `stored: <fingerprint>` for a freshly stored credential and return the line, so callers
/// can both show it and (in tests) capture it.
pub fn confirm_secret(secret: &str, _label: &str) -> String {
    format!("stored: {}", mask(secret))
}

/// Replace every credential-shaped token in `text` with its fingerprint.
///
/// Used for anything that came back off the network (`provider test` error bodies, upstream error
/// payloads): an upstream that echoes your Authorization header back at you must not be able to
/// print it through us. Best effort by construction — the split points are whitespace, quotes and
/// the punctuation that separates a key from its JSON envelope — so this is a redaction, not a
/// guarantee, and is documented as such.
pub fn redact_text(text: &str) -> String {
    const DELIMS: [char; 9] = [' ', '\t', '\n', '\r', '"', '\'', '=', ',', ';'];
    let mut out = String::with_capacity(text.len());
    for chunk in text.split_inclusive(|c: char| DELIMS.contains(&c)) {
        // Separate trailing delimiters from the token core so `"sk-…"` yields `sk-…`.
        let cut = chunk
            .char_indices()
            .rev()
            .find(|(_, c)| !DELIMS.contains(c))
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        let (core, tail) = chunk.split_at(cut);
        if looks_like_a_credential(core) {
            out.push_str(&mask(core));
        } else {
            out.push_str(core);
        }
        out.push_str(tail);
    }
    out
}

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

/// A box-drawing table with a header row.
///
/// Widths are counted in `char`s, which is right for the ids and labels this CLI shows (ASCII);
/// a wide CJK label would shift the right edge, and the fix for that is a real unicode-width
/// dependency, which this crate deliberately does not carry.
#[derive(Debug, Default)]
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub fn new<S: Into<String>>(headers: impl IntoIterator<Item = S>) -> Self {
        Self {
            headers: headers.into_iter().map(Into::into).collect(),
            rows: Vec::new(),
        }
    }

    pub fn row<S: Into<String>>(&mut self, cells: impl IntoIterator<Item = S>) -> &mut Self {
        self.rows.push(cells.into_iter().map(Into::into).collect());
        self
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Render with box-drawing characters, header underlined by `├─┼─┤`.
    pub fn render(&self) -> String {
        let cols = self.headers.len().max(1);
        let mut widths: Vec<usize> = self.headers.iter().map(|h| h.chars().count()).collect();
        widths.resize(cols, 6);
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate().take(cols) {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
        let border = |l: &str, m: &str, r: &str| -> String {
            let mut s = String::from(l);
            for (i, w) in widths.iter().enumerate() {
                if i > 0 {
                    s.push_str(m);
                }
                for _ in 0..w + 2 {
                    s.push('─');
                }
            }
            s.push_str(r);
            s.push('\n');
            s
        };
        let cells_line = |cells: &[String]| -> String {
            let mut s = String::from("│");
            for (i, w) in widths.iter().enumerate() {
                let cell = cells.get(i).map(String::as_str).unwrap_or("");
                s.push(' ');
                s.push_str(cell);
                for _ in cell.chars().count()..*w {
                    s.push(' ');
                }
                s.push(' ');
                if i + 1 < cols {
                    s.push('│');
                }
            }
            s.push_str("│\n");
            s
        };
        let mut out = String::new();
        out.push_str(&border("┌", "┬", "┐"));
        out.push_str(&cells_line(&self.headers));
        out.push_str(&border("├", "┼", "┤"));
        if self.rows.is_empty() {
            let none: Vec<String> = vec!["(none)".to_owned()];
            out.push_str(&cells_line(&none));
        }
        for row in &self.rows {
            out.push_str(&cells_line(row));
        }
        out.push_str(&border("└", "┴", "┘"));
        out
    }

    pub fn print(&self) {
        print!("{}", self.render());
    }
}

// ---------------------------------------------------------------------------
// Enum argument parsing
// ---------------------------------------------------------------------------

/// Parse an enum-ish command line value.
///
/// Order: normalise (`lower`, `_`/space → `-`), try serde's spelling first — that is the spelling
/// written into `config.json`, so `--strategy round-robin` and the config file can never disagree —
/// then the enum's own permissive `parse()`, then fail with the full list of accepted values.
pub fn parse_choice<T, F>(
    what: &str,
    raw: &str,
    choices: &[String],
    permissive: F,
) -> Result<T, CliError>
where
    T: serde::de::DeserializeOwned,
    F: FnOnce(&str) -> Option<T>,
{
    let norm = raw.trim().to_ascii_lowercase().replace(['_', ' '], "-");
    if let Ok(v) = serde_json::from_str::<T>(&format!("\"{norm}\"")) {
        return Ok(v);
    }
    if let Some(v) = permissive(&norm) {
        return Ok(v);
    }
    Err(CliError::usage(format!(
        "invalid {what} `{raw}`: expected one of {}",
        choices.join(", ")
    )))
}

/// The config-file spelling of a serialisable enum value, for "expected one of" lists.
pub fn serde_label<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "<unknown>".to_owned())
}

/// Every serialisable spelling in a `const ALL` array.
pub fn all_labels<T: Serialize>(all: &[T]) -> Vec<String> {
    all.iter().map(serde_label).collect()
}

// ---------------------------------------------------------------------------
// Lookups every command repeats
// ---------------------------------------------------------------------------

/// Find a provider or exit 4 with a hint that lists what does exist.
pub fn require_provider<'a>(
    cfg: &'a mr_core::config::Config,
    id: &str,
) -> Result<&'a mr_core::model::Provider, CliError> {
    cfg.provider(id).ok_or_else(|| {
        let mut e = CliError::not_found(format!("no provider with id `{id}`"));
        if !cfg.providers.is_empty() {
            let ids: Vec<&str> = cfg.providers.iter().map(|p| p.id.as_str()).collect();
            e = e.with_detail(format!("known providers: {}", ids.join(", ")));
        } else {
            e = e.with_detail("no providers are configured yet; try `mr provider list`");
        }
        e
    })
}

/// Find a combo by id or name, or exit 4.
pub fn require_combo<'a>(
    cfg: &'a mr_core::config::Config,
    id_or_name: &str,
) -> Result<&'a mr_core::model::Combo, CliError> {
    cfg.combo(id_or_name).ok_or_else(|| {
        let mut e = CliError::not_found(format!("no combo `{id_or_name}`"));
        if !cfg.combos.is_empty() {
            let names: Vec<&str> = cfg.combos.iter().map(|c| c.name.as_str()).collect();
            e = e.with_detail(format!("known combos: {}", names.join(", ")));
        } else {
            e = e.with_detail(
                "no combos exist yet; try `mr combo add <name> --step provider/model`",
            );
        }
        e
    })
}

/// Print a JSON document for `--json` readers; falls back to pretty form for humans.
pub fn print_json<T: Serialize>(value: &T) -> Result<(), CliError> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| CliError::failure(format!("cannot serialise output: {e}")))?;
    println!("{text}");
    Ok(())
}

/// Line-print an informational success message (stdout, so pipelines can see it).
pub fn say(line: impl AsRef<str>) {
    println!("{}", line.as_ref());
}

/// Print a warning to stderr so `--json` stdout stays machine-parseable.
pub fn warn(line: impl AsRef<str>) {
    eprintln!("warning: {}", line.as_ref());
}

#[cfg(test)]
pub(crate) mod testutil {
    //! Test scaffolding: unique temp directories without a `tempfile` dependency, and the env
    //! serialisation every test that points the CLI at one needs.
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    static N: AtomicU64 = AtomicU64::new(0);

    /// Serialises every test that must repoint `MANAROUTER_CONFIG_DIR`: environment variables
    /// are process-global and the harness runs tests concurrently.
    static ENV: Mutex<()> = Mutex::new(());

    /// A fresh directory under the system temp dir, unique per test name + pid + counter.
    pub(crate) fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mr-cli-test-{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test temp dir");
        dir
    }

    /// Best-effort cleanup; failing to remove a temp dir must not fail the test.
    pub(crate) fn cleanup(dir: &std::path::Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Run `f` with `MANAROUTER_CONFIG_DIR` pointing at a fresh temp directory, then restore the
    /// environment and remove the directory — even when `f` panics, so a failed assertion never
    /// leaves the next test looking at the wrong config.
    pub(crate) fn with_config_dir<T>(name: &str, f: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = temp_dir(name);
        std::env::set_var("MANAROUTER_CONFIG_DIR", &dir);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&dir)));
        std::env::remove_var("MANAROUTER_CONFIG_DIR");
        cleanup(&dir);
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    /// Run an async `mr` command on a throwaway runtime: unit tests are synchronous.
    pub(crate) fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio test runtime")
            .block_on(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_the_documented_numbers() {
        assert_eq!(Exit::Success.code(), 0);
        assert_eq!(Exit::Failure.code(), 1);
        assert_eq!(Exit::Usage.code(), 2);
        assert_eq!(Exit::Validation.code(), 3);
        assert_eq!(Exit::NotFound.code(), 4);
    }

    #[test]
    fn a_short_secret_masks_to_nothing() {
        assert_eq!(mask("abc"), "••••");
        assert_eq!(mask("12345678"), "••••");
        assert_eq!(mask(""), "••••");
    }

    #[test]
    fn a_long_secret_masks_to_fragments() {
        // "sk-proj-ABCDEFGH12345678": three leading characters, four trailing, nothing between. // secret-scan: allow
        let m = mask("sk-proj-ABCDEFGH12345678"); // secret-scan: allow
        assert_eq!(m, "sk-…5678");
        assert!(!m.contains("EFGH1234"), "{m}");
    }

    #[test]
    fn redact_text_masks_credentials_inside_prose_and_json() {
        let body = "upstream said: Authorization sk-live-abcdefghijklmnop rejected, code=401"; // secret-scan: allow
        let redacted = redact_text(body);
        assert!(!redacted.contains("sk-live-abcdefghijklmnop"), "{redacted}"); // secret-scan: allow
        assert!(redacted.contains("rejected"), "{redacted}");

        // A `ghp_…` GitHub token: recognised by mr-core's own credential detector (prefix list
        // plus minimum length), which is what redact_text defers to.
        let json =
            "{\"error\":{\"message\":\"key ghp_ABCDEFGHIJKLMNOPQRSTUV123456 was rejected\"}}"; // secret-scan: allow
        let redacted = redact_text(json);
        assert!(
            !redacted.contains("ghp_ABCDEFGHIJKLMNOPQRSTUV123456"), // secret-scan: allow
            "{redacted}"
        );
        assert!(redacted.contains("rejected"), "{redacted}");
    }

    #[test]
    fn redact_text_leaves_plain_words_alone() {
        let text = "no credentials here, just words";
        assert_eq!(redact_text(text), text);
    }

    #[test]
    fn table_renders_headers_rows_and_none() {
        let mut t = Table::new(["ID", "ENABLED"]);
        t.row(["openai", "yes"]);
        let s = t.render();
        assert!(s.contains("┌──"), "{s}");
        assert!(s.contains("│ ID"), "{s}");
        assert!(s.contains("openai"), "{s}");

        let empty = Table::new(["ID"]).render();
        assert!(empty.contains("(none)"), "{empty}");
    }

    #[test]
    fn parse_choice_prefers_the_serde_spelling_and_lists_the_rest_on_failure() {
        #[derive(Debug, serde::Deserialize, PartialEq)]
        #[serde(rename_all = "kebab-case")]
        enum Sample {
            RoundRobin,
        }
        let choices = vec!["round-robin".to_string()];
        let got: Sample = parse_choice("--strategy", "round_robin", &choices, |_| None).unwrap();
        assert_eq!(got, Sample::RoundRobin);
        let got: Sample = parse_choice("--strategy", " ROUND-ROBIN ", &choices, |_| None).unwrap();
        assert_eq!(got, Sample::RoundRobin);

        let err = parse_choice::<Sample, _>("--strategy", "magic", &choices, |_| None).unwrap_err();
        assert_eq!(err.exit, Exit::Usage);
        assert!(err.message.contains("round-robin"), "{}", err.message);
    }

    #[test]
    fn parse_choice_falls_back_to_a_permissive_parser() {
        #[derive(Debug, serde::Deserialize, PartialEq)]
        #[serde(rename_all = "kebab-case")]
        enum Dialectish {
            OpenAi,
        }
        let choices = vec!["open-ai".to_string()];
        // The serde spelling fails for `openai`, so the permissive parser (Dialect::parse's
        // alias list, in the real command) gets its turn — and wins.
        let got: Dialectish = parse_choice("--dialect", "openai", &choices, |s| {
            (s == "openai").then_some(Dialectish::OpenAi)
        })
        .unwrap();
        assert_eq!(got, Dialectish::OpenAi);
        // Neither path: the usage error lists the canonical spellings.
        let err = parse_choice::<Dialectish, _>("--dialect", "claude-code", &choices, |_| None)
            .unwrap_err();
        assert_eq!(err.exit, Exit::Usage);
        assert!(err.message.contains("open-ai"), "{}", err.message);
    }

    #[test]
    fn require_provider_and_combo_exit_not_found_with_known_lists() {
        let cfg = mr_core::config::Config::default();
        let err = require_provider(&cfg, "nope").unwrap_err();
        assert_eq!(err.exit, Exit::NotFound);
        assert!(err.details.iter().any(|d| d.contains("no providers")));
        let err = require_combo(&cfg, "nope").unwrap_err();
        assert_eq!(err.exit, Exit::NotFound);
    }

    #[test]
    fn core_errors_map_to_the_right_exit_codes() {
        let not_found: CliError = mr_core::Error::NotFound("provider `x`".into()).into();
        assert_eq!(not_found.exit, Exit::NotFound);
        let invalid: CliError = mr_core::Error::Config("bad base_url".into()).into();
        assert_eq!(invalid.exit, Exit::Validation);
        let io: CliError =
            mr_core::Error::Io(std::io::Error::from(std::io::ErrorKind::NotFound)).into();
        assert_eq!(io.exit, Exit::Failure);
    }

    #[test]
    fn render_indents_details_under_one_error_line() {
        let e = CliError::validation("nope")
            .with_detail("first")
            .with_detail("second");
        assert_eq!(e.render(), "error: nope\n  first\n  second");
    }
}
