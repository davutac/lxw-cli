//! HTTP client (blocking `ureq`, rustls + ring, Mozilla roots built in):
//! auth header, shared rate limiting, retries and error mapping.
//!
//! Retry policy (writes are never repeated unless the request provably did
//! not reach Lexware):
//! - 429: always retried (Lexware does not execute rate-limited calls); honors
//!   `Retry-After`, otherwise exponential backoff with jitter. The shared rate
//!   limiter is pushed back so parallel processes slow down too.
//! - resolve/connect failures (nothing sent): retried for every method.
//! - other network errors, timeouts and 500/502/503/504: retried for GET only.
//! - 401 with OAuth: one token refresh, then retry.

use crate::auth::Session;
use crate::cli::Globals;
use crate::config;
use crate::error::{CliError, Kind, RequestContext};
use crate::ratelimit::RateLimiter;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Value, json};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};
pub use ureq::http::Method;
use ureq::tls::{RootCerts, TlsConfig};
use ureq::{Agent, Body, Timeout};

pub type Response = ureq::http::Response<Body>;

/// Largest JSON response read into memory (Lexware pages are far smaller).
const MAX_JSON_BYTES: u64 = 64 << 20;

/// Query values: encode everything except RFC 3986 unreserved characters and
/// `,` (Lexware uses comma lists such as `voucherType=invoice,creditnote`).
const QUERY_VALUE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~')
    .remove(b',');
/// Path segments: unreserved characters only.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');

pub fn encode_segment(s: &str) -> String {
    utf8_percent_encode(s, PATH_SEGMENT).to_string()
}

pub enum Payload {
    None,
    Json(Value),
    Multipart {
        file: PathBuf,
        fields: Vec<(String, String)>,
    },
}

pub struct Request {
    pub method: Method,
    /// Already includes substituted path parameters, e.g. `/v1/invoices/<uuid>`.
    pub path: String,
    pub query: Vec<(String, String)>,
    pub payload: Payload,
    pub accept: String,
    pub operation: Option<String>,
    pub partner_only: bool,
}

impl Request {
    pub fn get(path: impl Into<String>, operation: Option<&str>) -> Request {
        Request {
            method: Method::GET,
            path: path.into(),
            query: Vec::new(),
            payload: Payload::None,
            accept: "application/json".into(),
            operation: operation.map(str::to_string),
            partner_only: false,
        }
    }

    pub fn path_and_query(&self) -> String {
        if self.query.is_empty() {
            return self.path.clone();
        }
        let qs: Vec<String> = self
            .query
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    utf8_percent_encode(k, QUERY_VALUE),
                    utf8_percent_encode(v, QUERY_VALUE)
                )
            })
            .collect();
        format!("{}?{}", self.path, qs.join("&"))
    }

    pub fn url(&self, base_url: &str) -> String {
        format!("{}{}", base_url.trim_end_matches('/'), self.path_and_query())
    }

    pub fn context(&self) -> RequestContext {
        RequestContext {
            operation: self.operation.clone(),
            method: self.method.to_string(),
            path: self.path_and_query(),
            partner_only: self.partner_only,
        }
    }

    pub fn set_query(&mut self, key: &str, value: String) {
        self.query.retain(|(k, _)| k != key);
        self.query.push((key.to_string(), value));
    }
}

pub struct Client {
    http: Agent,
    pub session: Session,
    limiter: RateLimiter,
    max_retries: u32,
    verbose: bool,
    last_retries: u32,
}

/// Blocking HTTP agent. TLS trusts Mozilla's root certificates compiled into the
/// binary, so it works without system CA files; `SSL_CERT_FILE` (PEM bundle)
/// replaces them, e.g. for a TLS-intercepting corporate proxy.
pub fn http_agent(timeout: Duration) -> Result<Agent, CliError> {
    let mut tls = TlsConfig::builder();
    if let Some(path) = std::env::var_os("SSL_CERT_FILE").filter(|p| !p.is_empty()) {
        let pem = std::fs::read(&path)
            .map_err(|e| CliError::usage(format!("cannot read SSL_CERT_FILE {}: {e}", path.to_string_lossy())))?;
        let certs: Vec<_> = ureq::tls::parse_pem(&pem)
            .filter_map(|item| match item {
                Ok(ureq::tls::PemItem::Certificate(cert)) => Some(cert),
                _ => None,
            })
            .collect();
        if certs.is_empty() {
            return Err(CliError::usage("SSL_CERT_FILE contains no PEM certificates"));
        }
        tls = tls.root_certs(RootCerts::new_with_certs(&certs));
    }
    Ok(Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .timeout_connect(Some(Duration::from_secs(15)))
        .user_agent(concat!("lxw-cli/", env!("CARGO_PKG_VERSION")))
        .tls_config(tls.build())
        .build()
        .new_agent())
}

