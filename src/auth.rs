//! Credentials.
//!
//! - Public API (any Lexware Office XL user): a personal API key created at
//!   https://app.lexware.de/addons/public-api, sent as `Authorization: Bearer`.
//!   Source order: `LXW_API_KEY`, `LXW_API_KEY_FILE`, config profile.
//! - Partner API: OAuth2 authorization code + PKCE with client credentials
//!   issued by Lexware to partners. Refresh tokens rotate, so refreshes run
//!   under the config lock and re-read the profile first.
//!
//! Profile secrets live in the OS credential store (`secrets`) and are bound
//! to the API host they were verified against.

use crate::cli::Globals;
use crate::client::{Client, Request, Response, http_agent};
use crate::config::{self, AuthKind, Config, Profile, SecretStore};
use crate::error::{CliError, Kind, RequestContext};
use crate::secrets::{self, Secrets};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use clap::ArgMatches;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Slightly below Lexware's 2 req/s to absorb network jitter (as the docs advise).
pub const DEFAULT_RPS: f64 = 1.8;

pub enum Credential {
    None,
    ApiKey(String),
    OAuth(OAuthState),
}

pub struct OAuthState {
    pub auth_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_at: Option<u64>,
}

pub struct Session {
    pub base_url: String,
    pub credential: Credential,
    /// e.g. `env:LXW_API_KEY` or `profile:default`.
    pub source: String,
    pub profile: String,
    pub requests_per_second: f64,
    pub burst: u32,
}

impl Session {
    pub fn is_api_key(&self) -> bool {
        matches!(self.credential, Credential::ApiKey(_))
    }

    pub fn can_refresh(&self) -> bool {
        matches!(&self.credential, Credential::OAuth(o) if o.refresh_token.is_some())
    }

    /// Rate limits are per API client, so the shared limiter state is keyed by credential.
    pub fn limiter_key(&self) -> String {
        let id = match &self.credential {
            Credential::ApiKey(k) => format!("key:{k}"),
            Credential::OAuth(o) => format!("oauth:{}:{}", o.client_id, self.profile),
            Credential::None => "anonymous".to_string(),
        };
        hex_prefix(&Sha256::digest(format!("{}|{id}", self.base_url).as_bytes()), 16)
    }

    pub fn bearer(&mut self, http: &ureq::Agent) -> Result<String, CliError> {
        let refresh = match &self.credential {
            Credential::ApiKey(k) => return Ok(k.clone()),
            Credential::None => return Err(missing_credentials()),
            Credential::OAuth(o) => match (&o.access_token, o.expires_at) {
                (Some(_), Some(exp)) => exp <= now_secs() + 60,
                (Some(_), None) => false,
                (None, _) => true,
            },
        };
        if refresh {
            self.refresh(http)?;
        }
        match &self.credential {
            Credential::OAuth(OAuthState {
                access_token: Some(t), ..
            }) => Ok(t.clone()),
            _ => Err(missing_credentials()),
        }
    }

    /// Refreshes the OAuth access token (no-op for API keys).
    pub fn refresh(&mut self, http: &ureq::Agent) -> Result<(), CliError> {
        let profile_name = self.profile.clone();
        let Credential::OAuth(state) = &mut self.credential else {
            return Ok(());
        };
        let stale_token = state.access_token.clone();
        config::update(|cfg| {
            let profile = cfg.profiles.get_mut(&profile_name).ok_or_else(missing_credentials)?;
            let mut current = profile.load_secrets(&profile_name)?;
            // Another process may already have rotated the tokens.
            let fresh = current.access_token.is_some()
                && current.access_token != stale_token
                && profile.expires_at.is_none_or(|e| e > now_secs() + 60);
            if !fresh {
                let refresh_token = current.refresh_token.clone().ok_or_else(|| {
                    CliError::new(Kind::AuthMissing, "OAuth profile has no refresh token")
                        .with_hint("Run `lxw auth login --oauth ...` again.")
                })?;
                let tokens = token_request(
                    http,
                    &state.auth_url,
                    &state.client_id,
                    &state.client_secret,
                    &[("grant_type", "refresh_token"), ("refresh_token", &refresh_token)],
                )?;
                tokens.apply(profile, &mut current);
                profile.store_secrets(&profile_name, current.clone())?;
            }
            state.access_token = current.access_token;
            state.refresh_token = current.refresh_token;
            state.expires_at = profile.expires_at;
            Ok(())
        })
    }
}

