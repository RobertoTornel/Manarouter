//! Built-in TLS termination for the relay and the control plane.
//!
//! Both listeners take an optional `tls` block (`cert` + `key` PEM paths). When it is present the
//! listener serves HTTPS instead of HTTP. The obvious source of a trusted pair on a tailnet is
//! Tailscale's own `tailscale cert` (publicly trusted, MagicDNS hostname, auto-renewable); the
//! paths are read from disk at startup, so renewing the files requires a restart.
//!
//! Nothing here invents a certificate: a missing file, an unreadable file, or a key that does not
//! match the certificate is a startup error that names the file.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use rustls::ServerConfig;

/// Load a rustls server configuration from a certificate and a private key in PEM form.
///
/// Both files must exist, the certificate chain must contain at least one certificate, and the key
/// must be a PKCS#8 private key (what `tailscale cert` and `openssl req` write).
pub fn server_config(cert_path: &Path, key_path: &Path) -> Result<ServerConfig, String> {
    let cert_file = File::open(cert_path).map_err(|e| {
        format!(
            "cannot read the TLS certificate {}: {e}",
            cert_path.display()
        )
    })?;
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(cert_file))
            .collect::<Result<_, _>>()
            .map_err(|e| {
                format!(
                    "the TLS certificate {} is not valid PEM: {e}",
                    cert_path.display()
                )
            })?;
    if certs.is_empty() {
        return Err(format!(
            "the TLS certificate {} contains no certificates",
            cert_path.display()
        ));
    }

    let key_file = File::open(key_path).map_err(|e| {
        format!(
            "cannot read the TLS private key {}: {e}",
            key_path.display()
        )
    })?;
    let key = rustls_pemfile::private_key(&mut BufReader::new(key_file))
        .map_err(|e| {
            format!(
                "the TLS private key {} is not valid PEM: {e}",
                key_path.display()
            )
        })?
        .ok_or_else(|| {
            format!(
                "the TLS private key {} contains no PKCS#8 key",
                key_path.display()
            )
        })?;

    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("the TLS certificate and key do not match: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CERT: &str = "-----BEGIN CERTIFICATE-----\nMIIDFTCCAf2gAwIBAgIUYQWNdzqoE529TvN5QOmn4ABDNB8wDQYJKoZIhvcNAQEL\nBQAwGjEYMBYGA1UEAwwPdGFpbG5ldC5pbnZhbGlkMB4XDTI2MTExODE1MTczMFoX\nDTM2MTExNTE1MTczMFowGjEYMBYGA1UEAwwPdGFpbG5ldC5pbnZhbGlkMIIBIjAN\nBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAyqwXkiM+iTM2ACqKjK2Ms23TyKcn\nISFLmpOps4H1RccvNRAdd12JLL8E+HNvsZT3LnWSRdf1y0iJ6tyRgUAdo3T5EHQv\n8u0/s5F0+AZNGICJqRKVWhvnbUj0HfWnqFjLFeXDhfcWoNEl6ZZ+/8pSVftE9M/H\njFXKBYl71nrbg2K3P2wY6xm9u3DbsKmsj0NAQ3wQ5mAw4BXEBvCcZ0ByNCJ6a33u\n9LAaBZJppIS/tE+HIMuUpJN/8pfO5JTuJhheneUBgmBztBGp/CX7wa8gMiXKkNuy\nE6IP7wCp21GTXr+IIZnDImCTeNM0/4OCSEelMHuSHFjuVst0Rm9se62p1wIDAQAB\no1MwUTAdBgNVHQ4EFgQUBq8kQAHKCllzcejXi4xQoO9vEPswHwYDVR0jBBgwFoAU\nBq8kQAHKCllzcejXi4xQoO9vEPswDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG9w0B\nAQsFAAOCAQEAljx9W6NiAWvpRyJXkc+Z5xHvQvdh0OfRSY97Ap+klHyS5Y+NqNob\nfWN0RHyXACDbnEMCZnw6n8qqi/ET5xHQm/GpTs6H7u7BPzAe8wxpRs5bENzETm4q\nPp6Xp28/9k2AEh7UzKhKfQdJ1mS/xP2ZaAsRvrBFd6Ftuy0cDBVH11BPyGHIW4O8\n6eC3Cn8ZwS1EwWgWIvvWBERWcWYuU7Or+W8B9u7Mg/0iXTna5FrkGNeOB48RkrY4\naPbBDN++TaOMmnKIeTpB0PAgjHiJ4zm8/bPTW1TH921ALRSmkLTXh395Dsa3I6QS\nyGr4kvqkjCcgcPM+ab7WBICZmxRA+IZccQ==\n-----END CERTIFICATE-----\n";

    const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQDKrBeSIz6JMzYA\nKoqMrYyzbdPIpychIUuak6mzgfVFxy81EB13XYksvwT4c2+xlPcudZJF1/XLSInq\n3JGBQB2jdPkQdC/y7T+zkXT4Bk0YgImpEpVaG+dtSPQd9aeoWMsV5cOF9xag0SXp\nln7/ylJV+0T0z8eMVcoFiXvWetuDYrc/bBjrGb27cNuwqayPQ0BDfBDmYDDgFcQG\n8JxnQHI0Inprfe70sBoFkmmkhL+0T4cgy5Skk3/yl87klO4mGF6d5QGCYHO0Ean8\nJfvBryAyJcqQ27ITog/vAKnbUZNev4ghmcMiYJN40zT/g4JIR6Uwe5IcWO5Wy3RG\nb2x7ranXAgMBAAECggEASbM6/xGB9DKDOJw/tdiGidkuYpEmUY3iCFwaIgToRG+i\nYxFxJa8F6xernw2bWGBrYqGuh2d5ImtGe/mpj+7Y2E/5Ilqd9SEBQTNr7aWc84QK\ni0kOtq3fAoa8/wgN59IiR9w9tcGcoO5tcvwfQrU4K7wr3zqPviCUTl8ogV475RKi\nLS/euSBdy0ZWgCC6tQ8DWDRSgDyf3SwUepC7UxjEa1C5Md0COqOFNLXHhP2W2npj\nzn3+stL3bOKEUV+7tRfzwfLl97VRk0eG2y0bti4wGy5bYgeXmEimXhmyd0TjXWgw\ncvRx2Q6IRewR7TDArWXyLBDrFaNuaUwl8eA1R8swAQKBgQD899CiQfY0DlKNKKhd\nPRZrTm7EfkzijxfC5qunZi/5yJHkDDK0K3WNa0u7Mg1DVDs2Qp/jSVcq5Gs1AVgV\nfgarJL/PVtXee6+QcWmW65J1/DdNYDHNcDVwcrQoh1W7epSMpiGhaBFRJ3oLI2YD\n2hRfL7zitbwHBjWkYN0IO0djAQKBgQDNGfQxNvDvClA3Qmf4uO9MVEV4hzF3zbov\nHaKmEycTkyAi2RU39DhnC++VfwtWpjBZXa2+EOJVkpQ5xUnwlJvKDb27NU14Kvcs\ngSeRDrj/s3+8ARwVR2KrRaTBFNQc9VETFKN6Ln1OTQFA9nfrlm6+uETAsHJogpJi\nAd0//K2E1wKBgCqZIIzyRp9leglDO3bX/pp7TrPQAOp15i5v4aAC2TlayRZ551/n\noZf65XnUZ99ao83zUjFX/FHAvTzjyQ11d3ICcjpcCQ+6DEpqjrlXWgZ8PmPNOxW5\nO4aACk5427sCXVmB8E/Z0nC0ER0nDNrBMoUE6O3UZKFaLlZL6WylbtQBAoGAekfs\nB9+Ku+m5M4j69hZETYy8soRf4N6eD3MqwrsOhCQmd8BXXznS2tUJ7WaGOw3wK4en\ni3cDiiNPDjZqw+3GCCwzH0dDPc7X+QezsE9HyuOY110OAj3CPPzTom4pnG5B/Q9D\n/Bd1i9ZrVfDcRHA27KxuCTfprheS8K3N3FVompcCgYBk0qTrAkz3MbCR1+ITLRGU\nB0AIJODRiXEoB7BYha7mXHKyFGzDYVy67pTmLoMnBAj0sqaMXGcRNW64tS7JlMEw\nRq0n5hyOQNRi2kGMOEMBd+MWsPTb0rAyhSSafsuhK2dET2qas4aU2tD0lM2AzIx7\npCsKhFkY0rLDe/H2luQbeQ==\n-----END PRIVATE KEY-----\n";

    fn write_pair(dir: &tempfile::TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, TEST_CERT).expect("write cert");
        std::fs::write(&key_path, TEST_KEY).expect("write key");
        (cert_path, key_path)
    }

    #[test]
    fn a_missing_certificate_is_a_named_error() {
        let err = server_config(
            Path::new("/nonexistent/cert.pem"),
            Path::new("/nonexistent/key.pem"),
        )
        .expect_err("a missing certificate must fail");
        assert!(err.contains("cert"), "{err}");
    }

    #[test]
    fn a_key_file_without_a_private_key_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (cert_path, _) = write_pair(&dir);
        // A PEM file that holds no PKCS#8 private key (here: the certificate itself).
        let err = server_config(&cert_path, &cert_path).expect_err("no key must fail");
        assert!(err.contains("PKCS#8"), "{err}");
    }

    #[test]
    fn a_valid_pair_loads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (cert_path, key_path) = write_pair(&dir);
        assert!(server_config(&cert_path, &key_path).is_ok());
    }
}
