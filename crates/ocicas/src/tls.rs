//! The extra trust anchor both registry clients read from the same CA file.

use std::path::Path;

use crate::error::{Error, Result};

/// A CA file read from disk: the PEM bytes, for `oci-client`'s own config, and
/// the certificates parsed out of them, for a `reqwest` builder.
#[derive(Debug)]
pub(crate) struct CaRoots {
    pub(crate) pem: Vec<u8>,
    pub(crate) certs: Vec<reqwest::Certificate>,
}

/// Reads `ca_file` and parses the certificates it holds, naming the file in
/// every error — otherwise a missing or malformed anchor surfaces as an
/// unattributed "no such file" or "builder error".
///
/// The parse is not redundant with the client build: under the rustls backend
/// `reqwest::Certificate::from_pem` only stores the bytes, so an empty file, a
/// DER `.crt`, or a PEM holding no certificate would be accepted here and show
/// up much later as a connection that cannot be verified.
pub(crate) fn ca_roots(ca_file: &Path) -> Result<CaRoots> {
    let named = |e: &dyn std::fmt::Display| {
        Error::format(format!("ca certificate {}: {e}", ca_file.display()))
    };
    let pem = std::fs::read(ca_file).map_err(|e| named(&e))?;
    let certs = reqwest::Certificate::from_pem_bundle(&pem).map_err(|e| named(&e))?;
    if certs.is_empty() {
        return Err(named(&"holds no PEM certificate"));
    }
    Ok(CaRoots { pem, certs })
}

#[cfg(test)]
mod tests {
    use super::*;

    // `create_new` rather than a plain write: a predictable shared-temp name is
    // a redirect waiting to happen.
    fn write_temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!("{name}-{}.pem", std::process::id()));
        let _ = std::fs::remove_file(&path);
        std::fs::File::create_new(&path)
            .expect("create")
            .write_all(bytes)
            .expect("write");
        path
    }

    #[test]
    fn a_missing_file_is_named() {
        let err = ca_roots(Path::new("/nope/absent.crt")).expect_err("missing file");
        assert!(
            err.to_string().contains("/nope/absent.crt"),
            "unhelpful error: {err}"
        );
    }

    // A file with no certificate in it would otherwise be accepted and add no
    // anchor at all, leaving the connection to fail with the reason a level
    // away from the mistake.
    #[test]
    fn a_file_holding_no_certificate_is_rejected() {
        for (name, bytes) in [
            ("ocicas-empty", &b""[..]),
            ("ocicas-not-pem", &b"\x30\x82\x01\x0a not pem"[..]),
            (
                "ocicas-key-only",
                &b"-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n"[..],
            ),
        ] {
            let path = write_temp(name, bytes);
            let err = ca_roots(&path).expect_err("no certificate");
            let _ = std::fs::remove_file(&path);
            let msg = err.to_string();
            assert!(msg.contains(name), "unhelpful error: {msg}");
        }
    }
}