/// Content type and bytes of a request body, built once per request (not per retry).
fn encode_payload(payload: &Payload) -> Result<Option<(String, Vec<u8>)>, CliError> {
    match payload {
        Payload::None => Ok(None),
        Payload::Json(v) => {
            let bytes = serde_json::to_vec(v).map_err(|e| CliError::internal(e.to_string()))?;
            Ok(Some(("application/json".into(), bytes)))
        }
        Payload::Multipart { file, fields } => multipart(file, fields).map(Some),
    }
}

/// `multipart/form-data` with the text `fields` and the file as part `file`.
fn multipart(file: &Path, fields: &[(String, String)]) -> Result<(String, Vec<u8>), CliError> {
    let content =
        std::fs::read(file).map_err(|e| CliError::new(Kind::Io, format!("cannot read {}: {e}", file.display())))?;
    let boundary = format!("lxw-cli-{:016x}", getrandom::u64().unwrap_or(0x5eed));
    let file_name = file
        .file_name()
        .map(|n| n.to_string_lossy().replace(['"', '\r', '\n'], "_"))
        .unwrap_or_else(|| "upload".into());
    let mime = match file
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("pdf") => "application/pdf",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("xml") => "application/xml",
        _ => "application/octet-stream",
    };
    let mut body = Vec::with_capacity(content.len() + 512);
    for (name, value) in fields {
        let _ = write!(
            body,
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        );
    }
    let _ = write!(
        body,
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\nContent-Type: {mime}\r\n\r\n"
    );
    body.extend_from_slice(&content);
    let _ = write!(body, "\r\n--{boundary}--\r\n");
    Ok((format!("multipart/form-data; boundary={boundary}"), body))
}

impl Client {
    pub fn new(session: Session, g: &Globals) -> Result<Client, CliError> {
        let http = http_agent(Duration::from_secs(g.timeout_secs))?;
        let state = config::cache_dir().join("ratelimit").join(session.limiter_key());
        let limiter = RateLimiter::new(Some(state), session.requests_per_second, session.burst);
        Ok(Client {
            http,
            session,
            limiter,
            max_retries: g.max_retries,
            verbose: g.verbose,
            last_retries: 0,
        })
    }

    /// Lexware's Fair Usage Policy allows at most 20 outgoing vouchers (invoices,
    /// quotations, ...) per minute. One every 3 s, shared by all processes using
    /// this credential, can never exceed that in any minute.
    pub fn acquire_outgoing_voucher_quota(&self) {
        let path = config::cache_dir()
            .join("ratelimit")
            .join(format!("{}-outgoing-vouchers", self.session.limiter_key()));
        let waited = RateLimiter::new(Some(path), 20.0 / 60.0, 1).acquire();
        if !waited.is_zero() {
            self.log(format!(
                "fair usage: waited {} ms (max. 20 outgoing vouchers per minute)",
                waited.as_millis()
            ));
        }
    }

    fn pause(&self, attempt: u32, reason: impl std::fmt::Display, delay: Duration) {
        self.log(format!(
            "retry {attempt}/{} after {reason} in {} ms",
            self.max_retries,
            delay.as_millis()
        ));
        sleep(delay);
    }

    fn log(&self, msg: impl AsRef<str>) {
        if self.verbose {
            eprintln!("lxw: {}", msg.as_ref());
        }
    }

    /// Sends the request with rate limiting and retries. Non-2xx responses are
    /// returned as-is; use [`Client::json`] or check the status yourself.
    pub fn execute(&mut self, req: &Request) -> Result<Response, CliError> {
        let url = req.url(&self.session.base_url);
        let idempotent = matches!(req.method, Method::GET | Method::HEAD);
        let body = encode_payload(&req.payload)?;
        let mut attempt = 0u32;
        let mut refreshed = false;
        loop {
            let waited = self.limiter.acquire();
            if !waited.is_zero() {
                self.log(format!("rate limit: waited {} ms", waited.as_millis()));
            }
            let token = self.session.bearer(&self.http)?;
            let builder = ureq::http::Request::builder()
                .method(req.method.clone())
                .uri(&url)
                .header("Authorization", format!("Bearer {token}"))
                .header("Accept", &req.accept);
            self.log(format!("-> {} {}", req.method, req.path_and_query()));
            let started = Instant::now();
            let result = match &body {
                None => builder.body(()).map(|r| self.http.run(r)),
                Some((content_type, bytes)) => builder
                    .header("Content-Type", content_type)
                    .body(bytes.as_slice())
                    .map(|r| self.http.run(r)),
            }
            .map_err(|e| CliError::internal(format!("invalid request: {e}")))?;
            match result {
                Err(e) => {
                    let safe =
                        not_sent(&e) || (idempotent && matches!(e, ureq::Error::Timeout(_) | ureq::Error::Io(_)));
                    if safe && attempt < self.max_retries {
                        attempt += 1;
                        self.pause(attempt, format!("network error: {e}"), backoff(attempt));
                        continue;
                    }
                    return Err(network_error(&e, req));
                }
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    self.log(format!("<- {status} in {} ms", started.elapsed().as_millis()));
                    if status == 401 && !refreshed && self.session.can_refresh() {
                        refreshed = true;
                        self.log("401: refreshing OAuth access token");
                        self.session.refresh(&self.http)?;
                        continue;
                    }
                    let retryable = status == 429 || (idempotent && matches!(status, 500 | 502 | 503 | 504));
                    if retryable && attempt < self.max_retries {
                        attempt += 1;
                        let delay = if status == 429 {
                            // Push the shared schedule back so parallel processes slow down too.
                            let delay = retry_after(&resp).unwrap_or_else(|| backoff(attempt));
                            self.limiter.penalize(delay);
                            delay
                        } else {
                            backoff(attempt)
                        };
                        self.pause(attempt, status, delay);
                        continue;
                    }
                    self.last_retries = attempt;
                    return Ok(resp);
                }
            }
        }
    }

    /// Sends the request and returns the JSON body, or a normalized error.
    pub fn json(&mut self, req: &Request) -> Result<Value, CliError> {
        let mut resp = self.execute(req)?;
        let status = resp.status().as_u16();
        let bytes = read_body(&mut resp, req)?;
        if !(200..300).contains(&status) {
            return Err(self.api_error(status, &bytes, req));
        }
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(json!({ "ok": true, "status": status }));
        }
        Ok(serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({ "ok": true, "status": status, "body": String::from_utf8_lossy(&bytes) })))
    }

    pub fn api_error(&self, status: u16, body: &[u8], req: &Request) -> CliError {
        CliError::from_api(status, body, &req.context(), self.last_retries)
    }
}

