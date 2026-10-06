//! Offline discovery, modeled on the Cloudflare `cf` CLI:
//! - `lxw cli search "<task>"`: up to five compact JSON matches.
//! - `lxw schema <command words>`: the API request behind a command
//!   (replace the leading `lxw` of any command with `lxw schema`).
//! - `lxw schema <resource>`: commands plus the field reference.
//! - `lxw cli guide`: conventions, auth, errors, rate limits.

use crate::catalog::{
    Access, Operation, ParamType, Resource, ResponseKind, catalog, doc_section, has_docs, kebab, positional_name,
};
use crate::cli::Globals;
use crate::error::{CliError, Kind};
use crate::output;
use clap::ArgMatches;
use serde_json::{Map, Value, json};
use std::sync::OnceLock;

pub const GUIDE: &str = include_str!("guide.md");

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

const STOPWORDS: &[&str] = &[
    "a", "an", "the", "to", "of", "for", "in", "on", "by", "with", "and", "or", "my", "me", "i", "how", "do", "is",
    "it", "from", "as", "at", "ein", "eine", "einen", "einer", "der", "die", "das", "den", "dem", "des", "und", "oder",
    "mit", "fur", "fuer", "von", "zu", "im", "ich", "wie", "lexware", "lxw", "api", "via", "please", "bitte",
];

/// Task words that point at an action.
const ACTION_SYNONYMS: &[(&str, &[&str])] = &[
    (
        "create",
        &[
            "create",
            "new",
            "add",
            "make",
            "write",
            "post",
            "anlegen",
            "erstellen",
            "erzeugen",
            "neu",
            "neue",
            "schreiben",
            "pursue",
            "convert",
            "subscribe",
            "register",
        ],
    ),
    (
        "list",
        &[
            "list",
            "all",
            "search",
            "find",
            "filter",
            "query",
            "browse",
            "overview",
            "liste",
            "suchen",
            "finden",
            "alle",
            "auflisten",
            "which",
        ],
    ),
    (
        "get",
        &[
            "get", "show", "read", "fetch", "retrieve", "detail", "details", "view", "lookup", "anzeigen", "lesen",
            "abrufen", "holen", "check",
        ],
    ),
    (
        "update",
        &[
            "update",
            "edit",
            "change",
            "modify",
            "rename",
            "put",
            "aendern",
            "bearbeiten",
            "aktualisieren",
            "set",
        ],
    ),
    (
        "delete",
        &["delete", "remove", "drop", "loeschen", "entfernen", "unsubscribe"],
    ),
    (
        "download",
        &[
            "download",
            "pdf",
            "file",
            "document",
            "export",
            "save",
            "herunterladen",
            "datei",
            "speichern",
            "xml",
            "dokument",
        ],
    ),
    (
        "send-email",
        &[
            "send",
            "email",
            "mail",
            "versenden",
            "senden",
            "verschicken",
            "zustellen",
        ],
    ),
    ("upload", &["upload", "hochladen", "scan", "import"]),
    ("upload-file", &["upload", "hochladen", "attach", "anhaengen", "scan"]),
    ("render-document", &["render", "document", "pdf"]),
];

/// Appends a lowercase char with German umlauts folded (ä -> ae, ß -> ss).
fn push_folded(out: &mut String, c: char) {
    match c {
        'ä' => out.push_str("ae"),
        'ö' => out.push_str("oe"),
        'ü' => out.push_str("ue"),
        'ß' => out.push_str("ss"),
        c => out.push(c),
    }
}

fn fold_umlauts(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        push_folded(&mut out, c);
    }
    out
}

fn stem(mut t: String) -> String {
    if t.ends_with("ies") && t.len() > 4 {
        t.truncate(t.len() - 3);
        t.push('y');
    } else if t.ends_with("en") && t.len() > 6 {
        t.truncate(t.len() - 2);
    } else if t.ends_with('s') && !t.ends_with("ss") && t.len() > 3 {
        t.pop();
    }
    if t.ends_with('e') && t.len() > 4 {
        t.pop();
    }
    t
}

