//! Error model: every failure becomes a typed JSON error on stderr with a
//! stable exit code, so agents can branch on `error.type` or `$?`.

use serde_json::{Map, Value, json};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Usage,
    ConfirmationRequired,
    AuthMissing,
    Unauthorized,
    Forbidden,
    PaymentRequired,
    NotFound,
    MethodNotAllowed,
    BadRequest,
    Validation,
    Conflict,
    UnsupportedMediaType,
    RateLimited,
    Server,
    GatewayTimeout,
    Network,
    Timeout,
    Io,
    Internal,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Usage => "usage",
            Kind::ConfirmationRequired => "confirmation_required",
            Kind::AuthMissing => "auth_missing",
            Kind::Unauthorized => "unauthorized",
            Kind::Forbidden => "forbidden",
            Kind::PaymentRequired => "payment_required",
            Kind::NotFound => "not_found",
            Kind::MethodNotAllowed => "method_not_allowed",
            Kind::BadRequest => "bad_request",
            Kind::Validation => "validation_failed",
            Kind::Conflict => "conflict",
            Kind::UnsupportedMediaType => "unsupported_media_type",
            Kind::RateLimited => "rate_limited",
            Kind::Server => "server_error",
            Kind::GatewayTimeout => "gateway_timeout",
            Kind::Network => "network_error",
            Kind::Timeout => "timeout",
            Kind::Io => "io_error",
            Kind::Internal => "internal_error",
        }
    }

    /// Exit codes are part of the CLI contract (see `lxw cli guide`).
    pub fn exit_code(self) -> i32 {
        match self {
            Kind::Internal | Kind::Io => 1,
            Kind::Usage => 2,
            Kind::AuthMissing | Kind::Unauthorized | Kind::Forbidden => 3,
            Kind::NotFound => 4,
            Kind::BadRequest | Kind::Validation | Kind::UnsupportedMediaType | Kind::MethodNotAllowed => 5,
            Kind::Conflict => 6,
            Kind::RateLimited => 7,
            Kind::Server | Kind::GatewayTimeout | Kind::Network | Kind::Timeout => 8,
            Kind::PaymentRequired => 9,
            Kind::ConfirmationRequired => 10,
        }
    }

    pub fn from_status(status: u16) -> Kind {
        match status {
            400 => Kind::BadRequest,
            401 => Kind::Unauthorized,
            402 => Kind::PaymentRequired,
            403 => Kind::Forbidden,
            404 => Kind::NotFound,
            405 | 501 => Kind::MethodNotAllowed,
            406 | 422 => Kind::Validation,
            409 => Kind::Conflict,
            415 => Kind::UnsupportedMediaType,
            429 => Kind::RateLimited,
            504 => Kind::GatewayTimeout,
            500..=599 => Kind::Server,
            _ => Kind::Internal,
        }
    }
}

/// Where a failed request was going; included in API errors.
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub operation: Option<String>,
    pub method: String,
    pub path: String,
    pub partner_only: bool,
}

#[derive(Debug)]
pub struct CliError {
    pub kind: Kind,
    pub message: String,
    pub status: Option<u16>,
    pub details: Vec<Value>,
    pub trace_id: Option<String>,
    pub hint: Option<String>,
    pub retryable: bool,
    /// Boxed to keep `Result<_, CliError>` small.
    pub context: Option<Box<RequestContext>>,
}