fn missing_credentials() -> CliError {
    CliError::new(Kind::AuthMissing, "no Lexware credentials configured").with_hint(
        "Set LXW_API_KEY (or LXW_API_KEY_FILE), or run `lxw auth login`. Create an API \
         key at https://app.lexware.de/addons/public-api (requires Lexware Office XL).",
    )
}

impl Session {
    /// A session for `credential` with the settings that apply to `profile`:
    /// flags > `LXW_*` env vars > profile > defaults. Every caller (normal
    /// commands and both login flows) goes through here.
    fn new(g: &Globals, profile_name: &str, profile: &Profile, credential: Credential, source: String) -> Session {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        Session {
            base_url: g
                .base_url
                .clone()
                .or_else(|| env("LXW_BASE_URL"))
                .or_else(|| profile.base_url.clone())
                .unwrap_or_else(|| config::DEFAULT_BASE_URL.to_string()),
            requests_per_second: g
                .rate_limit
                .or_else(|| env("LXW_RATE_LIMIT").and_then(|v| v.parse().ok()))
                .or(profile.requests_per_second)
                .unwrap_or(DEFAULT_RPS),
            burst: profile.burst.unwrap_or(1),
            credential,
            source,
            profile: profile_name.to_string(),
        }
    }
}

fn oauth_state(profile: &Profile, secrets: Secrets) -> OAuthState {
    OAuthState {
        auth_url: profile
            .auth_url
            .clone()
            .unwrap_or_else(|| config::DEFAULT_AUTH_URL.to_string()),
        client_id: profile.client_id.clone().unwrap_or_default(),
        client_secret: secrets.client_secret.unwrap_or_default(),
        access_token: secrets.access_token,
        refresh_token: secrets.refresh_token,
        expires_at: profile.expires_at,
    }
}

/// Stored credentials only ever go to the host they were verified against;
/// otherwise `--base-url` / `LXW_BASE_URL` could send them anywhere.
fn check_host_binding(session: &Session, profile: &Profile) -> Result<(), CliError> {
    let bound = profile.bound_base_url().trim_end_matches('/');
    if session.base_url.trim_end_matches('/') == bound {
        return Ok(());
    }
    Err(CliError::usage(format!(
        "the stored credentials of profile `{}` are bound to {bound}; refusing to send them to {}",
        session.profile, session.base_url
    ))
    .with_hint("Use LXW_API_KEY for other hosts, or log in for that host with `lxw auth login --base-url <url>`."))
}

/// Resolves the credential (`LXW_API_KEY`, `LXW_API_KEY_FILE`, then the
/// profile) and settings. With `require`, a missing credential is an error;
/// otherwise `Credential::None` is returned (used by `--dry-run`).
pub fn resolve(g: &Globals, require: bool) -> Result<Session, CliError> {
    let cfg = Config::load()?;
    let profile_name = cfg.profile_name(g.profile.as_deref());
    let profile = cfg.profiles.get(&profile_name).cloned().unwrap_or_default();
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let mut from_profile = false;

    let (credential, source) = if let Some(key) = env("LXW_API_KEY") {
        (
            Credential::ApiKey(key.trim().to_string()),
            "env:LXW_API_KEY".to_string(),
        )
    } else if let Some(path) = env("LXW_API_KEY_FILE") {
        let key = std::fs::read_to_string(&path)
            .map_err(|e| CliError::new(Kind::AuthMissing, format!("cannot read LXW_API_KEY_FILE {path}: {e}")))?;
        (
            Credential::ApiKey(key.trim().to_string()),
            format!("env:LXW_API_KEY_FILE={path}"),
        )
    } else if cfg.profiles.contains_key(&profile_name) {
        from_profile = true;
        let secrets = profile.load_secrets(&profile_name)?;
        let source = format!("profile:{profile_name}");
        match (profile.auth, secrets.api_key.clone()) {
            (AuthKind::ApiKey, Some(k)) => (Credential::ApiKey(k), source),
            (AuthKind::ApiKey, None) => (Credential::None, "none".to_string()),
            (AuthKind::Oauth, _) => (Credential::OAuth(oauth_state(&profile, secrets)), source),
        }
    } else {
        (Credential::None, "none".to_string())
    };
    if require && matches!(credential, Credential::None) {
        return Err(missing_credentials());
    }
    let session = Session::new(g, &profile_name, &profile, credential, source);
    if from_profile && !matches!(session.credential, Credential::None) {
        check_host_binding(&session, &profile)?;
    }
    Ok(session)
}

