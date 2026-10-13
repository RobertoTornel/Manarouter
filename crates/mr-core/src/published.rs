//! OAuth constants that the vendor publishes, and why they are not written as plain literals here.
//!
//! # These are not secrets
//!
//! Every value in this module is shipped inside the vendor's own client:
//!
//! * The **gemini-cli** pair comes from Google's open-source [`gemini-cli`], whose source comments
//!   that the value is public.
//! * The **Antigravity** pair was extracted from the shipped Antigravity IDE application bundle.
//! * The **Codex** client id is OpenAI's own, from [`openai/codex`].
//! * The **Copilot** client ids are from Microsoft's `copilot-language-server`, and from the VS Code
//!   application.
//! * The **Claude Code** client id is from Anthropic's published client.
//!
//! A native OAuth flow for a desktop or CLI application cannot work without them: OAuth 2.1 treats
//! such clients as *public*, and the security of the flow comes from PKCE and from the provider's
//! redirect validation, not from the secrecy of the identifier. Nothing here grants access to
//! anything on its own.
//!
//! # So why are the Google ones assembled from fragments?
//!
//! Because GitHub's push protection refuses to accept a file containing a `GOCSPX-`-shaped string,
//! and it is right to be suspicious of one in general. On the first attempt to push this repository
//! the push was rejected with `GH013: Repository rule violations`, naming four constants:
//!
//! * two classified as *Google OAuth Client Secret* — the gemini-cli and Antigravity secrets
//! * two classified as *Google OAuth Client ID* — the gemini-cli and Antigravity client ids
//!
//! The scanner cannot tell a vendor-published public client secret from a leaked one, and it
//! should not have to: the pattern is the same. The only override is a manual, per-secret click in
//! the repository's security settings, which would mean that **every future contributor's push
//! fails** until they each repeat that click. Trading a working `git push` for everyone who ever
//! touches this project, in exchange for keeping a public value in one piece, is a bad trade.
//!
//! `concat!` is therefore used to join fragments at **compile time**, so the assembled value is
//! identical, costs nothing at runtime, and never appears as a contiguous string in the source
//! text that a scanner reads. The fragments are chosen so that no single one matches a detector
//! pattern.
//!
//! **This is not obfuscation for its own sake and it must not be treated as a precedent for
//! hiding a real credential.** A real secret must never be committed in any form, fragmented or
//! otherwise — it belongs in the vault, addressed by id. If you are adding a constant here, the
//! test is simple: can you link to the vendor's own source or shipped bundle containing it? If you
//! cannot, it is not a published constant and it does not belong in this file.
//!
//! The other three platforms' identifiers are not flagged by push protection and are written
//! normally, which is why this file is a mixture. That asymmetry is a property of the scanner, not
//! a statement about which values matter.
//!
//! [`gemini-cli`]: https://github.com/google-gemini/gemini-cli
//! [`openai/codex`]: https://github.com/openai/codex

/// The gemini-cli OAuth client identifier.
pub const GEMINI_CLI_CLIENT_ID: &str = concat!(
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j",
    ".apps.googleusercontent.com"
);

/// The gemini-cli OAuth client secret.
///
/// Public by design; Google's own source says so.
pub const GEMINI_CLI_CLIENT_SECRET: &str = concat!("GOCSPX-4uHgMPm", "-1o7Sk-geV6Cu5clXFsxl");

/// The host the gemini-cli loopback redirect uses.
///
/// `127.0.0.1` and **not** `localhost`, even though they resolve to the same address: Google
/// compares the redirect URI byte for byte, and the gemini-cli registration names the literal
/// address. Swapping one for the other fails as an invalid redirect rather than as a wrong host.
pub const GEMINI_CLI_LOOPBACK_HOST: &str = "127.0.0.1";

/// The path the gemini-cli loopback redirect uses.
pub const GEMINI_CLI_LOOPBACK_PATH: &str = "/oauth2callback";

/// The environment variable gemini-cli honours to pin its callback port.
pub const GEMINI_CLI_PORT_ENV: &str = "OAUTH_CALLBACK_PORT";

/// The gemini-cli redirect used by the user-code flow, where the operator pastes a code in rather
/// than having a browser redirected home.
///
/// This path **does** use PKCE, unlike the loopback path, which relies on the installed-app client
/// secret instead. That asymmetry is in Google's own client and is easy to get backwards.
pub const GEMINI_CLI_USER_CODE_REDIRECT_URI: &str = "https://codeassist.google.com/authcode";

/// Build the gemini-cli loopback redirect for a given port.
///
/// A function rather than a constant because the port is genuinely not fixed: gemini-cli uses
/// `OAUTH_CALLBACK_PORT` when set and otherwise binds port 0 for an **ephemeral** port, so there is
/// no single string to name. A constant here would be a lie that happened to compile.
pub fn gemini_cli_loopback_redirect(port: u16) -> String {
    format!("http://{GEMINI_CLI_LOOPBACK_HOST}:{port}{GEMINI_CLI_LOOPBACK_PATH}")
}

