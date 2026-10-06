//! Executes catalog operations and raw requests.

use crate::auth;
use crate::catalog::{BodyKind, Operation, ParamType, QueryParam, ResponseKind, catalog};
use crate::cli::Globals;
use crate::client::{Client, Method, Payload, Request, Response, encode_segment, read_body};
use crate::error::{CliError, Kind};
use crate::jsonx;
use crate::output;
use clap::ArgMatches;
use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub fn run_operation(op: &Operation, m: &ArgMatches, g: &Globals) -> Result<(), CliError> {
    let mut path = op.path.clone();
    let mut first_id = String::new();
    for name in op.path_params() {
        let value = m
            .get_one::<String>(&format!("path:{name}"))
            .expect("clap enforces path params");
        validate_uuid(name, value)?;
        if first_id.is_empty() {
            first_id = value.clone();
        }
        path = path.replace(&format!("{{{name}}}"), &encode_segment(value));
    }

    let mut query = Vec::new();
    for q in &op.query {
        let id = format!("query:{}", q.name);
        let value = if q.ty == ParamType::Flag {
            m.get_flag(&id).then(|| "true".to_string())
        } else {
            m.get_one::<String>(&id).cloned()
        };
        if let Some(value) = value {
            query.push((q.name.clone(), validate_param(q, &value)?));
        }
    }
    query.extend(extra_query(m)?);

    let payload = match op.body {
        BodyKind::None => Payload::None,
        BodyKind::Json | BodyKind::JsonArray => Payload::Json(build_body(m, Some(op))?),
        BodyKind::Multipart => build_multipart(op, m)?,
    };
    let accept = if op.response == ResponseKind::Binary {
        m.get_one::<String>("accept").cloned().unwrap_or_else(|| "*/*".into())
    } else {
        "application/json".into()
    };
    let mut req = Request {
        method: Method::from_bytes(op.method.as_bytes()).expect("catalog methods are valid"),
        path,
        query,
        payload,
        accept,
        operation: Some(op.id.clone()),
        partner_only: op.is_partner(),
    };
    let confirmations = confirmations(op, &req.query);

    let merge = op.merge_from.is_some() && m.get_flag("merge");
    if let Some(field) = op.lock_field()
        && !merge
        && let Payload::Json(body) = &req.payload
        && body.get(field).is_none()
    {
        let get = op.merge_from.as_deref().unwrap_or_default().replace('.', " ");
        return Err(CliError::usage(format!(
            "`{} {}` needs the resource's current `{field}` (optimistic locking)",
            op.resource(),
            op.action()
        ))
        .with_hint(format!(
            "Add --merge to fetch the current resource and apply your body as a patch, or include \"{field}\" from `lxw {get} ...`."
        )));
    }
    if !g.dry_run {
        require_confirmation(op, &confirmations, g)?;
    } else if !merge {
        return dry_run(&req, &auth::resolve(g, false)?.base_url, g, &confirmations, false);
    }

    let session = auth::resolve(g, true)?;
    if op.is_partner() && session.is_api_key() && !g.dry_run {
        output::note(format!(
            "`{}` is documented only for the Partner API (OAuth); with an API key Lexware will likely reject it.",
            op.id
        ));
    }
    let mut client = Client::new(session, g)?;
    if merge {
        let mut get = Request::get(req.path.clone(), op.merge_from.as_deref());
        get.partner_only = op.is_partner();
        let mut current = client.json(&get)?;
        if let Payload::Json(patch) = &req.payload {
            jsonx::merge_patch(&mut current, patch);
        }
        req.payload = Payload::Json(current);
        if g.dry_run {
            return dry_run(&req, &client.session.base_url, g, &confirmations, true);
        }
    }

    if op.quota.is_some() {
        client.acquire_outgoing_voucher_quota();
    }
    match op.response {
        ResponseKind::Binary => download(&mut client, &req, m, g, &format!("{}-{first_id}", op.resource())),
        _ if op.is_paged() => run_paged(&mut client, req, m, g, op.max_page_size, &op.sort),
        _ => {
            let v = client.json(&req)?;
            output::print_json(&jsonx::project(v, &fields(m)), g.pretty);
            Ok(())
        }
    }
}

