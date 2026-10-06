//! End-to-end tests: run the real binary against a local mock of the Lexware API.

use serde_json::{Value, json};
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use wiremock::matchers::{body_json, body_string_contains, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const KEY: &str = "test-api-key-123456";
const ID: &str = "e9066f04-8cc7-4616-93f8-ac9ecc8479c8";

struct Env {
    dir: TempDir,
    base_url: String,
}

impl Env {
    fn new(server: &MockServer) -> Env {
        Env {
            dir: tempfile::tempdir().unwrap(),
            base_url: server.uri(),
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_lxw"));
        for (k, _) in std::env::vars() {
            if k.starts_with("LXW_") {
                c.env_remove(k);
            }
        }
        c.env("LXW_CONFIG_DIR", self.dir.path().join("config"))
            .env("LXW_CACHE_DIR", self.dir.path().join("cache"))
            .env("LXW_BASE_URL", &self.base_url)
            .env("LXW_API_KEY", KEY)
            .env("LXW_RATE_LIMIT", "0")
            // Never touch the developer's real Keychain / Secret Service from tests.
            .env("LXW_CREDENTIAL_STORE", "file")
            .current_dir(self.dir.path())
            .args(args);
        c
    }

    async fn run(&self, args: &[&str]) -> Out {
        run_cmd(self.cmd(args)).await
    }
}

/// Runs a prepared command off the async runtime (the mock server keeps serving).
async fn run_cmd(mut c: Command) -> Out {
    Out(tokio::task::spawn_blocking(move || c.output().unwrap()).await.unwrap())
}

struct Out(Output);

impl Out {
    fn code(&self) -> i32 {
        self.0.status.code().unwrap()
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.0.stdout)
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {}\nstderr: {}", self.stdout(), self.stderr()))
    }
    fn err(&self) -> Value {
        serde_json::from_slice(&self.0.stderr).unwrap_or_else(|e| panic!("stderr is not JSON ({e}): {}", self.stderr()))
    }
    fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.0.stdout).into_owned()
    }
    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.0.stderr).into_owned()
    }
}

async fn requests(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn sends_bearer_key_and_prints_json() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/profile"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .and(header("accept", "application/json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"organizationId": ID, "companyName": "Testfirma GmbH"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let env = Env::new(&server);
    let out = env.run(&["profile", "get", "--fields", "companyName"]).await;
    assert_eq!(out.code(), 0, "{}", out.stderr());
    assert_eq!(out.json(), json!({"companyName": "Testfirma GmbH"}));
}