/// `--store os|file` (or `LXW_CREDENTIAL_STORE=file`), checking that the OS store works.
fn chosen_store(m: &ArgMatches) -> Result<SecretStore, CliError> {
    let file = m.get_one::<String>("store").is_some_and(|s| s == "file") || secrets::disabled_by_env();
    if file {
        return Ok(SecretStore::File);
    }
    secrets::check_available()?;
    Ok(SecretStore::Os)
}

fn store_label(profile: &Profile) -> String {
    match profile.secret_store {
        SecretStore::Os => secrets::store_name().to_string(),
        SecretStore::File => format!("config file {} (plaintext)", config::config_path().display()),
    }
}

// ---------------------------------------------------------------------------
// `lxw auth ...`
// ---------------------------------------------------------------------------

pub fn run(m: &ArgMatches, g: &Globals) -> Result<Value, CliError> {
    match m.subcommand() {
        Some(("login", sub)) if sub.get_flag("oauth") => oauth_login(sub, g),
        Some(("login", sub)) => api_key_login(sub, g),
        Some(("status", sub)) => status(sub, g),
        Some(("list", _)) => list(g),
        Some(("logout", _)) => logout(g),
        _ => Err(CliError::usage("unknown auth command")),
    }
}

fn api_key_login(m: &ArgMatches, g: &Globals) -> Result<Value, CliError> {
    let stdin = std::io::stdin();
    let key = if m.get_flag("with-token") || !stdin.is_terminal() {
        let mut s = String::new();
        stdin.lock().read_to_string(&mut s)?;
        s
    } else {
        eprint!(
            "Paste your Lexware API key (create one at https://app.lexware.de/addons/public-api).\n\
             Input is visible; pipe it instead to avoid that: `... | lxw auth login --with-token`\n> "
        );
        std::io::stderr().flush()?;
        let mut s = String::new();
        stdin.lock().read_line(&mut s)?;
        s
    };
    let key = key.trim().to_string();
    if key.is_empty() || key.contains(char::is_whitespace) {
        return Err(CliError::usage("expected a single API key on stdin"));
    }

    let store = chosen_store(m)?;
    let cfg = Config::load()?;
    let profile_name = cfg.profile_name(g.profile.as_deref());
    let existing = cfg.profiles.get(&profile_name).cloned().unwrap_or_default();
    let session = Session::new(
        g,
        &profile_name,
        &existing,
        Credential::ApiKey(key.clone()),
        "login".into(),
    );
    // The key gets bound to the host it is verified against.
    let base_url = session.base_url.trim_end_matches('/').to_string();

    let account = if m.get_flag("no-verify") {
        Value::Null
    } else {
        account_summary(&Client::new(session, g)?.json(&Request::get("/v1/profile", Some("profile.get")))?)
    };

    let profile = config::update(|cfg| {
        // Keep per-profile settings, replace any previous credentials.
        let old = cfg.profiles.remove(&profile_name).unwrap_or_default();
        if old.uses_os_store() && store == SecretStore::File {
            secrets::delete(&profile_name)?;
        }
        let mut profile = Profile {
            auth: AuthKind::ApiKey,
            secret_store: store,
            base_url: (base_url != config::DEFAULT_BASE_URL).then(|| base_url.clone()),
            requests_per_second: old.requests_per_second,
            burst: old.burst,
            ..Default::default()
        };
        let new = Secrets {
            api_key: Some(key.clone()),
            ..Default::default()
        };
        profile.store_secrets(&profile_name, new)?;
        cfg.profiles.insert(profile_name.clone(), profile.clone());
        Ok(profile)
    })?;
    Ok(json!({
        "loggedIn": true,
        "profile": profile_name,
        "auth": "api_key",
        "credentialStore": store_label(&profile),
        "boundTo": profile.bound_base_url(),
        "account": account,
    }))
}