/// `lxw request METHOD PATH ...`
pub fn run_raw(m: &ArgMatches, g: &Globals) -> Result<(), CliError> {
    let method = m.get_one::<String>("method").expect("required").to_ascii_uppercase();
    let raw_path = m.get_one::<String>("path").expect("required");
    let (path, mut query) = normalize_raw_path(raw_path)?;
    query.extend(extra_query(m)?);
    // Only documented endpoints: Lexware's terms (AGB, SaaS section 2.2 d) forbid
    // using undocumented interfaces. The matched operation also supplies
    // confirmation rules, access and the default Accept header.
    let op = catalog().match_request(&method, &path).ok_or_else(|| {
        CliError::usage(format!("{method} {path} is not a documented Lexware API endpoint")).with_hint(
            "Lexware's terms forbid using undocumented interfaces, so `lxw request` only accepts documented \
             endpoints. Find the right one with `lxw cli search \"<task>\"` or list them with `lxw schema --list`.",
        )
    })?;
    let has_body = m.get_one::<String>("body").is_some() || m.get_many::<String>("set").is_some();
    let accept = m
        .get_one::<String>("accept")
        .cloned()
        .or_else(|| (op.response == ResponseKind::Binary).then(|| op.accept[0].clone()));
    let req = Request {
        method: Method::from_bytes(method.as_bytes()).map_err(|_| CliError::usage("invalid method"))?,
        path,
        query,
        payload: if has_body {
            Payload::Json(build_body(m, None)?)
        } else {
            Payload::None
        },
        accept: accept.unwrap_or_else(|| "application/json".into()),
        operation: Some(op.id.clone()),
        partner_only: op.is_partner(),
    };
    let confirmations = confirmations(op, &req.query);
    if g.dry_run {
        return dry_run(&req, &auth::resolve(g, false)?.base_url, g, &confirmations, false);
    }
    if !confirmations.is_empty() && !g.yes {
        return Err(confirmation_error(
            &format!("request {method} {}", req.path),
            &confirmations,
        ));
    }

    let mut client = Client::new(auth::resolve(g, true)?, g)?;
    if op.quota.is_some() {
        client.acquire_outgoing_voucher_quota();
    }
    if m.get_one::<String>("out").is_some() {
        return download(&mut client, &req, m, g, "response");
    }
    if m.get_flag("all") || m.get_one::<u32>("page").is_some() || m.get_one::<u32>("size").is_some() {
        return run_paged(&mut client, req, m, g, None, &[]);
    }
    let v = client.json(&req)?;
    output::print_json(&jsonx::project(v, &fields(m)), g.pretty);
    Ok(())
}

/// Why sending `query` to `op` needs `--yes`: the operation's own reason plus those
/// of confirm-flagged parameters that are set (e.g. `finalize=true`).
fn confirmations(op: &Operation, query: &[(String, String)]) -> Vec<String> {
    let set_params = op
        .query
        .iter()
        .filter(|q| query.iter().any(|(k, v)| *k == q.name && v != "false"));
    op.confirm
        .iter()
        .chain(set_params.filter_map(|q| q.confirm.as_ref()))
        .cloned()
        .collect()
}

fn normalize_raw_path(raw: &str) -> Result<(String, Vec<(String, String)>), CliError> {
    if raw.contains("://") {
        return Err(CliError::usage("pass a path such as /v1/contacts, not a full URL")
            .with_hint("The API key is only ever sent to the configured base URL (--base-url / LXW_BASE_URL)."));
    }
    let (p, qs) = raw.split_once('?').unwrap_or((raw, ""));
    let mut path = if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/{p}")
    };
    if !path.starts_with("/v1/") && path != "/v1" {
        path = format!("/v1{path}");
    }
    let query = form_urlencoded::parse(qs.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    Ok((path, query))
}