/// Lowercase, umlaut-folded, camelCase/kebab split, stemmed tokens, in one pass.
pub fn tokens(s: &str) -> Vec<String> {
    fn flush(word: &mut String, out: &mut Vec<String>) {
        if word.len() >= 2 && !STOPWORDS.contains(&word.as_str()) {
            out.push(stem(std::mem::take(word)));
        } else {
            word.clear();
        }
    }
    let mut out = Vec::new();
    let mut word = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if !c.is_alphanumeric() {
            flush(&mut word, &mut out);
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower {
            flush(&mut word, &mut out);
        }
        prev_lower = c.is_lowercase();
        for l in c.to_lowercase() {
            push_folded(&mut word, l);
        }
    }
    flush(&mut word, &mut out);
    out
}

fn match_quality(q: &str, d: &str) -> f64 {
    if q == d {
        return 1.0;
    }
    let (short, long) = if q.len() <= d.len() { (q, d) } else { (d, q) };
    if short.len() >= 4 && long.starts_with(short) {
        return 0.75;
    }
    if q.len() >= 5 && d.contains(q) {
        return 0.55;
    }
    if q.len() >= 5 && d.len() >= 5 && strsim::jaro_winkler(q, d) >= 0.9 {
        return 0.5;
    }
    0.0
}

struct Field {
    tokens: Vec<String>,
    weight: f64,
}

fn field(text: &str, weight: f64) -> Field {
    Field {
        tokens: tokens(text),
        weight,
    }
}

/// Searchable fields shared by all operations of a resource (tokenized once per search).
fn resource_fields(res: &Resource) -> Vec<Field> {
    let mut fields = vec![field(&res.name, 3.0), field(&res.title, 2.0), field(&res.summary, 0.6)];
    fields.extend(res.keywords.iter().map(|k| field(k, 2.5)));
    fields
}

fn op_fields(op: &Operation) -> Vec<Field> {
    let f = field;
    let mut fields = vec![
        f(op.action(), 3.0),
        f(&op.summary, 1.5),
        f(&op.path.replace(['{', '}'], " "), 1.0),
    ];
    for k in &op.keywords {
        fields.push(f(k, 2.0));
    }
    for q in &op.query {
        fields.push(f(&q.name, 1.0));
    }
    for n in &op.notes {
        fields.push(f(n, 0.3));
    }
    fields
}

/// `ACTION_SYNONYMS` normalized like query tokens, computed once per process.
fn action_lexicon() -> &'static [(&'static str, Vec<String>)] {
    static LEXICON: OnceLock<Vec<(&'static str, Vec<String>)>> = OnceLock::new();
    LEXICON.get_or_init(|| {
        ACTION_SYNONYMS
            .iter()
            .map(|(action, words)| (*action, words.iter().map(|w| stem(fold_umlauts(w))).collect()))
            .collect()
    })
}

fn action_synonyms(action: &str) -> impl Iterator<Item = &'static String> {
    action_lexicon()
        .iter()
        .filter(move |(a, _)| *a == action)
        .flat_map(|(_, words)| words)
}

/// Tie-break for equally scored operations (e.g. a bare resource noun).
fn action_rank(action: &str) -> usize {
    const ORDER: &[&str] = &["list", "get", "create", "update", "download", "delete"];
    ORDER.iter().position(|a| *a == action).unwrap_or(ORDER.len())
}

/// Caches `match_quality` per distinct catalog token: most tokens (resource
/// names, keywords) repeat across operations, and fuzzy matching is the costly part.
#[derive(Default, Clone)]
struct QualityMemo(std::collections::HashMap<String, f64>);

impl QualityMemo {
    fn quality(&mut self, q: &str, d: &str) -> f64 {
        if let Some(&v) = self.0.get(d) {
            return v;
        }
        let v = match_quality(q, d);
        self.0.insert(d.to_string(), v);
        v
    }
}

pub struct Hit<'a> {
    pub op: &'a Operation,
    pub score: f64,
    /// Share of query words this operation matched.
    pub coverage: f64,
}