#[tokio::test(flavor = "multi_thread")]
async fn retries_429_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/contacts"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/contacts"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": ID, "version": 1})))
        .mount(&server)
        .await;
    let env = Env::new(&server);
    let started = Instant::now();
    let out = env
        .run(&[
            "contacts",
            "create",
            "--set",
            "version=0",
            "--set",
            "roles.customer={}",
            "--set",
            "person.lastName=Muster",
        ])
        .await;
    assert_eq!(out.code(), 0, "{}", out.stderr());
    assert_eq!(out.json()["id"], ID);
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "Retry-After was not honored"
    );
    let reqs = requests(&server).await;
    assert_eq!(reqs.len(), 2);
    let body: Value = serde_json::from_slice(&reqs[1].body).unwrap();
    assert_eq!(
        body,
        json!({"version": 0, "roles": {"customer": {}}, "person": {"lastName": "Muster"}})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn gives_up_on_persistent_429_with_exit_7() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "1")
                .set_body_json(json!({"message": "Rate limit exceeded"})),
        )
        .mount(&server)
        .await;
    let env = Env::new(&server);
    let out = env.run(&["countries", "list", "--max-retries", "1"]).await;
    assert_eq!(out.code(), 7);
    let err = out.err();
    assert_eq!(err["error"]["type"], "rate_limited");
    assert_eq!(err["error"]["retryable"], true);
    assert_eq!(requests(&server).await.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn validation_errors_are_normalized() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/invoices"))
        .respond_with(ResponseTemplate::new(406).set_body_json(json!({
            "status": 406, "error": "Not Acceptable", "path": "/v1/invoices", "traceId": "abc123",
            "message": "Validation failed for request.",
            "details": [{"violation": "NOTNULL", "field": "lineItems[0].unitPrice.taxRatePercentage", "message": "darf nicht leer sein"}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/contacts"))
        .respond_with(ResponseTemplate::new(406).set_body_json(json!({
            "IssueList": [{"i18nKey": "missing_entity", "source": "company.name", "type": "validation_failure", "additionalData": null, "args": null}]
        })))
        .mount(&server)
        .await;
    let env = Env::new(&server);

    let out = env
        .run(&[
            "invoices",
            "create",
            "--body",
            r#"{"voucherDate":"2026-01-01T00:00:00.000+01:00"}"#,
        ])
        .await;
    assert_eq!(out.code(), 5);
    let e = &out.err()["error"];
    assert_eq!(e["type"], "validation_failed");
    assert_eq!(e["status"], 406);
    assert_eq!(e["traceId"], "abc123");
    assert_eq!(e["details"][0]["field"], "lineItems[0].unitPrice.taxRatePercentage");
    assert_eq!(e["operation"], "invoices.create");
    assert!(e["hint"].as_str().unwrap().contains("lxw schema invoices create"));

    let out = env.run(&["contacts", "create", "--body", r#"{"version":0}"#]).await;
    assert_eq!(out.code(), 5);
    assert_eq!(out.err()["error"]["message"], "missing_entity (company.name)");
}

#[tokio::test(flavor = "multi_thread")]
async fn server_errors_retry_get_but_never_post() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/countries"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/countries"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"countryCode": "DE"}])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/articles"))
        .respond_with(ResponseTemplate::new(504).set_body_json(json!({"message": "Endpoint request timed out"})))
        .mount(&server)
        .await;
    let env = Env::new(&server);

    let out = env.run(&["countries", "list"]).await;
    assert_eq!(out.code(), 0, "{}", out.stderr());
    assert_eq!(out.json()[0]["countryCode"], "DE");

    let out = env.run(&["articles", "create", "--body", r#"{"title":"x"}"#]).await;
    assert_eq!(out.code(), 8);
    let e = &out.err()["error"];
    assert_eq!(e["type"], "gateway_timeout");
    assert!(e["hint"].as_str().unwrap().contains("MAY still have been processed"));
    let posts = requests(&server)
        .await
        .into_iter()
        .filter(|r| r.method.as_str() == "POST")
        .count();
    assert_eq!(posts, 1, "POST must not be retried");
}

#[tokio::test(flavor = "multi_thread")]
async fn fetches_all_pages() {
    let server = MockServer::start().await;
    for page in 0..3 {
        let content: Vec<Value> = (0..2)
            .map(|i| json!({"id": format!("{page}-{i}"), "voucherNumber": format!("RE{page}{i}"), "x": 1}))
            .collect();
        Mock::given(method("GET"))
            .and(path("/v1/voucherlist"))
            .and(query_param("page", page.to_string().as_str()))
            .and(query_param("size", "250"))
            .and(query_param("voucherType", "invoice"))
            .and(query_param("voucherStatus", "open,overdue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": content, "first": page == 0, "last": page == 2, "totalPages": 3, "totalElements": 6, "number": page, "size": 250
            })))
            .mount(&server)
            .await;
    }
    let env = Env::new(&server);
    let out = env
        .run(&[
            "voucherlist",
            "list",
            "--voucher-type",
            "invoice",
            "--voucher-status",
            "open,overdue",
            "--all",
            "--fields",
            "id,voucherNumber",
        ])
        .await;
    assert_eq!(out.code(), 0, "{}", out.stderr());
    let items = out.json();
    assert_eq!(items.as_array().unwrap().len(), 6);
    assert_eq!(items[5], json!({"id": "2-1", "voucherNumber": "RE21"}));
    assert_eq!(requests(&server).await.len(), 3, "each page fetched once");

    let out = env
        .run(&[
            "voucherlist",
            "list",
            "--voucher-type",
            "invoice",
            "--voucher-status",
            "open,overdue",
            "--all",
            "--max-items",
            "3",
            "--ndjson",
        ])
        .await;
    assert_eq!(out.code(), 0, "{}", out.stderr());
    assert_eq!(out.stdout().lines().count(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn search_strings_are_html_escaped_then_url_encoded() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/contacts"))
        .and(query_param("name", "Johnson &amp; Partner"))
        .and(query_param("customer", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"content": [], "last": true})))
        .expect(1)
        .mount(&server)
        .await;
    let env = Env::new(&server);
    let out = env
        .run(&["contacts", "list", "--name", "Johnson & Partner", "--customer", "true"])
        .await;
    assert_eq!(out.code(), 0, "{}", out.stderr());
    let url = requests(&server).await[0].url.to_string();
    assert!(url.contains("name=Johnson%20%26amp%3B%20Partner"), "{url}");
}

#[tokio::test(flavor = "multi_thread")]
async fn downloads_files_with_server_file_name() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/invoices/{ID}/file")))
        .and(header("accept", "application/xml"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "application/xml")
                .insert_header("Content-Disposition", "attachment; filename=\"RE1001.xml\"")
                .set_body_bytes(b"<Invoice/>".to_vec()),
        )
        .mount(&server)
        .await;
    let env = Env::new(&server);
    let out = env
        .run(&[
            "invoices",
            "download",
            ID,
            "--accept",
            "application/xml",
            "--out",
            "docs/",
        ])
        .await;
    assert_eq!(out.code(), 0, "{}", out.stderr());
    let v = out.json();
    assert_eq!(v["bytes"], 10);
    assert_eq!(v["contentType"], "application/xml");
    let file = Path::new(v["file"].as_str().unwrap());
    assert!(file.ends_with("docs/RE1001.xml"), "{file:?}");
    assert_eq!(std::fs::read(file).unwrap(), b"<Invoice/>");

    // Regular (non-XRechnung) invoices have no XML: Lexware answers 404 with an empty body.
    let other = "a7bcbf92-9111-4092-9f03-ebaf46d8ae93";
    Mock::given(method("GET"))
        .and(path(format!("/v1/invoices/{other}/file")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let out = env
        .run(&["invoices", "download", other, "--accept", "application/xml"])
        .await;
    assert_eq!(out.code(), 4);
    assert!(out.err()["error"]["hint"].as_str().unwrap().contains("XRechnung"));
}

#[tokio::test(flavor = "multi_thread")]
async fn uploads_multipart_files() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/files"))
        .and(body_string_contains("name=\"type\""))
        .and(body_string_contains("voucher"))
        .and(body_string_contains("%PDF-1.4 test"))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({"id": ID, "voucherId": ID})))
        .expect(1)
        .mount(&server)
        .await;
    let env = Env::new(&server);
    std::fs::write(env.dir.path().join("receipt.pdf"), b"%PDF-1.4 test").unwrap();
    let out = env.run(&["files", "upload", "--file", "receipt.pdf"]).await;
    assert_eq!(out.code(), 0, "{}", out.stderr());
    assert_eq!(out.json()["voucherId"], ID);

    std::fs::write(env.dir.path().join("notes.txt"), b"x").unwrap();
    let out = env.run(&["files", "upload", "--file", "notes.txt"]).await;
    assert_eq!(out.code(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn irreversible_actions_need_yes() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path(format!("/v1/articles/{ID}")))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/invoices"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": ID})))
        .expect(0)
        .mount(&server)
        .await;
    let env = Env::new(&server);

    let out = env.run(&["articles", "delete", ID]).await;
    assert_eq!(out.code(), 10);
    assert_eq!(out.err()["error"]["type"], "confirmation_required");
    let out = env.run(&["invoices", "create", "--finalize", "--body", "{}"]).await;
    assert_eq!(out.code(), 10);
    assert!(requests(&server).await.is_empty(), "nothing may be sent without --yes");

    let out = env.run(&["articles", "delete", ID, "--yes"]).await;
    assert_eq!(out.code(), 0, "{}", out.stderr());
    assert_eq!(out.json(), json!({"ok": true, "status": 204}));
}