fn fields(m: &ArgMatches) -> Vec<String> {
    m.get_many::<String>("fields")
        .map(|v| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

fn extra_query(m: &ArgMatches) -> Result<Vec<(String, String)>, CliError> {
    let Some(values) = m.get_many::<String>("query") else {
        return Ok(Vec::new());
    };
    values
        .map(|kv| {
            kv.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| CliError::usage(format!("--query expects KEY=VALUE, got {kv:?}")))
        })
        .collect()
}

fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(len, p)| p.len() == *len && p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn validate_uuid(name: &str, value: &str) -> Result<(), CliError> {
    if is_uuid(value) {
        return Ok(());
    }
    Err(CliError::usage(format!("{name} must be a UUID like 8f8664a1-fd86-11e1-a21f-0800200c9a66, got {value:?}")).with_hint(
        "Lexware ids are UUIDs, not voucher or customer numbers. Find ids with `lxw voucherlist list --voucher-number <NR>` or `lxw contacts list --name <NAME>`.",
    ))
}

fn validate_param(q: &QueryParam, value: &str) -> Result<String, CliError> {
    let flag = crate::catalog::kebab(&q.name);
    match q.ty {
        ParamType::Uuid => validate_uuid(&format!("--{flag}"), value).map(|_| value.to_string()),
        ParamType::Integer => value
            .parse::<i64>()
            .map(|n| n.to_string())
            .map_err(|_| CliError::usage(format!("--{flag} must be an integer, got {value:?}"))),
        ParamType::Date => {
            let ok = value.len() == 10
                && value
                    .char_indices()
                    .all(|(i, c)| if i == 4 || i == 7 { c == '-' } else { c.is_ascii_digit() });
            if ok {
                Ok(value.to_string())
            } else {
                Err(CliError::usage(format!(
                    "--{flag} must be a date like 2026-01-31, got {value:?}"
                )))
            }
        }
        ParamType::Csv => {
            let items: Vec<&str> = value.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
            if !q.open_enum
                && !q.values.is_empty()
                && let Some(bad) = items.iter().find(|i| !q.values.iter().any(|v| v == *i))
            {
                return Err(CliError::usage(format!(
                    "--{flag}: unknown value {bad:?}; allowed: {}",
                    q.values.join(", ")
                )));
            }
            Ok(items.join(","))
        }
        ParamType::String if q.html_encode => Ok(html_escape(value)),
        _ => Ok(value.to_string()),
    }
}

/// Lexware stores &, < and > HTML-escaped and expects search strings in that form.
pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn read_input(raw: &str) -> Result<String, CliError> {
    if raw == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        Ok(s)
    } else if let Some(path) = raw.strip_prefix('@') {
        fs::read_to_string(path).map_err(|e| CliError::usage(format!("cannot read body file {path}: {e}")))
    } else {
        Ok(raw.to_string())
    }
}

fn build_body(m: &ArgMatches, op: Option<&Operation>) -> Result<Value, CliError> {
    let mut body = match m.get_one::<String>("body") {
        Some(raw) => {
            let text = read_input(raw)?;
            serde_json::from_str(&text).map_err(|e| {
                CliError::usage(format!(
                    "--body is not valid JSON (line {}, column {}): {e}",
                    e.line(),
                    e.column()
                ))
            })?
        }
        None => Value::Null,
    };
    if let Some(sets) = m.get_many::<String>("set") {
        for s in sets {
            let (path, raw) = s
                .split_once('=')
                .ok_or_else(|| CliError::usage(format!("--set expects PATH=VALUE, got {s:?}")))?;
            jsonx::set_path(&mut body, path.trim(), jsonx::parse_value(raw))
                .map_err(|e| CliError::usage(format!("--set: {e}")))?;
        }
    }
    let Some(op) = op else { return Ok(body) };
    if body.is_null() {
        let merge = op.merge_from.is_some() && m.get_flag("merge");
        return Err(CliError::usage(format!("`{}` needs a JSON body", op.id)).with_hint(if merge {
            "With --merge, pass only the fields to change, e.g. --set name=New or --body '{\"note\":\"x\"}'.".to_string()
        } else {
            format!(
                "Pass --body '<json>' / --body @file.json / --body -, or build it with --set path=value. Required fields and an example body: `lxw schema {} {}`.",
                op.resource(), op.action()
            )
        }));
    }
    match op.body {
        BodyKind::JsonArray if !body.is_array() => {
            Err(CliError::usage(format!("`{}` expects a JSON array body", op.id)))
        }
        BodyKind::Json if !body.is_object() => Err(CliError::usage(format!("`{}` expects a JSON object body", op.id))),
        _ => Ok(body),
    }
}