fn status(m: &ArgMatches, g: &Globals) -> Result<Value, CliError> {
    let session = resolve(g, true)?;
    let cfg = Config::load()?;
    let store = if session.source.starts_with("profile:") {
        cfg.profiles.get(&session.profile).map(store_label)
    } else {
        Some("environment".to_string())
    };
    let mut out = json!({
        "source": session.source,
        "credentialStore": store,
        "profile": session.profile,
        "baseUrl": session.base_url,
        "rateLimit": { "requestsPerSecond": session.requests_per_second, "burst": session.burst },
        "configFile": config::config_path(),
    });
    match &session.credential {
        Credential::ApiKey(k) => {
            out["auth"] = json!("api_key");
            out["key"] = json!(mask(k));
        }
        Credential::OAuth(o) => {
            out["auth"] = json!("oauth");
            out["clientId"] = json!(o.client_id);
            out["accessTokenExpiresAt"] = json!(o.expires_at);
        }
        Credential::None => {}
    }
    if m.get_flag("offline") {
        out["verified"] = json!(false);
        return Ok(out);
    }
    let mut client = Client::new(session, g)?;
    let profile = client.json(&Request::get("/v1/profile", Some("profile.get")))?;
    out["verified"] = json!(true);
    out["account"] = account_summary(&profile);
    Ok(out)
}

fn list(g: &Globals) -> Result<Value, CliError> {
    let cfg = Config::load()?;
    let active = cfg.profile_name(g.profile.as_deref());
    let profiles: Vec<Value> = cfg
        .profiles
        .iter()
        .map(|(name, p)| {
            json!({
                "name": name,
                "active": *name == active,
                "auth": p.auth,
                "credentialStore": store_label(p),
                "key": p.secrets.api_key.as_deref().map(mask),
                "clientId": p.client_id,
                "boundTo": p.bound_base_url(),
            })
        })
        .collect();
    let mut out = json!({ "active": active, "profiles": profiles, "configFile": config::config_path() });
    if std::env::var_os("LXW_API_KEY").is_some() {
        out["note"] = json!("LXW_API_KEY is set and takes precedence over stored profiles.");
    }
    Ok(out)
}

fn logout(g: &Globals) -> Result<Value, CliError> {
    let cfg = Config::load()?;
    let name = cfg.profile_name(g.profile.as_deref());
    let Some(profile) = cfg.profiles.get(&name).cloned() else {
        return Ok(json!({ "loggedOut": false, "profile": name, "message": "no stored credentials" }));
    };
    let stored = profile.load_secrets(&name)?;
    let mut revoked = Value::Null;
    if profile.auth == AuthKind::Oauth
        && let (Some(token), Some(id), Some(secret)) =
            (&stored.refresh_token, &profile.client_id, &stored.client_secret)
    {
        let auth_url = profile
            .auth_url
            .clone()
            .unwrap_or_else(|| config::DEFAULT_AUTH_URL.to_string());
        let http = http_agent(Duration::from_secs(g.timeout_secs))?;
        let url = format!("{}/oauth2/revoke", auth_url.trim_end_matches('/'));
        let res = post_form(&http, &url, id, secret, &[("token", token)]);
        revoked = json!(matches!(res, Ok(r) if r.status().is_success()));
    }
    if profile.uses_os_store() {
        secrets::delete(&name)?;
    }
    config::update(|cfg| {
        cfg.profiles.remove(&name);
        Ok(())
    })?;
    let mut out =
        json!({ "loggedOut": true, "profile": name, "oauthRevoked": revoked, "removedFrom": store_label(&profile) });
    if profile.auth == AuthKind::ApiKey {
        out["note"] =
            json!("The key was removed locally. Delete it at https://app.lexware.de/addons/public-api to revoke it.");
    }
    if std::env::var_os("LXW_API_KEY").is_some() {
        out["warning"] = json!("LXW_API_KEY is still set in the environment and will be used.");
    }
    Ok(out)
}