#[tokio::test(flavor = "multi_thread")]
async fn merge_updates_use_current_version() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/contacts/{ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": ID, "version": 7, "roles": {"customer": {"number": 10308}}, "person": {"lastName": "Musterfrau"}, "note": "old"
        })))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("/v1/contacts/{ID}")))
        .and(body_json(json!({
            "id": ID, "version": 7, "roles": {"customer": {"number": 10308}}, "person": {"lastName": "Musterfrau"}, "note": "VIP"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": ID, "version": 8})))
        .expect(1)
        .mount(&server)
        .await;
    let env = Env::new(&server);
    let out = env
        .run(&["contacts", "update", ID, "--merge", "--set", "note=VIP"])
        .await;
    assert_eq!(out.code(), 0, "{}", out.stderr());
    assert_eq!(out.json()["version"], 8);

    // Without --merge a PUT needs an explicit version.
    let out = env.run(&["contacts", "update", ID, "--set", "note=VIP"]).await;
    assert_eq!(out.code(), 2);
    assert!(out.err()["error"]["hint"].as_str().unwrap().contains("--merge"));
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_and_usage_errors_send_nothing() {
    let server = MockServer::start().await;
    let env = Env::new(&server);

    let out = env
        .run(&["invoices", "create", "--finalize", "--dry-run", "--body", r#"{"a":1}"#])
        .await;
    assert_eq!(out.code(), 0, "{}", out.stderr());
    let v = out.json();
    assert_eq!(v["method"], "POST");
    assert_eq!(v["url"], format!("{}/v1/invoices?finalize=true", server.uri()));
    assert_eq!(v["headers"]["Authorization"], "Bearer <redacted>");
    assert_eq!(v["body"], json!({"a": 1}));
    assert!(v["needsConfirmation"].is_array());

    let out = env.run(&["invoices", "get", "RE-1001"]).await;
    assert_eq!(out.code(), 2);
    assert!(out.err()["error"]["hint"].as_str().unwrap().contains("voucherlist"));

    let out = env.run(&["invoices", "create", "--body", "{not json"]).await;
    assert_eq!(out.code(), 2);

    let out = env.run(&["invoices", "frobnicate"]).await;
    assert_eq!(out.code(), 2);
    assert_eq!(out.err()["error"]["type"], "usage");

    assert!(requests(&server).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_credentials_exit_3() {
    let server = MockServer::start().await;
    let env = Env::new(&server);
    let mut c = env.cmd(&["profile", "get"]);
    c.env_remove("LXW_API_KEY");
    let out = run_cmd(c).await;
    assert_eq!(out.code(), 3);
    assert_eq!(out.err()["error"]["type"], "auth_missing");
}

#[tokio::test(flavor = "multi_thread")]
async fn api_key_login_stores_and_uses_profile() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/profile"))
        .and(header("authorization", "Bearer stored-key-abcdef"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"organizationId": ID, "companyName": "Testfirma GmbH", "created": {"userEmail": "a@b.de"}}),
        ))
        .mount(&server)
        .await;
    let env = Env::new(&server);
    let mut c = env.cmd(&["auth", "login", "--with-token"]);
    c.env_remove("LXW_API_KEY")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let out = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut child = c.spawn().unwrap();
        child.stdin.take().unwrap().write_all(b"stored-key-abcdef\n").unwrap();
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap();
    let out = Out(out);
    assert_eq!(out.code(), 0, "{}", out.stderr());
    assert_eq!(out.json()["account"]["companyName"], "Testfirma GmbH");

    let config = std::fs::read_to_string(env.dir.path().join("config/config.json")).unwrap();
    assert!(config.contains("stored-key-abcdef"));
    assert!(config.contains("\"secret_store\": \"file\""), "{config}");
    assert!(
        config.contains(&format!("\"base_url\": \"{}\"", server.uri())),
        "bound to the verified host: {config}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(env.dir.path().join("config/config.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let mut c = env.cmd(&["auth", "status"]);
    c.env_remove("LXW_API_KEY");
    let out = run_cmd(c).await;
    assert_eq!(out.code(), 0, "{}", out.stderr());
    let v = out.json();
    assert_eq!(v["source"], "profile:default");
    assert_eq!(v["key"], "stor...cdef");
    assert_eq!(v["verified"], true);
    assert!(v["credentialStore"].as_str().unwrap().starts_with("config file"));

    // Stored credentials are never sent to another host...
    let mut c = env.cmd(&["profile", "get", "--base-url", "https://evil.example"]);
    c.env_remove("LXW_API_KEY");
    let out = run_cmd(c).await;
    assert_eq!(out.code(), 2);
    assert!(out.err()["error"]["message"].as_str().unwrap().contains("bound to"));
    // ...while an explicit env key may go anywhere (e.g. this mock server).
    let out = env.run(&["profile", "get", "--dry-run"]).await;
    assert_eq!(out.code(), 0, "{}", out.stderr());
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_refreshes_on_401_and_stores_rotated_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/profile"))
        .and(header("authorization", "Bearer old-access"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"message": "Unauthorized"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth2/token"))
        .and(header("authorization", "Basic Y2xpZW50OnNlY3JldA==")) // client:secret
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=old-refresh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "new-access", "refresh_token": "new-refresh", "expires_in": 14399, "token_type": "Bearer"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/profile"))
        .and(header("authorization", "Bearer new-access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"companyName": "Partner GmbH"})))
        .mount(&server)
        .await;
    let env = Env::new(&server);
    let config_dir = env.dir.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.json"),
        serde_json::to_string(&json!({"profiles": {"default": {
            "auth": "oauth", "auth_url": server.uri(), "base_url": server.uri(), "client_id": "client",
            "client_secret": "secret", "access_token": "old-access", "refresh_token": "old-refresh"
        }}}))
        .unwrap(),
    )
    .unwrap();
    let mut c = env.cmd(&["profile", "get"]);
    c.env_remove("LXW_API_KEY");
    let out = run_cmd(c).await;
    assert_eq!(out.code(), 0, "{}", out.stderr());
    assert_eq!(out.json()["companyName"], "Partner GmbH");
    let config = std::fs::read_to_string(config_dir.join("config.json")).unwrap();
    assert!(
        config.contains("new-refresh") && config.contains("new-access"),
        "{config}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rate_limit_is_shared_across_processes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/countries"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    let env = Env::new(&server);
    let started = Instant::now();
    let children: Vec<_> = (0..5)
        .map(|_| {
            let mut c = env.cmd(&["countries", "list"]);
            c.env("LXW_RATE_LIMIT", "5") // one request every 200 ms, across processes
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            c.spawn().unwrap()
        })
        .collect();
    let outputs = tokio::task::spawn_blocking(move || {
        children
            .into_iter()
            .map(|c| c.wait_with_output().unwrap())
            .collect::<Vec<_>>()
    })
    .await
    .unwrap();
    assert!(outputs.iter().all(|o| o.status.success()));
    // 5 requests need at least 4 intervals of 200 ms.
    assert!(
        started.elapsed() >= Duration::from_millis(780),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(requests(&server).await.len(), 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn lexware_terms_are_enforced() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/invoices"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": ID})))
        .mount(&server)
        .await;
    let env = Env::new(&server);

    // Undocumented interfaces are refused (AGB, SaaS section 2.2 d); nothing is sent.
    let out = env.run(&["request", "GET", "/v1/ping"]).await;
    assert_eq!(out.code(), 2);
    assert!(
        out.err()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not a documented")
    );
    let out = env.run(&["request", "GET", "/v1/countries", "--dry-run"]).await;
    assert_eq!(out.code(), 0, "{}", out.stderr());

    // Fair Usage Policy: at most 20 outgoing vouchers per minute -> one every 3 s.
    let started = Instant::now();
    for _ in 0..2 {
        let out = env.run(&["invoices", "create", "--body", "{}"]).await;
        assert_eq!(out.code(), 0, "{}", out.stderr());
    }
    assert!(
        started.elapsed() >= Duration::from_millis(2900),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(requests(&server).await.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_works_offline() {
    let server = MockServer::start().await;
    let env = Env::new(&server);
    let out = env.run(&["cli", "search", "rechnung als pdf"]).await;
    assert_eq!(out.code(), 0);
    assert_eq!(out.json()[0]["command"], "lxw invoices download");

    let out = env.run(&["schema", "invoices", "create"]).await;
    let v = out.json();
    assert_eq!(v["httpMethod"], "POST");
    if cfg!(lxw_docs) {
        // Lexware's field reference and examples are only in personal builds (see build.rs).
        assert!(v["requestBody"]["example"]["lineItems"].is_array());
        assert!(v["requestBody"]["requiredFields"]["root"].is_array());
    } else {
        assert!(v["requestBody"]["example"].is_null());
        assert!(
            v["requestBody"]["fieldReference"]
                .as_str()
                .unwrap()
                .contains("developers.lexware.io")
        );
    }

    let out = env.run(&["schema", "--list"]).await;
    assert!(out.json().as_array().unwrap().len() >= 90);

    let out = env.run(&["--help"]).await;
    assert!(out.stdout().contains("AGENT COMMAND DISCOVERY"));
    assert!(requests(&server).await.is_empty());
}