impl CliError {
    pub fn new(kind: Kind, message: impl Into<String>) -> Self {
        CliError {
            kind,
            message: message.into(),
            status: None,
            details: Vec::new(),
            trace_id: None,
            hint: None,
            retryable: false,
            context: None,
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        CliError::new(Kind::Usage, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        CliError::new(Kind::Internal, message)
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn exit_code(&self) -> i32 {
        self.kind.exit_code()
    }

    /// Normalizes the three Lexware error shapes (regular, legacy `IssueList`,
    /// gateway `{message}`) plus OAuth2 errors into one structure.
    pub fn from_api(status: u16, body: &[u8], ctx: &RequestContext, retries: u32) -> Self {
        let kind = Kind::from_status(status);
        let parsed: Option<Value> = serde_json::from_slice(body).ok();
        let mut message: Option<String> = None;
        let mut details = Vec::new();
        let mut trace_id = None;

        if let Some(v) = &parsed {
            if let Some(issues) = v.get("IssueList").and_then(Value::as_array) {
                let mut parts = Vec::new();
                for issue in issues {
                    let key = str_field(issue, "i18nKey");
                    let source = str_field(issue, "source");
                    parts.push(match (&key, &source) {
                        (Some(k), Some(s)) => format!("{k} ({s})"),
                        (Some(k), None) => k.clone(),
                        (None, Some(s)) => s.clone(),
                        (None, None) => "unknown issue".to_string(),
                    });
                    let mut d = Map::new();
                    insert_some(&mut d, "field", issue.get("source"));
                    insert_some(&mut d, "violation", issue.get("i18nKey"));
                    insert_some(&mut d, "type", issue.get("type"));
                    insert_some(&mut d, "additionalData", issue.get("additionalData"));
                    insert_some(&mut d, "args", issue.get("args"));
                    details.push(Value::Object(d));
                }
                message = Some(parts.join("; "));
            } else {
                message = str_field(v, "message")
                    .or_else(|| str_field(v, "error_description"))
                    .or_else(|| str_field(v, "error"));
                if let Some(d) = v.get("details").and_then(Value::as_array) {
                    details = d.clone();
                }
                trace_id = str_field(v, "traceId");
            }
        }

        let message = message.unwrap_or_else(|| {
            let text = String::from_utf8_lossy(body);
            let text = text.trim();
            if text.is_empty() {
                format!("HTTP {status}")
            } else {
                text.chars().take(300).collect()
            }
        });

        let is_get = ctx.method == "GET";
        let retryable = matches!(status, 429 | 502 | 503) || (matches!(status, 500 | 504) && is_get);
        let hint = hint_for(status, &message, ctx, retries);

        CliError {
            kind,
            message,
            status: Some(status),
            details,
            trace_id,
            hint,
            retryable,
            context: Some(Box::new(ctx.clone())),
        }
    }

    pub fn to_json(&self) -> Value {
        let mut e = Map::new();
        e.insert("type".into(), json!(self.kind.as_str()));
        e.insert("message".into(), json!(self.message));
        if let Some(s) = self.status {
            e.insert("status".into(), json!(s));
        }
        if !self.details.is_empty() {
            e.insert("details".into(), Value::Array(self.details.clone()));
        }
        if let Some(t) = &self.trace_id {
            e.insert("traceId".into(), json!(t));
        }
        if let Some(h) = &self.hint {
            e.insert("hint".into(), json!(h));
        }
        e.insert("retryable".into(), json!(self.retryable));
        if let Some(c) = &self.context {
            if let Some(op) = &c.operation {
                e.insert("operation".into(), json!(op));
            }
            e.insert("request".into(), json!(format!("{} {}", c.method, c.path)));
        }
        e.insert("exitCode".into(), json!(self.exit_code()));
        json!({ "error": e })
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.message)
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        CliError::new(Kind::Io, e.to_string())
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn insert_some(map: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(v) = value.filter(|v| !v.is_null()) {
        map.insert(key.to_string(), v.clone());
    }
}

fn hint_for(status: u16, message: &str, ctx: &RequestContext, retries: u32) -> Option<String> {
    let op = ctx.operation.as_deref().unwrap_or("<resource>.<action>");
    let resource = op.split('.').next().unwrap_or(op);
    let command = op.replace('.', " ");
    let partner = ctx.partner_only;
    let hint = match status {
        400 if message.contains("search window") => {
            "More than 10,000 matches: narrow the filters (e.g. a shorter date range).".to_string()
        }
        400 => format!("Malformed request or query. Check the parameters with `lxw schema {command}`."),
        401 => "Credentials missing, invalid, expired (API keys live 24 months) or deleted. Run \
                `lxw auth status`; create a key at https://app.lexware.de/addons/public-api. \
                Lexware outages can also cause 401s: https://status.lexware.de"
            .to_string(),
        402 => "Lexware contract issue: the plan does not include this feature or the \
                subscription is inactive (the Public API requires Lexware Office XL)."
            .to_string(),
        403 if partner => "This operation is documented only for the Lexware Partner API \
                (OAuth2 partner connection); personal API keys cannot call it."
            .to_string(),
        403 => "Authenticated but not permitted: the API key may lack the permission for this \
                resource (create a key with the needed scopes at \
                https://app.lexware.de/addons/public-api) or the Lexware user lacks rights."
            .to_string(),
        404 if partner => "Not found. Note: this operation is documented only for the Partner \
                API (OAuth2); personal API keys may not reach it."
            .to_string(),
        404 => "Not found: wrong or deleted id, or the id belongs to another resource type \
                (voucherlist items: invoice -> `invoices get`, salesinvoice/purchaseinvoice -> \
                `vouchers get`)."
            .to_string(),
        405 | 501 => "This HTTP method is not supported on this resource.".to_string(),
        406 => format!(
            "Validation failed; see details[].field. Required fields: `lxw schema {command}`; \
             field reference: `lxw schema {resource}`. Timestamps must look like \
             2026-01-31T00:00:00.000+01:00."
        ),
        409 if ctx.method == "PUT" => "Conflict: the version you sent is outdated (optimistic \
                locking). Fetch the resource again and retry with its current version; `--merge` \
                does this automatically."
            .to_string(),
        409 => "Conflict: the resource's current state does not allow this (e.g. draft vouchers \
                cannot be downloaded, a cash box with vouchers cannot be deleted). Check its \
                voucherStatus with the matching `get` command."
            .to_string(),
        415 => "Unsupported media type: JSON bodies need Content-Type application/json, uploads \
                need multipart/form-data."
            .to_string(),
        429 => format!(
            "Rate limit (about 2 requests/second per API client) still exceeded after {retries} \
             retries. Wait a few seconds and avoid parallel bursts with the same key."
        ),
        500 => "Lexware internal error (the gateway also reports some rate-limit hits as 500). \
                Retry later; status page: https://status.lexware.de"
            .to_string(),
        502 | 503 => {
            "Lexware is temporarily unavailable. Retry later; status page: https://status.lexware.de".to_string()
        }
        504 if ctx.method != "GET" => "Gateway timeout (30 s). The request MAY still have been \
                processed: verify (e.g. `voucherlist list`, `<resource> get`) before retrying to \
                avoid duplicates."
            .to_string(),
        504 => "Gateway timeout (30 s). Retry later.".to_string(),
        _ => return None,
    };
    Some(hint)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(method: &str) -> RequestContext {
        RequestContext {
            operation: Some("invoices.create".into()),
            method: method.into(),
            path: "/v1/invoices".into(),
            partner_only: false,
        }
    }

    #[test]
    fn parses_regular_error() {
        let body = br#"{"timestamp":"2023-05-11T17:12:31.233+02:00","status":406,"error":"Not Acceptable","path":"/v1/invoices","traceId":"90d78d0777be","message":"Validation failed for request.","details":[{"violation":"NOTNULL","field":"lineItems[0].unitPrice.taxRatePercentage","message":"darf nicht leer sein"}]}"#;
        let e = CliError::from_api(406, body, &ctx("POST"), 0);
        assert_eq!(e.kind, Kind::Validation);
        assert_eq!(e.exit_code(), 5);
        assert_eq!(e.message, "Validation failed for request.");
        assert_eq!(e.trace_id.as_deref(), Some("90d78d0777be"));
        assert_eq!(e.details[0]["field"], "lineItems[0].unitPrice.taxRatePercentage");
        assert!(!e.retryable);
    }

    #[test]
    fn parses_legacy_issue_list() {
        let body = br#"{"IssueList":[{"i18nKey":"missing_entity","source":"company.name","type":"validation_failure","additionalData":null,"args":null}]}"#;
        let e = CliError::from_api(406, body, &ctx("POST"), 0);
        assert_eq!(e.message, "missing_entity (company.name)");
        assert_eq!(e.details[0]["field"], "company.name");
        assert!(e.details[0].get("args").is_none());
    }

    #[test]
    fn parses_gateway_and_plain_errors() {
        let e = CliError::from_api(401, br#"{ "message": "Unauthorized" }"#, &ctx("GET"), 0);
        assert_eq!(e.kind, Kind::Unauthorized);
        assert_eq!(e.exit_code(), 3);
        assert_eq!(e.message, "Unauthorized");

        let e = CliError::from_api(503, b"", &ctx("GET"), 0);
        assert_eq!(e.message, "HTTP 503");
        assert!(e.retryable);
    }

    #[test]
    fn gateway_timeout_on_post_warns_about_duplicates() {
        let e = CliError::from_api(504, br#"{"message":"Endpoint request timed out"}"#, &ctx("POST"), 0);
        assert!(!e.retryable);
        assert!(e.hint.unwrap().contains("MAY still have been processed"));
    }

    #[test]
    fn conflict_hint_depends_on_method() {
        let put = CliError::from_api(409, b"{}", &ctx("PUT"), 0);
        assert!(put.hint.unwrap().contains("--merge"));
        let get = CliError::from_api(409, b"{}", &ctx("GET"), 0);
        assert!(get.hint.unwrap().contains("current state"));
    }

    #[test]
    fn json_shape_is_stable() {
        let e = CliError::from_api(409, b"{}", &ctx("PUT"), 0);
        let v = e.to_json();
        assert_eq!(v["error"]["type"], "conflict");
        assert_eq!(v["error"]["status"], 409);
        assert_eq!(v["error"]["exitCode"], 6);
        assert_eq!(v["error"]["request"], "PUT /v1/invoices");
    }
}
