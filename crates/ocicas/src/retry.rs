//! Repeating a registry request whose transport failed.
//!
//! Everything the store asks a registry is safe to ask twice: a pull is a GET of
//! content-addressed bytes verified on arrival, a push puts a blob under its own
//! digest or a manifest under its tag, and an existence check is a HEAD. So a
//! request that got no answer — the connection not made, a stalled read, a body
//! cut short by the kernel giving up on the socket — is made again, seconds
//! apart, before the store gives up on it. An answer the registry did give (a
//! miss, a refusal, a digest that does not match) is final at once.

use std::time::Duration;

use crate::error::{Error, MAX_CAUSES};

/// Attempts past the first before a transport failure is final.
pub(crate) const RETRIES: u32 = 3;
/// Pause before the second attempt; each later pause doubles (2 s, 4 s, 8 s).
pub(crate) const FIRST_PAUSE: Duration = Duration::from_secs(2);

/// Run `attempt`, and run it again after each transport failure ([`Error::Network`])
/// up to [`RETRIES`] times, pausing [`FIRST_PAUSE`] and doubling in between. Any
/// other error, and a success, come back at once.
pub(crate) async fn with_retry<T, F, Fut>(attempt: F) -> Result<T, Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Error>>,
{
    retry_paced(FIRST_PAUSE, attempt).await
}

/// [`with_retry`] with the first pause spelled out (the tests run it with none).
pub(crate) async fn retry_paced<T, F, Fut>(
    first_pause: Duration,
    mut attempt: F,
) -> Result<T, Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Error>>,
{
    let mut pause = first_pause;
    let mut left = RETRIES;
    loop {
        match attempt().await {
            Err(e) if left > 0 && e.is_unreachable() => {
                left = left.saturating_sub(1);
                tokio::time::sleep(pause).await;
                pause = pause.saturating_mul(2);
            }
            r => return r,
        }
    }
}

/// Whether `e` is the transport failing rather than the registry answering: a
/// connection not made or timed out, a request that got no response, a body cut
/// short. The last two arrive with the kernel's or hyper's error below reqwest's —
/// an `ETIMEDOUT` or `ECONNRESET`, or hyper's own "connection closed" — while a
/// response that could not be *understood* (a manifest that is not JSON) carries a
/// parse error there instead, and is not a transport failure.
pub(crate) fn is_transport(e: &reqwest::Error) -> bool {
    if e.is_connect() || e.is_timeout() {
        return true;
    }
    let mut transport = e.is_request();
    for cause in
        std::iter::successors(std::error::Error::source(e), |c| c.source()).take(MAX_CAUSES)
    {
        if let Some(hyper) = cause.downcast_ref::<hyper::Error>() {
            // Malformed HTTP and client misuse are final even during send().
            if hyper.is_parse() || hyper.is_user() {
                return false;
            }
            transport = true;
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            // Hyper reports malformed chunk framing as an I/O error in a body.
            if matches!(
                io.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData
            ) {
                return false;
            }
            transport = true;
        }
    }
    transport
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::testutil::{FakeServer, install_crypto};

    #[tokio::test]
    async fn a_closed_connection_is_the_transport() {
        install_crypto();
        let server = FakeServer::start_raw(vec![String::new()]);
        let error = reqwest::Client::new()
            .get(server.base())
            .send()
            .await
            .expect_err("server closed without answering");
        assert!(is_transport(&error));
    }

    #[tokio::test]
    async fn a_truncated_body_is_the_transport() {
        install_crypto();
        let server = FakeServer::start_raw(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nshort".into(),
        ]);
        let error = reqwest::get(server.base())
            .await
            .unwrap()
            .bytes()
            .await
            .expect_err("body cut short");
        assert!(is_transport(&error));
    }

    #[tokio::test]
    async fn registry_and_parse_errors_are_final() {
        install_crypto();
        let server = FakeServer::start(vec![(401, "denied"), (200, "not JSON")]);
        let status = reqwest::get(server.base())
            .await
            .unwrap()
            .error_for_status()
            .unwrap_err();
        assert!(!is_transport(&status));
        let json = reqwest::get(server.base())
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap_err();
        assert!(!is_transport(&json));

        let server = FakeServer::start_raw(vec!["NOT HTTP\r\n\r\n".into()]);
        let protocol = reqwest::get(server.base()).await.unwrap_err();
        assert!(!is_transport(&protocol));

        let server = FakeServer::start_raw(vec![
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\ninvalid-size\r\n".into(),
        ]);
        let framing = reqwest::get(server.base())
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap_err();
        assert!(!is_transport(&framing));
    }

    // Two transport failures, then the answer: three attempts.
    #[tokio::test]
    async fn transport_failures_are_retried() {
        let calls = AtomicU32::new(0);
        let got = retry_paced(Duration::ZERO, || {
            let n = calls.fetch_add(1, Ordering::Relaxed).saturating_add(1);
            async move {
                if n < 3 {
                    return Err(Error::network("reset by peer"));
                }
                Ok(n)
            }
        })
        .await
        .expect("third attempt answers");
        assert_eq!(got, 3);
    }

    // Every attempt failing: the last error surfaces, after RETRIES + 1 attempts.
    #[tokio::test]
    async fn a_persistent_transport_failure_surfaces_after_the_last_attempt() {
        let calls = AtomicU32::new(0);
        let err = retry_paced(Duration::ZERO, || {
            let n = calls.fetch_add(1, Ordering::Relaxed).saturating_add(1);
            async move { Err::<(), _>(Error::network(format!("attempt {n}"))) }
        })
        .await
        .expect_err("never answers");
        assert!(err.is_unreachable());
        assert_eq!(calls.load(Ordering::Relaxed), RETRIES.saturating_add(1));
        assert_eq!(err.to_string(), "ocicas: attempt 4");
    }

    // A registry answer — here a miss or a refusal, mapped to Format — is final.
    #[tokio::test]
    async fn a_registry_answer_is_not_retried() {
        let calls = AtomicU32::new(0);
        let err = retry_paced(Duration::ZERO, || {
            calls.fetch_add(1, Ordering::Relaxed);
            async { Err::<(), _>(Error::format("HTTP 401")) }
        })
        .await
        .expect_err("refused");
        assert!(!err.is_unreachable());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}