fn account_summary(profile: &Value) -> Value {
    let pick = |k: &str| profile.get(k).cloned().unwrap_or(Value::Null);
    let created = profile.get("created").cloned().unwrap_or(Value::Null);
    json!({
        "organizationId": pick("organizationId"),
        "companyName": pick("companyName"),
        "userName": created.get("userName").cloned().unwrap_or(Value::Null),
        "userEmail": created.get("userEmail").cloned().unwrap_or(Value::Null),
        "connectionId": pick("connectionId"),
        "businessFeatures": pick("businessFeatures"),
        "subscriptionStatus": pick("subscriptionStatus"),
        "taxType": pick("taxType"),
        "smallBusiness": pick("smallBusiness"),
    })
}

fn mask(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 10 {
        return "****".into();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}...{tail}")
}

// ---------------------------------------------------------------------------
// OAuth2 (Partner API)
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    scope: Option<String>,
}

impl Tokens {
    /// Records new tokens: secrets into `s`, expiry and scope into the profile.
    fn apply(&self, p: &mut Profile, s: &mut Secrets) {
        s.access_token = Some(self.access_token.clone());
        if self.refresh_token.is_some() {
            s.refresh_token = self.refresh_token.clone();
        }
        p.expires_at = self.expires_in.map(|secs| now_secs() + secs);
        if self.scope.is_some() {
            p.scope = self.scope.clone();
        }
    }
}

