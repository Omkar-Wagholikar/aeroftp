//! Backend-owned loopback redirect for MCP OAuth. The listener, redirect URI,
//! callback parsing and exchange stay here; the frontend only sees an opaque
//! attempt, the public authorization URL and a redacted outcome.
// SPDX-License-Identifier: GPL-3.0-or-later
use super::lifecycle::{self, PendingAuthorizationManager};
use super::*;
use std::time::Duration;
use tauri::AppHandle;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;

const LISTEN_FOR: Duration = Duration::from_secs(300);
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REQUEST_HEAD: usize = 8192;
const MAX_HEADERS: usize = 64;
const MAX_STRAY_REQUESTS: usize = 16;

pub(crate) struct Loopback {
    listener: TcpListener,
    port: u16,
}

#[derive(Debug, PartialEq, Eq)]
enum Request {
    /// Full callback URL rebuilt from the bound origin and the request target.
    Callback(String),
    Stray(&'static str),
}

impl Loopback {
    pub(crate) async fn bind() -> Result<Self, OAuthError> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|_| OAuthError::PendingUnavailable)?;
        let port = listener
            .local_addr()
            .map_err(|_| OAuthError::PendingUnavailable)?
            .port();
        Ok(Self { listener, port })
    }

    pub(crate) fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/callback", self.port)
    }

    /// Waits for the one request whose single `state` matches. Stray requests
    /// are answered and ignored within a fixed budget so a local process cannot
    /// consume the pending record; anything else ends the attempt.
    async fn accept_callback(
        &self,
        is_ours: impl Fn(&str) -> bool,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<(TcpStream, String), OAuthError> {
        let mut strays = 0;
        loop {
            let (mut stream, _) = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(HttpError::Cancelled.into()),
                _ = tokio::time::sleep_until(deadline) => return Err(HttpError::Timeout.into()),
                accepted = self.listener.accept() => accepted.map_err(|_| OAuthError::PendingUnavailable)?,
            };
            let read_until = deadline.min(Instant::now() + READ_TIMEOUT);
            let request = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(HttpError::Cancelled.into()),
                result = tokio::time::timeout_at(read_until, read_head(&mut stream)) => match result {
                    Ok(Ok(head)) => parse_request(&head, self.port, &is_ours),
                    Ok(Err(reason)) => Request::Stray(reason),
                    Err(_) => Request::Stray("408 Request Timeout"),
                },
            };
            match request {
                Request::Callback(url) => return Ok((stream, url)),
                Request::Stray(status) => {
                    respond(&mut stream, status, STRAY_PAGE).await;
                    strays += 1;
                    if strays >= MAX_STRAY_REQUESTS {
                        return Err(OAuthError::InvalidCallback);
                    }
                }
            }
        }
    }
}

async fn read_head(stream: &mut TcpStream) -> Result<Vec<u8>, &'static str> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|_| "400 Bad Request")?;
        if read == 0 {
            return Err("400 Bad Request");
        }
        head.extend_from_slice(&chunk[..read]);
        if let Some(end) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            if end > MAX_REQUEST_HEAD {
                return Err("431 Request Header Fields Too Large");
            }
            // Any byte after the head would be a body; GET callbacks carry none.
            if end + 4 != head.len() {
                return Err("400 Bad Request");
            }
            head.truncate(end);
            return Ok(head);
        }
        if head.len() > MAX_REQUEST_HEAD {
            return Err("431 Request Header Fields Too Large");
        }
    }
}

fn parse_request(head: &[u8], port: u16, is_ours: &impl Fn(&str) -> bool) -> Request {
    let Ok(head) = std::str::from_utf8(head) else {
        return Request::Stray("400 Bad Request");
    };
    let mut lines = head.split("\r\n");
    let mut line = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (line.next(), line.next(), line.next(), line.next())
    else {
        return Request::Stray("400 Bad Request");
    };
    if method != "GET" {
        return Request::Stray("405 Method Not Allowed");
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Request::Stray("400 Bad Request");
    }
    let expected_host = format!("127.0.0.1:{port}");
    let mut hosts = 0;
    for (count, header) in lines.enumerate() {
        let Some((name, value)) = header.split_once(':') else {
            return Request::Stray("400 Bad Request");
        };
        if count >= MAX_HEADERS || name.is_empty() || name.contains(char::is_whitespace) {
            return Request::Stray("400 Bad Request");
        }
        let value = value.trim();
        if name.eq_ignore_ascii_case("host") {
            hosts += 1;
            if value != expected_host {
                return Request::Stray("400 Bad Request");
            }
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            || (name.eq_ignore_ascii_case("content-length") && value != "0")
        {
            return Request::Stray("400 Bad Request");
        }
    }
    if hosts != 1 {
        return Request::Stray("400 Bad Request");
    }
    let Some(query) = target.strip_prefix("/callback?") else {
        return Request::Stray("404 Not Found");
    };
    let mut states = url::form_urlencoded::parse(query.as_bytes())
        .filter(|(key, _)| key == "state")
        .map(|(_, value)| value);
    match (states.next(), states.next()) {
        (Some(state), None) if is_ours(&state) => {
            Request::Callback(format!("http://{expected_host}{target}"))
        }
        _ => Request::Stray("400 Bad Request"),
    }
}

const STRAY_PAGE: &str = "This is not the authorization AeroFTP is waiting for.";
const DONE_PAGE: &str = "AeroFTP received the authorization. You can close this tab.";
const FAILED_PAGE: &str =
    "AeroFTP could not complete the authorization. Return to AeroFTP for details.";

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = tokio::time::timeout(READ_TIMEOUT, async {
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
        // Drain what the peer already sent so closing does not reset the reply.
        let mut sink = [0u8; 1024];
        let mut drained = 0;
        while drained < 64 * 1024 {
            match stream.read(&mut sink).await {
                Ok(0) | Err(_) => break,
                Ok(read) => drained += read,
            }
        }
    })
    .await;
}