fn build_multipart(op: &Operation, m: &ArgMatches) -> Result<Payload, CliError> {
    let file = PathBuf::from(m.get_one::<String>("file").expect("clap enforces --file"));
    let meta = fs::metadata(&file).map_err(|e| CliError::usage(format!("cannot read {}: {e}", file.display())))?;
    if !meta.is_file() {
        return Err(CliError::usage(format!("{} is not a file", file.display())));
    }
    let fields: Vec<(String, String)> = op
        .form
        .iter()
        .filter_map(|f| {
            let v = m.get_one::<String>(&format!("form:{}", f.name))?;
            Some((f.name.clone(), v.clone()))
        })
        .collect();
    // The limits for the selected `type` form value, or the operation-wide ones.
    let upload_type = fields.iter().find(|(k, _)| k == "type").map(|(_, v)| v.as_str());
    let limit = op
        .upload
        .iter()
        .find(|u| u.form_type.as_deref() == upload_type)
        .or_else(|| op.upload.iter().find(|u| u.form_type.is_none()));
    if let Some(limit) = limit {
        let ext = file
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        if !limit.extensions.contains(&ext) {
            return Err(CliError::usage(format!(
                "{}: unsupported file type {ext:?}; allowed: {}",
                file.display(),
                limit.extensions.join(", ")
            )));
        }
        if meta.len() > limit.max_mb << 20 {
            return Err(CliError::usage(format!(
                "{} is {} bytes; Lexware accepts at most {} MB for this upload type",
                file.display(),
                meta.len(),
                limit.max_mb
            )));
        }
    }
    Ok(Payload::Multipart { file, fields })
}

fn confirmation_error(what: &str, reasons: &[String]) -> CliError {
    CliError::new(Kind::ConfirmationRequired, format!("`{what}` needs confirmation: {}", reasons.join(" "))).with_hint(
        "Nothing was sent. Re-run with --yes once this action is intended (ask the user first if unsure); use --dry-run to preview the request.",
    )
}

fn require_confirmation(op: &Operation, reasons: &[String], g: &Globals) -> Result<(), CliError> {
    if reasons.is_empty() || g.yes {
        return Ok(());
    }
    Err(confirmation_error(
        &format!("{} {}", op.resource(), op.action()),
        reasons,
    ))
}

fn dry_run(req: &Request, base_url: &str, g: &Globals, confirmations: &[String], merged: bool) -> Result<(), CliError> {
    let mut headers = json!({ "Accept": req.accept, "Authorization": "Bearer <redacted>" });
    let body = match &req.payload {
        Payload::None => Value::Null,
        Payload::Json(v) => {
            headers["Content-Type"] = json!("application/json");
            v.clone()
        }
        Payload::Multipart { file, fields } => {
            headers["Content-Type"] = json!("multipart/form-data");
            let mut parts = serde_json::Map::new();
            parts.insert("file".into(), json!(file));
            for (k, v) in fields {
                parts.insert(k.clone(), json!(v));
            }
            Value::Object(parts)
        }
    };
    let mut out = json!({
        "dryRun": true,
        "operation": req.operation,
        "method": req.method.as_str(),
        "url": req.url(base_url),
        "headers": headers,
        "body": body,
    });
    if merged {
        out["note"] = json!("body = current resource (fetched with GET) merged with your patch");
    }
    if !confirmations.is_empty() {
        out["needsConfirmation"] = json!(confirmations);
    }
    output::print_json(&out, g.pretty);
    Ok(())
}