pub fn search(query: &str) -> Vec<Hit<'static>> {
    let cat = catalog();
    let q_tokens = tokens(query);
    if q_tokens.is_empty() {
        return Vec::new();
    }
    let mut memo = vec![QualityMemo::default(); q_tokens.len()];
    let resource_index: std::collections::HashMap<&str, Vec<Field>> = cat
        .resources
        .iter()
        .map(|r| (r.name.as_str(), resource_fields(r)))
        .collect();
    let mut hits: Vec<Hit> = cat
        .operations
        .iter()
        .filter_map(|op| {
            let res_fields = resource_index.get(op.resource())?;
            let own_fields = op_fields(op);
            let mut total = 0.0;
            let mut matched = 0;
            for (qi, q) in q_tokens.iter().enumerate() {
                let mut best: f64 = 0.0;
                for field in res_fields.iter().chain(&own_fields) {
                    for d in &field.tokens {
                        best = best.max(field.weight * memo[qi].quality(q, d));
                    }
                }
                if action_synonyms(op.action()).any(|s| match_quality(q, s) >= 0.75) {
                    best = best.max(2.5);
                }
                if best > 0.0 {
                    matched += 1;
                }
                total += best;
            }
            let coverage = matched as f64 / q_tokens.len() as f64;
            // Prefer the resource whose whole name the query covers
            // ("invoice" -> invoices rather than down-payment-invoices).
            // resource_fields() puts the resource name first.
            let name_tokens = &res_fields[0].tokens;
            let named = name_tokens
                .iter()
                .filter(|n| q_tokens.iter().any(|q| match_quality(q, n) >= 0.5))
                .count();
            let specificity = named as f64 / name_tokens.len().max(1) as f64;
            let mut score = total * coverage * coverage * (1.0 + 0.2 * specificity);
            if op.deprecated.is_some() {
                score *= 0.5;
            }
            if op.access == Access::Partner {
                score *= 0.85;
            }
            (score > 0.0).then_some(Hit { op, score, coverage })
        })
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| action_rank(a.op.action()).cmp(&action_rank(b.op.action())))
            .then_with(|| a.op.id.cmp(&b.op.id))
    });
    if let Some(top) = hits.first().map(|h| h.score) {
        hits.retain(|h| h.score >= top * 0.25);
    }
    hits
}

/// Field-name matches in the docs field reference, e.g. `buyerReference`.
fn field_hits(query: &str) -> Vec<(String, String, String)> {
    let cat = catalog();
    let words: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .map(fold_umlauts)
        .filter(|w| w.len() >= 4)
        .filter(|w| {
            let w = stem(w.clone());
            let is_action = action_lexicon().iter().any(|(_, words)| words.contains(&w));
            let is_resource = cat
                .resources
                .iter()
                .any(|r| tokens(&r.name).contains(&w) || r.keywords.iter().any(|k| tokens(k).contains(&w)));
            !is_action && !is_resource
        })
        .collect();
    let mut out = Vec::new();
    if words.is_empty() {
        return out;
    }
    for r in &cat.resources {
        for sid in &r.schema {
            let Some(section) = doc_section(sid) else { continue };
            let Some(objects) = section.get("objects").and_then(Value::as_array) else {
                continue;
            };
            for obj in objects {
                let oname = obj["object"].as_str().unwrap_or("");
                for f in obj["fields"].as_array().into_iter().flatten() {
                    let name = f["name"].as_str().unwrap_or("");
                    let lname = fold_umlauts(name);
                    if words.iter().any(|w| lname == *w || lname.contains(w.as_str())) {
                        let path = if oname == "root" {
                            name.to_string()
                        } else {
                            format!("{oname} > {name}")
                        };
                        out.push((
                            r.name.clone(),
                            path,
                            f["description"].as_str().unwrap_or("").to_string(),
                        ));
                    }
                }
            }
        }
    }
    out.sort_by_key(|(_, p, _)| p.len());
    out.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    out.truncate(6);
    out
}

fn tags(op: &Operation) -> Vec<&'static str> {
    let mut t = Vec::new();
    if op.access == Access::Partner {
        t.push("partner-api-only");
    }
    if op.deprecated.is_some() {
        t.push("deprecated");
    }
    if op.confirm.is_some() || op.query.iter().any(|q| q.confirm.is_some()) {
        t.push(if op.confirm.is_some() {
            "needs --yes"
        } else {
            "some flags need --yes"
        });
    }
    t
}

pub fn command_of(op: &Operation) -> String {
    format!("lxw {} {}", op.resource(), op.action())
}

fn command_entry(op: &Operation) -> Value {
    let mut v = json!({ "command": command_of(op), "summary": op.summary });
    let t = tags(op);
    if !t.is_empty() {
        v["tags"] = json!(t);
    }
    v
}