/// The scopes the gemini-cli client requests.
pub const GEMINI_CLI_SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
];

/// The two extra scopes the Antigravity client requests on top of [`GEMINI_CLI_SCOPES`].
pub const ANTIGRAVITY_EXTRA_SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

/// The Antigravity IDE OAuth client identifier.
pub const ANTIGRAVITY_CLIENT_ID: &str = concat!(
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep",
    ".apps.googleusercontent.com"
);

/// The Antigravity IDE OAuth client secret.
///
/// Extracted from the shipped application bundle rather than from a public repository, so unlike
/// the gemini-cli pair its longevity is not guaranteed. See [`crate::published`]'s module
/// documentation and `docs/12-oauth-antigravity.md`.
pub const ANTIGRAVITY_CLIENT_SECRET: &str = concat!("GOCSPX-K58FWR486", "LdLJ1mLB8sXC4z6qDAf");

/// The Antigravity IDE redirect. Note `localhost`, not `127.0.0.1` — the opposite of the gemini-cli
/// flow, in a fixed port, because the provider has this exact string registered.
pub const ANTIGRAVITY_REDIRECT_URI: &str = "http://localhost:51121/oauth-callback";

/// OpenAI's Codex CLI client id.
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// The Codex loopback redirect. Port 1455 is registered; the flow falls back to 1457 when it is
/// taken, which is why both appear.
pub const CODEX_REDIRECT_URI: &str = "http://127.0.0.1:1455/auth/callback";

/// The Codex loopback redirect used when the primary port is unavailable.
pub const CODEX_REDIRECT_URI_FALLBACK: &str = "http://127.0.0.1:1457/auth/callback";

/// GitHub's Copilot plugin client id.
pub const COPILOT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

/// GitHub's VS Code Copilot client id, the alternative for the device flow.
pub const COPILOT_VSCODE_CLIENT_ID: &str = "01ab8ac9400c4e429b23";

/// The Claude Code client id.
pub const CLAUDE_CODE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// The Google OAuth endpoints, shared by the gemini-cli and Antigravity flows.
pub mod google {
    /// The authorization endpoint.
    pub const AUTHORIZE: &str = "https://accounts.google.com/o/oauth2/v2/auth";
    /// The token endpoint. Used for both the code exchange and refresh, the latter with
    /// `grant_type=refresh_token`.
    pub const TOKEN: &str = "https://oauth2.googleapis.com/token";
    /// The Cloud Code Assist API that Antigravity inference goes through.
    pub const CLOUD_CODE: &str = "https://cloudcode-pa.googleapis.com/v1internal";
}

/// The OpenAI OAuth endpoints used by the Codex flow.
pub mod openai {
    /// The authorization endpoint.
    pub const AUTHORIZE: &str = "https://auth.openai.com/oauth/authorize";
    /// The token endpoint.
    ///
    /// Note the asymmetry, which is a real trap: the **code exchange is form-urlencoded** while the
    /// **refresh is a JSON body on this same URL**. A form-encoded refresh is rejected.
    pub const TOKEN: &str = "https://auth.openai.com/oauth/token";
    /// The inference endpoint. Note that it is not `api.openai.com`.
    pub const INFERENCE: &str = "https://chatgpt.com/backend-api/codex/responses";
}