fn run_paged(
    client: &mut Client,
    mut req: Request,
    m: &ArgMatches,
    g: &Globals,
    max_size: Option<u32>,
    sort_fields: &[String],
) -> Result<(), CliError> {
    let page = m.get_one::<u32>("page").copied();
    let size = m.get_one::<u32>("size").copied();
    if let (Some(s), Some(max)) = (size, max_size)
        && (s == 0 || s > max)
    {
        return Err(CliError::usage(format!(
            "--size must be between 1 and {max} for this endpoint"
        )));
    }
    if let Some(sort) = m.try_get_one::<String>("sort").ok().flatten() {
        validate_sort(sort, sort_fields)?;
        req.set_query("sort", sort.clone());
    }
    let fields = fields(m);
    if !m.get_flag("all") {
        if let Some(p) = page {
            req.set_query("page", p.to_string());
        }
        if let Some(s) = size {
            req.set_query("size", s.to_string());
        }
        let v = client.json(&req)?;
        output::print_json(&jsonx::project(v, &fields), g.pretty);
        return Ok(());
    }

    let max_items = m.get_one::<usize>("max-items").copied();
    let ndjson = m.get_flag("ndjson");
    let size = size.or(max_size);
    let mut page = page.unwrap_or(0);
    let mut items = Vec::new();
    let mut count = 0usize;
    'pages: loop {
        req.set_query("page", page.to_string());
        if let Some(s) = size {
            req.set_query("size", s.to_string());
        }
        let mut v = client.json(&req)?;
        if !v.get("content").is_some_and(Value::is_array) {
            // Not a paged response: print it unchanged.
            output::print_json(&jsonx::project(v, &fields), g.pretty);
            return Ok(());
        }
        let Value::Array(content) = v["content"].take() else {
            unreachable!("checked above")
        };
        if count == 0
            && v.get("totalElements")
                .and_then(Value::as_u64)
                .is_some_and(|t| t >= 10_000)
        {
            output::note(
                "10,000+ matches: Lexware stops paging at 10,000 entries; narrow the filters to get everything.",
            );
        }
        if g.verbose {
            output::note(format!(
                "page {page}: {} items (total {})",
                content.len(),
                v.get("totalElements").map_or("?".to_string(), Value::to_string)
            ));
        }
        let last = v.get("last").and_then(Value::as_bool).unwrap_or(true) || content.is_empty();
        for item in content {
            if max_items.is_some_and(|max| count >= max) {
                break 'pages;
            }
            let item = jsonx::project(item, &fields);
            if ndjson {
                output::print_json(&item, false);
            } else {
                items.push(item);
            }
            count += 1;
        }
        if last || max_items.is_some_and(|max| count >= max) {
            break;
        }
        page += 1;
    }
    if !ndjson {
        output::print_json(&Value::Array(items), g.pretty);
    }
    Ok(())
}

fn validate_sort(sort: &str, allowed: &[String]) -> Result<(), CliError> {
    let (field, dir) = sort.split_once(',').unwrap_or((sort, "ASC"));
    if !allowed.is_empty() && !allowed.iter().any(|a| a == field) {
        return Err(CliError::usage(format!(
            "--sort: unknown field {field:?}; allowed: {}",
            allowed.join(", ")
        )));
    }
    if !dir.eq_ignore_ascii_case("asc") && !dir.eq_ignore_ascii_case("desc") {
        return Err(CliError::usage(format!(
            "--sort: direction must be ASC or DESC, got {dir:?}"
        )));
    }
    Ok(())
}

fn download(
    client: &mut Client,
    req: &Request,
    m: &ArgMatches,
    g: &Globals,
    fallback_stem: &str,
) -> Result<(), CliError> {
    let mut resp = client.execute(req)?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        let bytes = read_body(&mut resp, req).unwrap_or_default();
        let mut err = client.api_error(status, &bytes, req);
        if status == 404 && req.accept != "*/*" {
            // Lexware answers 404 when the document exists but not in the requested format.
            err.hint = Some(format!(
                "No document as {} (or the id does not exist). Only XRechnung e-invoices have an XML \
                 version; the default --accept */* returns the format that exists.",
                req.accept
            ));
        }
        return Err(err);
    }
    let content_type = header(&resp, "content-type");
    let suggested = header(&resp, "content-disposition")
        .as_deref()
        .and_then(disposition_filename);
    // Streamed, so documents of any size never sit in memory.
    let mut body = resp.into_body().into_with_config().limit(u64::MAX).reader();
    let out = m.get_one::<String>("out").map(String::as_str);
    if out == Some("-") {
        let mut stdout = std::io::stdout().lock();
        std::io::copy(&mut body, &mut stdout).map_err(|e| CliError::new(Kind::Network, e.to_string()))?;
        stdout.flush()?;
        return Ok(());
    }
    let file_name = suggested
        .clone()
        .unwrap_or_else(|| format!("{fallback_stem}.{}", extension_for(content_type.as_deref())));
    let target = match out {
        Some(p) if Path::new(p).is_dir() || p.ends_with('/') => PathBuf::from(p).join(&file_name),
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(&file_name),
    };
    if let Some(dir) = target.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    let tmp = target.with_file_name(format!(
        ".{}.part",
        target.file_name().and_then(|n| n.to_str()).unwrap_or("download")
    ));
    let written = {
        let mut f = fs::File::create(&tmp)?;
        let n = std::io::copy(&mut body, &mut f).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            CliError::new(Kind::Network, format!("download interrupted: {e}"))
        })?;
        f.sync_all()?;
        n
    };
    fs::rename(&tmp, &target)?;
    let shown = fs::canonicalize(&target).unwrap_or(target);
    output::print_json(
        &json!({ "file": shown, "bytes": written, "contentType": content_type, "suggestedFileName": suggested }),
        g.pretty,
    );
    Ok(())
}