/// Reads a (JSON or error) response body into memory.
pub fn read_body(resp: &mut Response, req: &Request) -> Result<Vec<u8>, CliError> {
    resp.body_mut()
        .with_config()
        .limit(MAX_JSON_BYTES)
        .read_to_vec()
        .map_err(|e| network_error(&e, req))
}

/// Failures that happen before anything was sent, so every method may retry.
fn not_sent(e: &ureq::Error) -> bool {
    matches!(
        e,
        ureq::Error::HostNotFound
            | ureq::Error::ConnectionFailed
            | ureq::Error::Timeout(Timeout::Resolve | Timeout::Connect)
    )
}

fn network_error(e: &ureq::Error, req: &Request) -> CliError {
    let timeout = matches!(e, ureq::Error::Timeout(_));
    let mut err = CliError::new(if timeout { Kind::Timeout } else { Kind::Network }, e.to_string());
    err.context = Some(Box::new(req.context()));
    err.retryable = req.method == Method::GET || not_sent(e);
    if err.retryable {
        err.with_hint("Check network connectivity and the base URL (`lxw auth status`).")
    } else {
        err.with_hint("The request may have reached Lexware before failing and MAY have been processed: verify before retrying to avoid duplicates.")
    }
}

/// Exponential backoff with jitter: ~1 s, 2 s, 4 s ... capped at 30 s.
fn backoff(attempt: u32) -> Duration {
    let base = 1000u64
        .saturating_mul(1 << attempt.saturating_sub(1).min(5))
        .min(30_000);
    let jitter = getrandom::u32().unwrap_or(0) as u64 % (base / 2 + 1);
    Duration::from_millis(base / 2 + jitter)
}

fn retry_after(resp: &Response) -> Option<Duration> {
    let secs: u64 = resp.headers().get("retry-after")?.to_str().ok()?.trim().parse().ok()?;
    Some(Duration::from_secs(secs.clamp(1, 60)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_encoding_matches_lexware_expectations() {
        let mut req = Request::get("/v1/contacts", None);
        req.query.push(("name".into(), "johnson &amp; partner".into()));
        req.query.push(("voucherType".into(), "invoice,creditnote".into()));
        assert_eq!(
            req.path_and_query(),
            "/v1/contacts?name=johnson%20%26amp%3B%20partner&voucherType=invoice,creditnote"
        );
        assert_eq!(
            req.url("https://api.lexware.io/"),
            format!("https://api.lexware.io{}", req.path_and_query())
        );
    }

    #[test]
    fn multipart_body_has_fields_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Beleg \"1\".pdf");
        std::fs::write(&file, b"%PDF-1.4 x").unwrap();
        let (ct, body) = multipart(&file, &[("type".into(), "voucher".into())]).unwrap();
        let boundary = ct.strip_prefix("multipart/form-data; boundary=").unwrap();
        let body = String::from_utf8(body).unwrap();
        assert!(body.starts_with(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"type\"\r\n\r\nvoucher\r\n"
        )));
        assert!(body.contains("filename=\"Beleg _1_.pdf\"\r\nContent-Type: application/pdf\r\n\r\n%PDF-1.4 x\r\n"));
        assert!(body.ends_with(&format!("--{boundary}--\r\n")));
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        for attempt in 1..10 {
            let d = backoff(attempt);
            assert!(d <= Duration::from_secs(30));
            assert!(d >= Duration::from_millis(500));
        }
        assert!(backoff(4) >= Duration::from_secs(4));
    }
}