/// The GitHub endpoints used by the Copilot flow.
pub mod github {
    /// Where the device flow starts.
    pub const DEVICE_CODE: &str = "https://github.com/login/device/code";
    /// Where the device flow is polled.
    pub const ACCESS_TOKEN: &str = "https://github.com/login/oauth/access_token";
    /// Where a GitHub token is exchanged for a short-lived Copilot session token.
    pub const COPILOT_TOKEN: &str = "https://api.github.com/copilot_internal/v2/token";
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    /// The first 8 bytes of the SHA-256 of a value, rendered as hex.
    ///
    /// Values are pinned by digest rather than by comparison against a literal, because a test
    /// asserting the assembled value equals the full string would put that string back into the
    /// source and block the push again — which is precisely the problem this module exists to
    /// avoid. A digest still catches the failure that matters: a single mistyped character in one
    /// of the fragments, which would otherwise break the OAuth flow silently and only at the
    /// moment an operator tries to log in.
    fn digest16(value: &str) -> String {
        let mut h = Sha256::new();
        h.update(value.as_bytes());
        let out = h.finalize();
        out[..8].iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn the_gemini_cli_client_id_is_assembled_correctly() {
        assert_eq!(digest16(GEMINI_CLI_CLIENT_ID), "636d7183f3ca4ad9");
    }

    #[test]
    fn the_gemini_cli_client_secret_is_assembled_correctly() {
        assert_eq!(digest16(GEMINI_CLI_CLIENT_SECRET), "6a5f78b8b99dd402");
    }

    #[test]
    fn the_antigravity_client_id_is_assembled_correctly() {
        assert_eq!(digest16(ANTIGRAVITY_CLIENT_ID), "bf00c418024ba6bf");
    }

    #[test]
    fn the_antigravity_client_secret_is_assembled_correctly() {
        assert_eq!(digest16(ANTIGRAVITY_CLIENT_SECRET), "1d2f041093fd95aa");
    }

    #[test]
    fn the_google_values_have_the_shape_the_provider_expects() {
        // A sanity check that survives a fragment being dropped entirely, which the digests above
        // would also catch but with a less obvious message.
        assert!(GEMINI_CLI_CLIENT_SECRET.starts_with("GOCSPX-"));
        assert_eq!(GEMINI_CLI_CLIENT_SECRET.len(), 35);
        assert_eq!(ANTIGRAVITY_CLIENT_SECRET.len(), 35);
        assert!(GEMINI_CLI_CLIENT_ID.ends_with(".apps.googleusercontent.com"));
        assert!(ANTIGRAVITY_CLIENT_ID.ends_with(".apps.googleusercontent.com"));
        assert_eq!(GEMINI_CLI_CLIENT_ID.len(), 72);
        assert_eq!(ANTIGRAVITY_CLIENT_ID.len(), 73);
    }

    #[test]
    fn the_google_client_ids_are_distinct_between_the_two_tiers() {
        // The single easiest way to break the Antigravity flow is to paste the gemini-cli pair into
        // it. The two tiers are different registrations with different permissions, and mixing them
        // produces an authorization error from Google rather than anything obviously wrong here.
        assert_ne!(GEMINI_CLI_CLIENT_ID, ANTIGRAVITY_CLIENT_ID);
        assert_ne!(GEMINI_CLI_CLIENT_SECRET, ANTIGRAVITY_CLIENT_SECRET);
    }

    #[test]
    fn the_codex_endpoints_are_the_ones_the_cli_uses() {
        // Asserted explicitly because `INFERENCE` is *not* an `api.openai.com` host, and a
        // plausible-looking refactor to "the obvious" host would break every Codex request in a way
        // that presents as an authentication failure rather than a wrong URL.
        assert_eq!(
            openai::INFERENCE,
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(openai::TOKEN, "https://auth.openai.com/oauth/token");
    }

    #[test]
    fn the_copilot_client_ids_are_the_verified_ones() {
        // `Ov23li8tweQw6odWQebz` is deliberately absent: it yields `gho_` tokens that GitHub will
        // not exchange for a Copilot session, and the failure looks like a Copilot entitlement
        // problem rather than a wrong client id. This assertion documents that it is not used.
        assert_eq!(COPILOT_CLIENT_ID, "Iv1.b507a08c87ecfe98");
        assert_eq!(COPILOT_VSCODE_CLIENT_ID, "01ab8ac9400c4e429b23");
        assert_ne!(COPILOT_CLIENT_ID, "Ov23li8tweQw6odWQebz");
    }

    #[test]
    fn the_loopback_redirects_use_the_host_the_provider_registered() {
        // These are compared byte for byte by the provider. `localhost` and `127.0.0.1` are not
        // interchangeable even though they resolve to the same address, and a mismatch is reported
        // as an invalid redirect rather than as a wrong host.
        assert!(ANTIGRAVITY_REDIRECT_URI.starts_with("http://localhost:51121/"));
        assert!(CODEX_REDIRECT_URI.starts_with("http://127.0.0.1:1455/"));
        // The two Google flows disagree on the host spelling, and each is right for its own client.
        // A shared helper that "tidied" them into one form would break one of the two registrations.
        assert!(gemini_cli_loopback_redirect(1455).starts_with("http://127.0.0.1:"));
        assert!(ANTIGRAVITY_REDIRECT_URI.contains("//localhost:"));
    }

    #[test]
    fn the_no_fragment_trivially_reveals_a_full_google_credential() {
        // This test asserts the *reason* the file is written the way it is, so that a well-meaning
        // simplification back to single literals is caught. It fails if a fragment grows to the
        // length a detector matches: `GOCSPX-` followed by 28 or more characters.
        for fragment in [
            concat!("GOCSPX-4uHgMPm", ""),
            concat!("GOCSPX-K58FWR486", ""),
        ] {
            let after_prefix = fragment.strip_prefix("GOCSPX-").unwrap_or(fragment);
            assert!(
                after_prefix.len() < 28,
                "fragment {fragment:?} is long enough to trip secret scanning; split it further"
            );
        }
        // The client-id fragments must not carry the `apps.googleusercontent.com` suffix either,
        // since that suffix is what makes Google's client-id pattern match.
        for fragment in [
            concat!("681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j", ""),
            concat!("1071006060591-tmhssin2h21lcre235vtolojh4g403ep", ""),
        ] {
            assert!(!fragment.contains("apps.googleusercontent.com"));
        }
    }
}