fn header(resp: &Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Extracts a safe file name from a Content-Disposition header.
pub fn disposition_filename(h: &str) -> Option<String> {
    let parts: Vec<&str> = h.split(';').map(str::trim).collect();
    let extended = parts.iter().find_map(|p| p.strip_prefix("filename*=")).and_then(|v| {
        let encoded = v.trim_matches('"').splitn(3, '\'').nth(2)?;
        percent_encoding::percent_decode_str(encoded)
            .decode_utf8()
            .ok()
            .map(|s| s.into_owned())
    });
    let plain = || {
        parts
            .iter()
            .find_map(|p| p.strip_prefix("filename="))
            .map(|v| v.trim_matches('"').to_string())
    };
    let name = extended.or_else(plain)?;
    let name = name
        .rsplit(['/', '\\'])
        .next()?
        .trim()
        .replace(|c: char| c.is_control(), "");
    (!name.is_empty() && name != "." && name != "..").then_some(name)
}

fn extension_for(content_type: Option<&str>) -> &'static str {
    match content_type.unwrap_or("").split(';').next().unwrap_or("").trim() {
        "application/pdf" => "pdf",
        "application/xml" | "text/xml" => "xml",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "application/json" => "json",
        _ => "bin",
    }
}

/// Resolves `lxw <resource> <action>` matches to a catalog operation.
pub fn dispatch(resource: &str, m: &ArgMatches, g: &Globals) -> Result<(), CliError> {
    let (action, am) = m
        .subcommand()
        .ok_or_else(|| CliError::usage(format!("missing action for {resource}")))?;
    let op = catalog()
        .operation(&format!("{resource}.{action}"))
        .ok_or_else(|| CliError::usage(format!("unknown command {resource} {action}")))?;
    run_operation(op, am, g)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_parsing() {
        assert_eq!(
            disposition_filename("attachment; filename=\"RE1001.pdf\"").as_deref(),
            Some("RE1001.pdf")
        );
        assert_eq!(
            disposition_filename("attachment; filename=\"x.pdf\"; filename*=UTF-8''Rechnung%20%C3%BC.pdf").as_deref(),
            Some("Rechnung ü.pdf")
        );
        assert_eq!(
            disposition_filename("attachment; filename=\"../../etc/passwd\"").as_deref(),
            Some("passwd")
        );
        assert_eq!(disposition_filename("inline"), None);
    }

    #[test]
    fn uuid_and_param_validation() {
        assert!(is_uuid("8f8664a1-fd86-11e1-a21f-0800200c9a66"));
        assert!(!is_uuid("RE-1001"));
        let q = QueryParam {
            name: "voucherDateFrom".into(),
            ty: ParamType::Date,
            description: String::new(),
            required: false,
            default: None,
            values: vec![],
            open_enum: false,
            html_encode: false,
            confirm: None,
        };
        assert!(validate_param(&q, "2026-01-31").is_ok());
        assert!(validate_param(&q, "31.01.2026").is_err());
        assert_eq!(html_escape("Johnson & <Partner>"), "Johnson &amp; &lt;Partner&gt;");
    }

    #[test]
    fn raw_paths_are_normalized() {
        assert_eq!(
            normalize_raw_path("contacts?page=1").unwrap(),
            ("/v1/contacts".into(), vec![("page".into(), "1".into())])
        );
        assert_eq!(normalize_raw_path("/v1/profile").unwrap().0, "/v1/profile");
        assert!(normalize_raw_path("https://evil.example/v1/x").is_err());
    }

    #[test]
    fn sort_validation() {
        let allowed = vec!["voucherDate".to_string()];
        assert!(validate_sort("voucherDate,DESC", &allowed).is_ok());
        assert!(validate_sort("voucherDate", &allowed).is_ok());
        assert!(validate_sort("name,ASC", &allowed).is_err());
        assert!(validate_sort("voucherDate,UP", &allowed).is_err());
    }
}