/// `lxw cli search <query>`
pub fn run_search(m: &ArgMatches, g: &Globals) -> Result<(), CliError> {
    let query: Vec<String> = m.get_many::<String>("query").expect("required").cloned().collect();
    let query = query.join(" ");
    let limit = *m.get_one::<usize>("limit").expect("has default");
    let hits = search(&query);
    let commands = hits.iter().map(|h| command_entry(h.op));
    // Body field names (e.g. `buyerReference`) lead when no command matches every query word.
    let fields_first = hits.first().is_none_or(|h| h.coverage < 1.0);
    let fields: Vec<Value> = if fields_first {
        field_hits(&query)
            .into_iter()
            .take(3)
            .map(|(resource, field, description)| {
                json!({
                    "field": field,
                    "resource": resource,
                    "summary": truncate(&description, 120),
                    "see": format!("lxw schema {resource}"),
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    let results: Vec<Value> = fields.into_iter().chain(commands).take(limit).collect();
    output::print_json(&Value::Array(results), g.pretty);
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max).collect::<String>())
    }
}

// ---------------------------------------------------------------------------
// schema
// ---------------------------------------------------------------------------

/// Resolves `invoices create`, `invoices.create` or `lxw invoices create`.
enum SchemaTarget {
    Operation(&'static Operation),
    Resource(&'static Resource),
}

fn resolve_target(words: &[String]) -> Result<SchemaTarget, CliError> {
    let cat = catalog();
    let words: Vec<&str> = words.iter().map(String::as_str).skip_while(|w| *w == "lxw").collect();
    let joined = words.join(".");
    if let Some(op) = cat.find_operation(&joined) {
        return Ok(SchemaTarget::Operation(op));
    }
    if let Some(r) = cat.resource(&joined) {
        return Ok(SchemaTarget::Resource(r));
    }
    let suggestions: Vec<String> = search(&words.join(" "))
        .iter()
        .take(3)
        .map(|h| command_of(h.op))
        .collect();
    let mut err = CliError::new(Kind::Usage, format!("no command or resource {:?}", words.join(" ")));
    err = err.with_hint(if suggestions.is_empty() {
        "Find commands with `lxw cli search \"<task>\"` or list them with `lxw schema --list`.".to_string()
    } else {
        format!("Did you mean: {}?", suggestions.join(", "))
    });
    Err(err)
}

pub fn run_schema(m: &ArgMatches, g: &Globals) -> Result<(), CliError> {
    if m.get_flag("list") {
        let list: Vec<Value> = catalog().operations.iter().map(command_entry).collect();
        output::print_json(&Value::Array(list), g.pretty);
        return Ok(());
    }
    let Some(words) = m.get_many::<String>("command") else {
        return Err(CliError::usage("usage: lxw schema <command...>  or  lxw schema --list")
            .with_hint("Example: lxw schema invoices create"));
    };
    let words: Vec<String> = words.cloned().collect();
    let v = match resolve_target(&words)? {
        SchemaTarget::Operation(op) => operation_schema(op, m.get_flag("response")),
        SchemaTarget::Resource(res) => resource_schema(
            res,
            m.get_one::<String>("object").map(String::as_str),
            m.get_flag("brief"),
        )?,
    };
    output::print_json(&v, g.pretty);
    Ok(())
}

fn operation_schema(op: &Operation, with_response: bool) -> Value {
    let section = doc_section(&op.docs);
    let mut v = Map::new();
    v.insert("command".into(), json!(command_of(op)));
    v.insert("summary".into(), json!(op.summary));
    v.insert("httpMethod".into(), json!(op.method));
    v.insert("path".into(), json!(op.path));
    v.insert("access".into(), json!(op.access));
    if !op.scopes.is_empty() {
        v.insert("oauthScopes".into(), json!(op.scopes));
    }
    if let Some(c) = &op.confirm {
        v.insert("needsYes".into(), json!(c));
    }
    if let Some(d) = &op.deprecated {
        v.insert("deprecated".into(), json!(d));
    }
    v.insert("usage".into(), json!(op.usage()));

    let path_params: Vec<Value> = op
        .path_params()
        .iter()
        .map(|p| json!({ "name": p, "positional": positional_name(p), "type": "uuid", "required": true }))
        .collect();
    if !path_params.is_empty() {
        v.insert("pathParams".into(), Value::Array(path_params));
    }
    let mut query: Vec<Value> = op
        .query
        .iter()
        .map(|q| {
            let mut p = json!({ "name": q.name, "flag": format!("--{}", kebab(&q.name)), "type": q.ty, "description": q.description });
            if q.required {
                p["required"] = json!(true);
            }
            if let Some(d) = &q.default {
                p["default"] = json!(d);
            }
            if !q.values.is_empty() {
                p["values"] = json!(q.values);
            }
            if q.html_encode {
                p["htmlEscapedByCli"] = json!(true);
            }
            if let Some(c) = &q.confirm {
                p["needsYes"] = json!(c);
            }
            if q.ty == ParamType::Flag {
                p["sends"] = json!(format!("{}=true", q.name));
            }
            p
        })
        .collect();
    for f in &op.form {
        query.push(json!({
            "name": f.name, "flag": format!("--{}", kebab(&f.name)), "type": "multipart-field",
            "values": f.values, "default": f.default, "description": f.description,
        }));
    }
    if op.is_paged() {
        query.push(json!({
            "name": "page/size/sort", "flag": "--page N --size N --all --max-items N --ndjson",
            "maxPageSize": op.max_page_size, "sort": op.sort,
            "description": "Paging (0-based pages). --all fetches every page into one JSON array.",
        }));
    }
    if !query.is_empty() {
        v.insert("queryParams".into(), Value::Array(query));
    }

    match op.body {
        crate::catalog::BodyKind::None => {}
        crate::catalog::BodyKind::Multipart => {
            v.insert(
                "requestBody".into(),
                json!({
                    "type": "multipart/form-data",
                    "flag": "--file PATH",
                    "limits": op.upload.iter().map(|u| json!({
                        "type": u.form_type, "maxMb": u.max_mb, "extensions": u.extensions,
                    })).collect::<Vec<_>>(),
                }),
            );
        }
        kind => {
            let mut body = Map::new();
            body.insert(
                "type".into(),
                json!(if kind == crate::catalog::BodyKind::JsonArray {
                    "json-array"
                } else {
                    "json-object"
                }),
            );
            body.insert("flags".into(), json!("--body JSON|@FILE|-  and/or  --set PATH=VALUE"));
            if let Some(field) = op.lock_field() {
                body.insert(
                    "merge".into(),
                    json!(format!("--merge: GET the current resource, apply the body as a JSON merge patch, PUT with the current `{field}`")),
                );
            }
            if let Some(Value::Array(required)) = section.as_ref().and_then(|s| s.get("required")) {
                body.insert("requiredFields".into(), compact_required(required));
            }
            if let Some(ex) = section.as_ref().and_then(|s| s.get("requestExample")) {
                body.insert("example".into(), ex.clone());
            }
            body.insert(
                "fieldReference".into(),
                if has_docs() {
                    json!(format!("lxw schema {}", op.resource()))
                } else {
                    json!(format!("not bundled in this build; see {}", op.docs_url()))
                },
            );
            v.insert("requestBody".into(), Value::Object(body));
        }
    }

    let mut response = Map::new();
    match op.response {
        ResponseKind::Binary => {
            response.insert("type".into(), json!("file"));
            response.insert("accept".into(), json!(op.accept));
            response.insert(
                "output".into(),
                json!("Saved to --out (file, directory or - for stdout); prints {file, bytes, contentType}"),
            );
        }
        ResponseKind::Empty => {
            response.insert("type".into(), json!("empty (prints {\"ok\":true,\"status\":204})"));
        }
        ResponseKind::Json => {
            response.insert(
                "type".into(),
                json!(if op.is_paged() {
                    "json page {content[], first, last, totalPages, totalElements, number, size}"
                } else {
                    "json"
                }),
            );
            if with_response {
                if let Some(ex) = section.as_ref().and_then(|s| s.get("responseExample")) {
                    response.insert("example".into(), ex.clone());
                }
            } else {
                response.insert(
                    "example".into(),
                    json!(format!("lxw schema {} {} --response", op.resource(), op.action())),
                );
            }
        }
    }
    v.insert("response".into(), Value::Object(response));
    if !op.notes.is_empty() {
        v.insert("notes".into(), json!(op.notes));
    }
    v.insert("docs".into(), json!(op.docs_url()));
    Value::Object(v)
}

/// [{object, fields:[{name, required, notes}]}] -> {"root": ["voucherDate: Yes", ...], ...}
fn compact_required(required: &[Value]) -> Value {
    let mut out = Map::new();
    for obj in required {
        let name = obj["object"].as_str().unwrap_or("root").to_string();
        let fields: Vec<Value> = obj["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|f| {
                let n = f["name"].as_str().unwrap_or("");
                let r = f["required"].as_str().unwrap_or("");
                let notes = f["notes"].as_str().unwrap_or("");
                let r = if r == "*" { "conditional" } else { r };
                json!(if notes.is_empty() {
                    format!("{n}: {r}")
                } else {
                    format!("{n}: {r} - {notes}")
                })
            })
            .collect();
        out.insert(name, Value::Array(fields));
    }
    Value::Object(out)
}

fn resource_schema(res: &Resource, object: Option<&str>, brief: bool) -> Result<Value, CliError> {
    let commands: Vec<Value> = catalog().operations_of(&res.name).map(command_entry).collect();
    let filter = object.map(str::to_lowercase);
    let mut objects = Vec::new();
    for sid in &res.schema {
        let Some(section) = doc_section(sid) else { continue };
        for obj in section["objects"].as_array().into_iter().flatten() {
            let name = obj["object"].as_str().unwrap_or("");
            if filter
                .as_ref()
                .is_some_and(|f| !name.to_lowercase().contains(f.as_str()))
            {
                continue;
            }
            let fields: Vec<Value> = obj["fields"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|f| {
                    let name = f["name"].as_str().unwrap_or("");
                    let ty = f["type"].as_str().unwrap_or("");
                    let ro = f["readOnly"].as_bool() == Some(true);
                    if brief {
                        json!(format!("{name}: {ty}{}", if ro { " (read-only)" } else { "" }))
                    } else {
                        let mut v = json!({ "name": name, "type": ty, "description": f["description"] });
                        if ro {
                            v["readOnly"] = json!(true);
                        }
                        v
                    }
                })
                .collect();
            objects.push(json!({ "object": name, "fields": fields }));
        }
    }
    if objects.is_empty() && filter.is_some() && has_docs() {
        return Err(CliError::usage(format!("no object of {} matches --object", res.name)));
    }
    let mut v = json!({
        "resource": res.name,
        "title": res.title,
        "summary": res.summary,
        "access": res.access,
        "commands": commands,
        "docs": res.docs_url(),
    });
    if !objects.is_empty() {
        v["objects"] = Value::Array(objects);
        v["objectsNote"] = json!("'root' is the resource itself; other objects are nested fields named in root.");
    } else if !has_docs() {
        v["objectsNote"] = json!("The field reference is not bundled in this build; see the docs URL.");
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn top(query: &str) -> String {
        search(query).first().map(|h| h.op.id.clone()).unwrap_or_default()
    }

    #[test]
    fn tokenizer_handles_german_and_camel_case() {
        assert_eq!(tokens("Rechnungen"), ["rechnung"]);
        assert_eq!(
            tokens("precedingSalesVoucherId"),
            ["preceding", "sale", "voucher", "id"]
        );
        assert_eq!(tokens("Auftragsbestätigung"), tokens("auftragsbestaetigung"));
    }

    #[test]
    fn search_finds_the_obvious_operation() {
        assert_eq!(top("create invoice"), "invoices.create");
        assert_eq!(top("rechnung erstellen"), "invoices.create");
        assert_eq!(top("download invoice pdf"), "invoices.download");
        assert_eq!(top("rechnung pdf"), "invoices.download");
        assert_eq!(top("find customer by name"), "contacts.list");
        assert_eq!(top("kunde anlegen"), "contacts.create");
        assert_eq!(top("list overdue invoices"), "voucherlist.list");
        assert_eq!(top("angebot"), "quotations.get");
        assert_eq!(top("upload receipt"), "files.upload");
        assert_eq!(top("webhook"), "event-subscriptions.list");
        assert_eq!(top("create webhook"), "event-subscriptions.create");
        assert_eq!(top("who am i"), "profile.get");
        assert_eq!(top("payment status of invoice"), "payments.get");
        assert_eq!(top("send invoice by email"), "invoices.send-email");
        assert_eq!(top("convert quotation to invoice"), "invoices.create");
        assert_eq!(top("update contact address"), "contacts.update");
        assert_eq!(top("mahnung"), "dunnings.get");
        assert_eq!(top("posting category id"), "posting-categories.list");
        assert_eq!(top("invioce"), "invoices.get", "typo tolerance");
    }

    #[cfg(lxw_docs)]
    #[test]
    fn field_search_finds_body_fields() {
        let hits = field_hits("buyerReference");
        assert!(
            hits.iter()
                .any(|(r, p, _)| r == "invoices" && p.contains("buyerReference")),
            "{hits:?}"
        );
        assert!(field_hits("create invoice").is_empty());
    }
}