/// Opens only an HTTPS authorization endpoint already validated by discovery.
pub(crate) fn open_in_browser(url: &Url) -> Result<(), OAuthError> {
    if url.scheme() != "https" || !matches!(url.host(), Some(url::Host::Domain(_))) {
        return Err(OAuthError::InvalidMetadata);
    }
    open::that_detached(url.as_str()).map_err(|_| OAuthError::PendingUnavailable)
}

/// One interactive authorization. `started` receives the public browser URL and
/// whether the browser opened, once the backend owns the pending record; the listener then lives until
/// success, denial, an invalid callback, cancellation, invalidation or timeout.
/// The callback is consumed at most once, so no second exchange is possible.
pub(crate) async fn authorize(
    app: &AppHandle,
    manager: &PendingAuthorizationManager,
    server_id: &str,
    operation: &CancellationToken,
    open: impl FnOnce(&Url) -> Result<(), OAuthError>,
    started: impl FnOnce(&Url, bool),
) -> Result<(), OAuthError> {
    let loopback = Loopback::bind().await?;
    let challenge = lifecycle::acquire_challenge(app, server_id, operation).await?;
    let (handle, url) = lifecycle::start(
        app,
        manager,
        server_id,
        &challenge,
        &loopback.redirect_uri(),
        &[],
        operation,
    )
    .await?;
    let outcome = async {
        let record_cancel = manager
            .pending_cancel(&handle)
            .ok_or(OAuthError::PendingUnavailable)?;
        // A browser that cannot be opened is not fatal: the URL is public.
        started(&url, open(&url).is_ok());
        tokio::select! {
            biased;
            _ = record_cancel.cancelled() => Err(HttpError::Cancelled.into()),
            result = loopback.accept_callback(
                |state| manager.pending_state_matches(&handle, state),
                operation,
                Instant::now() + LISTEN_FOR,
            ) => result,
        }
    }
    .await;
    let (mut stream, callback_url) = match outcome {
        Ok(found) => found,
        Err(error) => {
            manager.discard(&handle);
            return Err(error);
        }
    };
    drop(loopback);
    let result = lifecycle::complete(
        app,
        manager,
        &handle,
        &Zeroizing::new(callback_url),
        operation,
    )
    .await;
    let (status, page) = if result.is_ok() {
        ("200 OK", DONE_PAGE)
    } else {
        ("400 Bad Request", FAILED_PAGE)
    };
    respond(&mut stream, status, page).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(head: &str, ours: &str) -> Request {
        parse_request(head.as_bytes(), 4242, &|state: &str| state == ours)
    }

    #[test]
    fn only_one_bounded_get_on_the_bound_origin_and_path_is_a_callback() {
        let ok = "GET /callback?code=c&state=s HTTP/1.1\r\nHost: 127.0.0.1:4242";
        assert_eq!(
            parse(ok, "s"),
            Request::Callback("http://127.0.0.1:4242/callback?code=c&state=s".into())
        );
        for (head, status) in [
            ("GET /callback?code=c&state=x HTTP/1.1\r\nHost: 127.0.0.1:4242", "400 Bad Request"),
            ("GET /callback?code=c HTTP/1.1\r\nHost: 127.0.0.1:4242", "400 Bad Request"),
            ("GET /callback?state=s&state=s HTTP/1.1\r\nHost: 127.0.0.1:4242", "400 Bad Request"),
            ("GET /favicon.ico HTTP/1.1\r\nHost: 127.0.0.1:4242", "404 Not Found"),
            ("GET /callbackx?state=s HTTP/1.1\r\nHost: 127.0.0.1:4242", "404 Not Found"),
            ("POST /callback?state=s HTTP/1.1\r\nHost: 127.0.0.1:4242", "405 Method Not Allowed"),
            ("GET /callback?state=s HTTP/1.1\r\nHost: localhost:4242", "400 Bad Request"),
            ("GET /callback?state=s HTTP/1.1\r\nHost: 127.0.0.1:1", "400 Bad Request"),
            ("GET /callback?state=s HTTP/1.1", "400 Bad Request"),
            (
                "GET /callback?state=s HTTP/1.1\r\nHost: 127.0.0.1:4242\r\nHost: 127.0.0.1:4242",
                "400 Bad Request",
            ),
            (
                "GET /callback?state=s HTTP/1.1\r\nHost: 127.0.0.1:4242\r\nContent-Length: 4",
                "400 Bad Request",
            ),
            (
                "GET /callback?state=s HTTP/1.1\r\nHost: 127.0.0.1:4242\r\nTransfer-Encoding: chunked",
                "400 Bad Request",
            ),
            ("GET /callback?state=s HTTP/2\r\nHost: 127.0.0.1:4242", "400 Bad Request"),
            ("GET  /callback?state=s HTTP/1.1\r\nHost: 127.0.0.1:4242", "400 Bad Request"),
        ] {
            assert_eq!(parse(head, "s"), Request::Stray(status), "{head}");
        }
        let many = format!(
            "GET /callback?state=s HTTP/1.1\r\nHost: 127.0.0.1:4242{}",
            "\r\nX-A: b".repeat(MAX_HEADERS)
        );
        assert_eq!(parse(&many, "s"), Request::Stray("400 Bad Request"));
    }

    async fn send(port: u16, request: &[u8]) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(request).await.unwrap();
        let mut reply = Vec::new();
        let _ = stream.read_to_end(&mut reply).await;
        String::from_utf8_lossy(&reply).into_owned()
    }

    #[tokio::test]
    async fn listener_ignores_strays_and_returns_the_matching_callback_once() {
        let loopback = Loopback::bind().await.unwrap();
        let port = loopback.port;
        assert_eq!(
            loopback.redirect_uri(),
            format!("http://127.0.0.1:{port}/callback")
        );
        let cancel = CancellationToken::new();
        let client = tokio::spawn(async move {
            let stray = send(
                port,
                b"GET /callback?state=wrong HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            )
            .await;
            let oversized = send(
                port,
                format!(
                    "GET /callback?state=s HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX: {}\r\n\r\n",
                    "a".repeat(MAX_REQUEST_HEAD)
                )
                .as_bytes(),
            )
            .await;
            let body = send(
                port,
                format!("POST /callback?state=s HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n")
                    .as_bytes(),
            )
            .await;
            let mut real = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            real.write_all(
                format!("GET /callback?code=c&state=s HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
            (stray, oversized, body, real)
        });
        let (mut stream, url) = loopback
            .accept_callback(
                |s| s == "s",
                &cancel,
                Instant::now() + Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert_eq!(
            url,
            format!("http://127.0.0.1:{port}/callback?code=c&state=s")
        );
        respond(&mut stream, "200 OK", DONE_PAGE).await;
        let (stray, oversized, body, mut real) = client.await.unwrap();
        assert!(stray.starts_with("HTTP/1.1 400"), "{stray}");
        assert!(oversized.starts_with("HTTP/1.1 431"), "{oversized}");
        assert!(body.starts_with("HTTP/1.1 405"), "{body}");
        let mut reply = String::new();
        real.read_to_string(&mut reply).await.unwrap();
        assert!(reply.starts_with("HTTP/1.1 200") && reply.ends_with(DONE_PAGE));
    }

    #[tokio::test]
    async fn listener_ends_on_cancel_deadline_and_stray_budget() {
        let loopback = Loopback::bind().await.unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            loopback
                .accept_callback(|_| true, &cancel, Instant::now() + Duration::from_secs(10))
                .await
                .unwrap_err(),
            OAuthError::Http(HttpError::Cancelled)
        );
        assert_eq!(
            loopback
                .accept_callback(|_| true, &CancellationToken::new(), Instant::now())
                .await
                .unwrap_err(),
            OAuthError::Http(HttpError::Timeout)
        );
        let port = loopback.port;
        let flood = tokio::spawn(async move {
            for _ in 0..MAX_STRAY_REQUESTS {
                send(port, b"GET /favicon.ico HTTP/1.1\r\n\r\n").await;
            }
        });
        assert_eq!(
            loopback
                .accept_callback(
                    |_| true,
                    &CancellationToken::new(),
                    Instant::now() + Duration::from_secs(10)
                )
                .await
                .unwrap_err(),
            OAuthError::InvalidCallback
        );
        flood.await.unwrap();
    }

    #[test]
    fn browser_opens_only_public_https_authorization_endpoints() {
        for url in [
            "http://auth.example.com/authorize",
            "https://127.0.0.1/authorize",
            "file:///etc/passwd",
        ] {
            assert_eq!(
                open_in_browser(&Url::parse(url).unwrap()),
                Err(OAuthError::InvalidMetadata)
            );
        }
    }
}