fn form_body(pairs: &[(&str, &str)]) -> String {
    form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

/// Query parameter `key` of a URL or request target such as `/callback?code=..`.
fn query_param(target: &str, key: &str) -> Option<String> {
    let query = target.split_once('?')?.1.split('#').next()?;
    form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// POSTs a form to the OAuth2 server, authenticated with the client credentials.
fn post_form(
    http: &ureq::Agent,
    url: &str,
    client_id: &str,
    client_secret: &str,
    form: &[(&str, &str)],
) -> Result<Response, ureq::Error> {
    let basic = STANDARD.encode(format!("{client_id}:{client_secret}"));
    http.post(url)
        .header("Authorization", format!("Basic {basic}"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .send(form_body(form))
}

fn token_request(
    http: &ureq::Agent,
    auth_url: &str,
    client_id: &str,
    client_secret: &str,
    form: &[(&str, &str)],
) -> Result<Tokens, CliError> {
    let path = "/oauth2/token";
    let ctx = RequestContext {
        operation: None,
        method: "POST".into(),
        path: path.into(),
        partner_only: true,
    };
    let url = format!("{}{path}", auth_url.trim_end_matches('/'));
    let mut resp = post_form(http, &url, client_id, client_secret, form)
        .map_err(|e| CliError::new(Kind::Network, format!("OAuth token request failed: {e}")))?;
    let status = resp.status().as_u16();
    let bytes = resp
        .body_mut()
        .read_to_vec()
        .map_err(|e| CliError::new(Kind::Network, e.to_string()))?;
    if !(200..300).contains(&status) {
        let mut err = CliError::from_api(status, &bytes, &ctx, 0);
        if status == 400 || status == 401 {
            err.kind = Kind::Unauthorized;
            err.hint = Some(
                "The OAuth grant is invalid, expired or revoked (refresh tokens rotate on every \
                 refresh). Run `lxw auth login --oauth ...` again."
                    .into(),
            );
        }
        return Err(err);
    }
    serde_json::from_slice(&bytes).map_err(|e| CliError::new(Kind::Server, format!("invalid token response: {e}")))
}

fn random_b64(len: usize) -> Result<String, CliError> {
    let mut buf = vec![0u8; len];
    getrandom::fill(&mut buf).map_err(|e| CliError::internal(format!("no randomness: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn oauth_login(m: &ArgMatches, g: &Globals) -> Result<Value, CliError> {
    let store = chosen_store(m)?;
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let client_id = m
        .get_one::<String>("client-id")
        .cloned()
        .or_else(|| env("LXW_CLIENT_ID"))
        .ok_or_else(|| CliError::usage("--client-id (or LXW_CLIENT_ID) is required for --oauth"))?;
    let redirect_uri = m
        .get_one::<String>("redirect-uri")
        .cloned()
        .ok_or_else(|| CliError::usage("--redirect-uri is required for --oauth (as registered with Lexware)"))?;
    let client_secret = match env("LXW_CLIENT_SECRET") {
        Some(s) => s,
        None => {
            eprint!("OAuth client secret: ");
            std::io::stderr().flush()?;
            let mut s = String::new();
            std::io::stdin().lock().read_line(&mut s)?;
            s.trim().to_string()
        }
    };
    let sandbox = m.get_flag("sandbox");
    let auth_url = if sandbox {
        config::SANDBOX_AUTH_URL
    } else {
        config::DEFAULT_AUTH_URL
    }
    .to_string();
    let base_url = g.base_url.clone().unwrap_or_else(|| {
        if sandbox {
            config::SANDBOX_BASE_URL
        } else {
            config::DEFAULT_BASE_URL
        }
        .to_string()
    });

    let verifier = random_b64(64)?;
    let state = random_b64(16)?;
    let mut params = vec![
        ("client_id", client_id.clone()),
        ("redirect_uri", redirect_uri.clone()),
        ("response_type", "code".to_string()),
        ("state", state.clone()),
        ("code_challenge_method", "S256".to_string()),
        ("code_challenge", pkce_challenge(&verifier)),
    ];
    if let Some(scope) = m.get_one::<String>("scope") {
        params.push(("scope", scope.clone()));
    }
    if let Some(name) = m.get_one::<String>("connection-name") {
        params.push(("connection_name", name.clone()));
    }
    let authorize = format!(
        "{auth_url}/oauth2/authorize?{}",
        form_urlencoded::Serializer::new(String::new())
            .extend_pairs(&params)
            .finish()
    );

    eprintln!("Open this URL, log in to Lexware and approve the connection:\n\n  {authorize}\n");
    if !m.get_flag("no-browser") {
        open_browser(&authorize);
    }
    let code = match loopback_address(&redirect_uri) {
        Some((addr, path)) => wait_for_callback(&addr, &path, &state)?,
        None => read_pasted_code(&state)?,
    };

    let http = http_agent(Duration::from_secs(g.timeout_secs))?;
    let tokens = token_request(
        &http,
        &auth_url,
        &client_id,
        &client_secret,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("code_verifier", &verifier),
        ],
    )?;

    let cfg = Config::load()?;
    let profile_name = cfg.profile_name(g.profile.as_deref());
    let mut profile = Profile {
        auth: AuthKind::Oauth,
        secret_store: store,
        base_url: Some(base_url),
        auth_url: Some(auth_url),
        client_id: Some(client_id),
        redirect_uri: Some(redirect_uri),
        ..Default::default()
    };
    let mut new = Secrets {
        client_secret: Some(client_secret),
        ..Default::default()
    };
    tokens.apply(&mut profile, &mut new);
    config::update(|cfg| {
        profile.store_secrets(&profile_name, new.clone())?;
        cfg.profiles.insert(profile_name.clone(), profile.clone());
        Ok(())
    })?;

    let account = if m.get_flag("no-verify") {
        Value::Null
    } else {
        // Verify the new tokens themselves, even if LXW_API_KEY is set.
        let credential = Credential::OAuth(oauth_state(&profile, new));
        let session = Session::new(g, &profile_name, &profile, credential, "login".into());
        account_summary(&Client::new(session, g)?.json(&Request::get("/v1/profile", Some("profile.get")))?)
    };
    Ok(json!({
        "loggedIn": true,
        "profile": profile_name,
        "auth": "oauth",
        "sandbox": sandbox,
        "scope": tokens.scope,
        "credentialStore": store_label(&profile),
        "account": account,
    }))
}

fn open_browser(url: &str) {
    let mut cmd = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        // Not `cmd /C start`: cmd would treat the `&`s in the URL as command separators.
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    let _ = cmd
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// `http://127.0.0.1:8765/callback` -> ("127.0.0.1:8765", "/callback")
fn loopback_address(redirect_uri: &str) -> Option<(String, String)> {
    let uri: ureq::http::Uri = redirect_uri.parse().ok()?;
    if uri.scheme_str() != Some("http") {
        return None;
    }
    let host = match uri.host()? {
        "127.0.0.1" | "localhost" => "127.0.0.1",
        "[::1]" | "::1" => "[::1]",
        _ => return None,
    };
    Some((
        format!("{host}:{}", uri.port_u16().unwrap_or(80)),
        uri.path().to_string(),
    ))
}

fn wait_for_callback(addr: &str, path: &str, state: &str) -> Result<String, CliError> {
    let listener = TcpListener::bind(addr)
        .map_err(|e| CliError::new(Kind::Io, format!("cannot listen on {addr} for the OAuth callback: {e}")))?;
    listener.set_nonblocking(true)?;
    eprintln!("Waiting for the redirect on http://{addr}{path} (5 minutes) ...");
    let deadline = Instant::now() + Duration::from_secs(300);
    while Instant::now() < deadline {
        let (mut stream, _) = match listener.accept() {
            Ok(conn) => conn,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line)?;
        let target = line.split_whitespace().nth(1).unwrap_or("/");
        let respond = |stream: &mut std::net::TcpStream, code: &str, body: &str| {
            let _ = write!(
                stream,
                "HTTP/1.1 {code}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        };
        if target.split('?').next() != Some(path) {
            respond(&mut stream, "404 Not Found", "not found");
            continue;
        }
        let param = |k: &str| query_param(target, k);
        if let Some(err) = param("error") {
            respond(
                &mut stream,
                "400 Bad Request",
                "Authorization failed. You can close this window.",
            );
            return Err(CliError::new(
                Kind::Unauthorized,
                format!("authorization denied: {err}"),
            ));
        }
        if param("state").as_deref() != Some(state) {
            respond(
                &mut stream,
                "400 Bad Request",
                "State mismatch. You can close this window.",
            );
            return Err(CliError::new(
                Kind::Unauthorized,
                "OAuth state mismatch (possible CSRF); aborting",
            ));
        }
        if let Some(code) = param("code") {
            respond(
                &mut stream,
                "200 OK",
                "Lexware connection approved. You can close this window.",
            );
            return Ok(code);
        }
        respond(&mut stream, "400 Bad Request", "Missing code.");
    }
    Err(CliError::new(
        Kind::Timeout,
        "no OAuth callback received within 5 minutes",
    ))
}

fn read_pasted_code(state: &str) -> Result<String, CliError> {
    eprint!("Paste the authorization code (or the full redirect URL): ");
    std::io::stderr().flush()?;
    let mut s = String::new();
    std::io::stdin().lock().read_line(&mut s)?;
    let s = s.trim();
    // A full redirect URL (contains a query) or just the code.
    if s.contains('?') {
        let param = |k: &str| query_param(s, k);
        if param("state").is_some_and(|st| st != state) {
            return Err(CliError::new(Kind::Unauthorized, "OAuth state mismatch; aborting"));
        }
        return param("code").ok_or_else(|| CliError::usage("redirect URL contains no code"));
    }
    if s.is_empty() {
        return Err(CliError::usage("no authorization code given"));
    }
    Ok(s.to_string())
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn hex_prefix(bytes: &[u8], chars: usize) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
        .chars()
        .take(chars)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_lexware_test_vector() {
        // Test vector from the Lexware Partner API docs (PKCE section).
        let verifier = "oJdaNUDOIK3xpqlkZ79Sa3dLWst2rF5F8EXQVrqWfzckNOPBC67SzNffeTTb54jSpfuSAnawxLgddxbIqXh4APOrKYqhI8W5fXtIytVSgjLlrnMn0AUB5h2O6VD1sTDJ";
        assert_eq!(pkce_challenge(verifier), "9s_XWOy5h9KO8CQCqzRKr3WEntoEJepUZS9d1IYh_uo");
    }

    #[test]
    fn loopback_detection() {
        assert_eq!(
            loopback_address("http://localhost:8765/callback"),
            Some(("127.0.0.1:8765".into(), "/callback".into()))
        );
        assert_eq!(loopback_address("https://example.com/cb"), None);
    }

    #[test]
    fn query_params_are_decoded() {
        assert_eq!(
            query_param("/cb?code=a%2Bb&state=x#frag", "code").as_deref(),
            Some("a+b")
        );
        assert_eq!(
            query_param("http://127.0.0.1:1/cb?state=s", "state").as_deref(),
            Some("s")
        );
        assert_eq!(query_param("/cb", "code"), None);
    }

    #[test]
    fn masks_keys() {
        assert_eq!(mask("abcdefghijklmnop"), "abcd...mnop");
        assert_eq!(mask("short"), "****");
    }
}
