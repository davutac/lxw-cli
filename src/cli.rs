//! Command tree. Resource commands are generated from the catalog so the
//! commands, `lxw cli search` and `lxw schema` can never drift apart.
//! Layout follows the Cloudflare `cf` CLI: an agent banner on every help page,
//! `cli search` for discovery, and `schema` mirroring every command path.

use crate::catalog::{BodyKind, Operation, ParamType, Resource, ResponseKind, catalog, kebab, positional_name};
use clap::builder::PossibleValuesParser;
use clap::{Arg, ArgAction, ArgMatches, Command, value_parser};
use std::io::IsTerminal;

const ABOUT: &str = "Unofficial, agent-friendly CLI for the Lexware Office API (not affiliated with Lexware)";

/// Shown at the top of every help page (like `cf`).
const AGENT_BANNER: &str = "\
=== AGENT COMMAND DISCOVERY ===
AGENTS: Do not explore commands by chaining nested --help calls. Instead run:
  lxw cli search \"<describe the task you want to accomplish>\"
It returns up to five compact JSON matches (English or German queries work).
Pick the best match instead of repeating similar searches.
Run `<discovered command> --help` for flags. For the API request, required body
fields and an example body, replace the leading `lxw` with `lxw schema`.
Conventions, auth, exit codes and rate limits: `lxw cli guide`.
=== END AGENT COMMAND DISCOVERY ===";

const AFTER_HELP: &str = "\
Auth: set LXW_API_KEY (create a key at https://app.lexware.de/addons/public-api)
      or run `lxw auth login`.
Output: JSON on stdout; errors as JSON on stderr.
Exit codes: 0 ok, 2 usage, 3 auth, 4 not found, 5 validation, 6 conflict,
            7 rate limited, 8 server/network, 9 payment required, 10 needs --yes";

pub struct Globals {
    pub profile: Option<String>,
    pub pretty: bool,
    pub verbose: bool,
    pub dry_run: bool,
    pub yes: bool,
    pub max_retries: u32,
    pub timeout_secs: u64,
    pub base_url: Option<String>,
    pub rate_limit: Option<f64>,
}

impl Globals {
    /// Global args propagate down, so read them from the innermost subcommand.
    pub fn from_matches(top: &ArgMatches) -> Globals {
        let mut m = top;
        while let Some((_, sub)) = m.subcommand() {
            m = sub;
        }
        fn env_num<T: std::str::FromStr>(k: &str) -> Option<T> {
            std::env::var(k).ok().and_then(|v| v.parse().ok())
        }
        Globals {
            profile: m.get_one::<String>("profile").cloned(),
            pretty: if m.get_flag("pretty") {
                true
            } else if m.get_flag("compact") {
                false
            } else {
                std::io::stdout().is_terminal()
            },
            verbose: m.get_flag("verbose"),
            dry_run: m.get_flag("dry-run"),
            yes: m.get_flag("yes"),
            max_retries: m
                .get_one::<u32>("max-retries")
                .copied()
                .or_else(|| env_num("LXW_MAX_RETRIES"))
                .unwrap_or(4),
            timeout_secs: m
                .get_one::<u64>("timeout")
                .copied()
                .or_else(|| env_num("LXW_TIMEOUT"))
                .unwrap_or(60),
            base_url: m.get_one::<String>("base-url").cloned(),
            rate_limit: m.get_one::<f64>("rate-limit").copied(),
        }
    }
}

fn global_args() -> Vec<Arg> {
    let g = |a: Arg| a.global(true).help_heading("Global options");
    vec![
        g(Arg::new("profile")
            .long("profile")
            .value_name("NAME")
            .help("Config profile (default: LXW_PROFILE or 'default')")),
        g(Arg::new("pretty")
            .long("pretty")
            .action(ArgAction::SetTrue)
            .conflicts_with("compact")
            .help("Pretty-print JSON (default when stdout is a terminal)")),
        g(Arg::new("compact")
            .long("compact")
            .action(ArgAction::SetTrue)
            .help("Single-line JSON (default when piped)")),
        g(Arg::new("verbose")
            .short('v')
            .long("verbose")
            .action(ArgAction::SetTrue)
            .help("Log requests, retries and rate-limit waits to stderr")),
        g(Arg::new("dry-run")
            .long("dry-run")
            .action(ArgAction::SetTrue)
            .help("Print the request instead of sending it")),
        g(Arg::new("yes")
            .short('y')
            .long("yes")
            .action(ArgAction::SetTrue)
            .help("Confirm irreversible actions (delete, finalize, send email)")),
        g(Arg::new("max-retries")
            .long("max-retries")
            .value_name("N")
            .value_parser(value_parser!(u32))
            .help("Retries for 429/network/5xx-on-GET [default: 4, env LXW_MAX_RETRIES]")),
        g(Arg::new("timeout")
            .long("timeout")
            .value_name("SECS")
            .value_parser(value_parser!(u64))
            .help("Per-request timeout [default: 60, env LXW_TIMEOUT]")),
        g(Arg::new("base-url")
            .long("base-url")
            .value_name("URL")
            .help("API base URL [default: https://api.lexware.io, env LXW_BASE_URL]")),
        g(Arg::new("rate-limit")
            .long("rate-limit")
            .value_name("RPS")
            .value_parser(value_parser!(f64))
            .help(
                "Client-side requests/second, shared by all processes; 0 disables [default: 1.8, env LXW_RATE_LIMIT]",
            )),
    ]
}

pub fn build() -> Command {
    let mut cmd = Command::new("lxw")
        .version(env!("CARGO_PKG_VERSION"))
        .about(ABOUT)
        .after_help(AFTER_HELP)
        .arg_required_else_help(true)
        .subcommand_value_name("COMMAND")
        .args(global_args())
        .subcommand(cli_command().display_order(1))
        .subcommand(
            Command::new("schema")
                .about("Show the API request behind a command, or a resource's commands and fields")
                .long_about(
                    "Show the API request behind a command: method, path, parameters, required body \
                     fields and a documented example body. Mirrors the command path: \
                     `lxw invoices create` -> `lxw schema invoices create`.\n\n\
                     With only a resource (`lxw schema invoices`) it lists the resource's commands \
                     and the field reference of its JSON objects.",
                )
                .display_order(2)
                .arg(
                    Arg::new("command")
                        .num_args(1..)
                        .value_name("COMMAND")
                        .help("Command words, e.g. `invoices create`, or a resource"),
                )
                .arg(
                    Arg::new("list")
                        .long("list")
                        .action(ArgAction::SetTrue)
                        .conflicts_with("command")
                        .help("List all commands"),
                )
                .arg(
                    Arg::new("response")
                        .long("response")
                        .action(ArgAction::SetTrue)
                        .help("Include the documented example response"),
                )
                .arg(
                    Arg::new("object")
                        .long("object")
                        .value_name("NAME")
                        .help("Resource only: objects whose name contains NAME, e.g. 'line items'"),
                )
                .arg(
                    Arg::new("brief")
                        .long("brief")
                        .action(ArgAction::SetTrue)
                        .help("Resource only: field names and types without descriptions"),
                ),
        )
        .subcommand(auth_command().display_order(3))
        .subcommand(config_command().display_order(4))
        .subcommand(request_command().display_order(5));
    for r in &catalog().resources {
        cmd = cmd.subcommand(resource_command(r));
    }
    with_banner(cmd)
}

fn with_banner(cmd: Command) -> Command {
    cmd.before_help(AGENT_BANNER).mut_subcommands(with_banner)
}

fn cli_command() -> Command {
    Command::new("cli")
        .about("Discover commands (search) and read the agent guide")
        .subcommand_required(true)
        .subcommand(
            Command::new("search")
                .about("Search all commands by intent; returns compact JSON matches")
                .arg(
                    Arg::new("query")
                        .required(true)
                        .num_args(1..)
                        .value_name("QUERY")
                        .help("What you want to do, e.g. \"create invoice\" or \"rechnung als pdf\""),
                )
                .arg(
                    Arg::new("limit")
                        .long("limit")
                        .value_name("N")
                        .value_parser(value_parser!(usize))
                        .default_value("5"),
                ),
        )
        .subcommand(
            Command::new("guide").about("Conventions: auth, bodies, paging, safety, errors, exit codes, rate limits"),
        )
}

fn auth_command() -> Command {
    Command::new("auth")
        .about("Log in with an API key (or OAuth for Partner API), check or remove credentials")
        .subcommand_required(true)
        .subcommand(
            Command::new("login")
                .about("Store an API key (read from stdin) or run the OAuth2 partner flow (--oauth)")
                .arg(
                    Arg::new("with-token")
                        .long("with-token")
                        .action(ArgAction::SetTrue)
                        .help("Read the API key from stdin"),
                )
                .arg(
                    Arg::new("no-verify")
                        .long("no-verify")
                        .action(ArgAction::SetTrue)
                        .help("Do not call /v1/profile to verify"),
                )
                .arg(
                    Arg::new("store")
                        .long("store")
                        .value_parser(["os", "file"])
                        .default_value("os")
                        .help("Where to keep secrets: the OS credential store, or the config file in plaintext"),
                )
                .arg(
                    Arg::new("oauth")
                        .long("oauth")
                        .action(ArgAction::SetTrue)
                        .help("Partner API: OAuth2 authorization code flow with PKCE"),
                )
                .arg(
                    Arg::new("client-id")
                        .long("client-id")
                        .value_name("ID")
                        .help("OAuth client id (or LXW_CLIENT_ID); secret via LXW_CLIENT_SECRET or prompt"),
                )
                .arg(
                    Arg::new("redirect-uri")
                        .long("redirect-uri")
                        .value_name("URI")
                        .help("Registered redirect URI; http://127.0.0.1:PORT/... is captured automatically"),
                )
                .arg(
                    Arg::new("scope")
                        .long("scope")
                        .value_name("SCOPES")
                        .help("Space-separated scopes (default: all scopes of the client)"),
                )
                .arg(
                    Arg::new("connection-name")
                        .long("connection-name")
                        .value_name("NAME")
                        .help("Name shown for this connection in Lexware"),
                )
                .arg(
                    Arg::new("sandbox")
                        .long("sandbox")
                        .action(ArgAction::SetTrue)
                        .help("Use the Lexware partner sandbox"),
                )
                .arg(
                    Arg::new("no-browser")
                        .long("no-browser")
                        .action(ArgAction::SetTrue)
                        .help("Do not try to open a browser"),
                ),
        )
        .subcommand(
            Command::new("status")
                .about("Show which credentials are used and verify them via /v1/profile")
                .arg(
                    Arg::new("offline")
                        .long("offline")
                        .action(ArgAction::SetTrue)
                        .help("Do not call the API"),
                ),
        )
        .subcommand(Command::new("list").about("List stored profiles (secrets masked) and which one is active"))
        .subcommand(Command::new("logout").about("Remove stored credentials of the profile (revokes OAuth tokens)"))
}

fn config_command() -> Command {
    Command::new("config")
        .about("Show the config file location and settings")
        .subcommand_required(true)
        .subcommand(Command::new("path").about("Print the config file path"))
        .subcommand(Command::new("show").about("Print the config with secrets masked"))
        .subcommand(
            Command::new("set")
                .about("Set a profile setting: base_url, requests_per_second, burst, or default_profile")
                .arg(Arg::new("key").required(true).value_parser([
                    "base_url",
                    "requests_per_second",
                    "burst",
                    "default_profile",
                ]))
                .arg(Arg::new("value").required(true)),
        )
}

fn request_command() -> Command {
    let cmd = Command::new("request")
        .about("Raw call to a documented endpoint with custom query/body (rate-limited, retried, errors normalized)")
        .arg(
            Arg::new("method")
                .required(true)
                .value_parser(["GET", "POST", "PUT", "DELETE"])
                .ignore_case(true),
        )
        .arg(
            Arg::new("path")
                .required(true)
                .value_name("PATH")
                .help("e.g. /v1/contacts or contacts (prefix /v1 is added)"),
        )
        .arg(
            Arg::new("accept")
                .long("accept")
                .value_name("MIME")
                .help("Accept header [default: application/json]"),
        )
        .arg(
            Arg::new("out")
                .long("out")
                .value_name("FILE|DIR|-")
                .help("Save the response body to a file (for binary responses)"),
        );
    paging_args(body_args(query_args(cmd), false), None, true)
}

fn query_args(cmd: Command) -> Command {
    cmd.arg(
        Arg::new("query")
            .long("query")
            .value_name("KEY=VALUE")
            .action(ArgAction::Append)
            .help("Extra query parameter (repeatable), sent as-is"),
    )
    .arg(
        Arg::new("fields")
            .long("fields")
            .value_name("A,B.C")
            .value_delimiter(',')
            .action(ArgAction::Append)
            .help("Only output these fields (dot paths; applied per item for lists)"),
    )
}

fn body_args(cmd: Command, merge: bool) -> Command {
    let cmd = cmd
        .arg(
            Arg::new("body")
                .long("body")
                .value_name("JSON|@FILE|-")
                .allow_hyphen_values(true)
                .help("Request body: inline JSON, @file.json, or - for stdin"),
        )
        .arg(
            Arg::new("set")
                .long("set")
                .value_name("PATH=VALUE")
                .action(ArgAction::Append)
                .help("Set a body field, e.g. --set address.contactId=<uuid> --set 'lineItems[0].quantity=2' (value parsed as JSON if valid)"),
        );
    if merge {
        cmd.arg(Arg::new("merge").long("merge").action(ArgAction::SetTrue).help(
            "Fetch the current resource, apply the body as a JSON merge patch and send it with the current version",
        ))
    } else {
        cmd
    }
}

fn paging_args(cmd: Command, op: Option<&Operation>, raw: bool) -> Command {
    let max = op.and_then(|o| o.max_page_size);
    let size_help = match max {
        Some(m) => format!("Page size (default 25, max {m})"),
        None => "Page size".to_string(),
    };
    let mut cmd = cmd
        .arg(
            Arg::new("page")
                .long("page")
                .value_name("N")
                .value_parser(value_parser!(u32))
                .help("Page index, 0-based")
                .help_heading("Paging"),
        )
        .arg(
            Arg::new("size")
                .long("size")
                .value_name("N")
                .value_parser(value_parser!(u32))
                .help(size_help)
                .help_heading("Paging"),
        )
        .arg(
            Arg::new("all")
                .long("all")
                .action(ArgAction::SetTrue)
                .help("Fetch all pages and print one JSON array")
                .help_heading("Paging"),
        )
        .arg(
            Arg::new("max-items")
                .long("max-items")
                .value_name("N")
                .value_parser(value_parser!(usize))
                .help("Stop after N items (with --all)")
                .help_heading("Paging"),
        )
        .arg(
            Arg::new("ndjson")
                .long("ndjson")
                .action(ArgAction::SetTrue)
                .requires("all")
                .help("With --all: print one JSON object per line as pages arrive")
                .help_heading("Paging"),
        );
    let sort = op.map(|o| o.sort.clone()).unwrap_or_default();
    if raw || !sort.is_empty() {
        let help = if sort.is_empty() {
            "Sort, e.g. voucherDate,DESC".to_string()
        } else {
            format!("Sort by {} with optional ,ASC or ,DESC", sort.join("|"))
        };
        cmd = cmd.arg(
            Arg::new("sort")
                .long("sort")
                .value_name("FIELD[,DIR]")
                .help(help)
                .help_heading("Paging"),
        );
    }
    cmd
}

fn resource_command(r: &Resource) -> Command {
    let partner = r.access == crate::catalog::Access::Partner;
    let about = if partner {
        format!("{} [Partner API only]", r.title)
    } else {
        r.title.clone()
    };
    Command::new(r.name.clone())
        .about(about)
        .long_about(format!("{}\n\n{}\nDocs: {}", r.title, r.summary, r.docs_url()))
        .subcommand_required(true)
        .arg_required_else_help(true)
        .display_order(100)
        .defer(add_operations)
}

/// Builds a resource's operation subcommands only when that resource is used,
/// so each call constructs a handful of commands instead of all ~90.
fn add_operations(cmd: Command) -> Command {
    let resource = cmd.get_name().to_string();
    catalog().operations_of(&resource).fold(cmd, |cmd, op| {
        cmd.subcommand(operation_command(op).before_help(AGENT_BANNER))
    })
}

fn operation_command(op: &Operation) -> Command {
    let mut tags = Vec::new();
    if op.is_partner() {
        tags.push("partner API only");
    }
    if op.deprecated.is_some() {
        tags.push("deprecated");
    }
    if op.confirm.is_some() {
        tags.push("needs --yes");
    }
    let about = if tags.is_empty() {
        op.summary.clone()
    } else {
        format!("{} [{}]", op.summary, tags.join(", "))
    };
    let mut long = format!("{}\n\n{} {}", op.summary, op.method, op.path);
    if let Some(d) = &op.deprecated {
        long.push_str(&format!("\nDeprecated: {d}"));
    }
    if let Some(c) = &op.confirm {
        long.push_str(&format!("\nNeeds --yes: {c}"));
    }
    for n in &op.notes {
        long.push_str(&format!("\n- {n}"));
    }
    long.push_str(&format!("\n\nDocs: {}", op.docs_url()));

    let mut cmd = Command::new(op.action().to_string())
        .about(about)
        .long_about(long)
        .after_help(format!(
            "To inspect the exact API request, run `lxw schema {} {}`",
            op.resource(),
            op.action()
        ));
    for p in op.path_params() {
        cmd = cmd.arg(
            Arg::new(format!("path:{p}"))
                .required(true)
                .value_name(positional_name(p))
                .help(format!("{p} (UUID)")),
        );
    }
    for q in &op.query {
        let mut arg = Arg::new(format!("query:{}", q.name))
            .long(kebab(&q.name))
            .help_heading("Parameters");
        let mut help = q.description.clone();
        if let Some(c) = &q.confirm {
            help.push_str(&format!(" Needs --yes: {c}"));
        }
        arg = match q.ty {
            ParamType::Flag => arg.action(ArgAction::SetTrue),
            ParamType::Boolean => arg.value_name("BOOL").value_parser(["true", "false"]),
            ParamType::Enum => arg
                .value_name("VALUE")
                .value_parser(PossibleValuesParser::new(q.values.clone())),
            t => {
                if q.ty == ParamType::Csv && !q.values.is_empty() {
                    help.push_str(&format!(" Values: {}.", q.values.join(", ")));
                }
                arg.value_name(t.placeholder())
            }
        };
        if q.required {
            arg = arg.required(true);
        }
        if let Some(d) = &q.default {
            arg = arg.default_value(d.clone());
        }
        cmd = cmd.arg(arg.help(help));
    }
    for f in &op.form {
        let mut arg = Arg::new(format!("form:{}", f.name))
            .long(kebab(&f.name))
            .value_parser(PossibleValuesParser::new(f.values.clone()))
            .help(f.description.clone())
            .help_heading("Parameters");
        if let Some(d) = &f.default {
            arg = arg.default_value(d.clone());
        }
        cmd = cmd.arg(arg);
    }
    match op.body {
        BodyKind::Json | BodyKind::JsonArray => cmd = body_args(cmd, op.merge_from.is_some()),
        BodyKind::Multipart => {
            cmd = cmd.arg(
                Arg::new("file")
                    .long("file")
                    .required(true)
                    .value_name("PATH")
                    .help("File to upload"),
            );
        }
        BodyKind::None => {}
    }
    if op.is_paged() {
        cmd = paging_args(cmd, Some(op), false);
    }
    if op.response == ResponseKind::Binary {
        cmd = cmd
            .arg(
                Arg::new("out")
                    .long("out")
                    .short('o')
                    .value_name("FILE|DIR|-")
                    .help("Where to save (default: server file name in the current directory; - for stdout)"),
            )
            .arg(
                Arg::new("accept")
                    .long("accept")
                    .value_name("MIME")
                    .value_parser(PossibleValuesParser::new(op.accept.clone()))
                    .default_value(op.accept[0].clone())
                    .help("Requested representation (e.g. application/xml for XRechnung)"),
            );
    }
    query_args(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_tree_is_valid() {
        build().debug_assert();
    }

    #[test]
    fn parses_generated_operation_args() {
        let m = build()
            .try_get_matches_from([
                "lxw",
                "voucherlist",
                "list",
                "--voucher-status",
                "open,overdue",
                "--voucher-date-from",
                "2026-01-01",
                "--all",
                "--fields",
                "id,voucherNumber",
                "-v",
            ])
            .unwrap();
        let g = Globals::from_matches(&m);
        assert!(g.verbose);
        let (_, r) = m.subcommand().unwrap();
        let (_, op) = r.subcommand().unwrap();
        assert_eq!(op.get_one::<String>("query:voucherType").unwrap(), "any");
        assert_eq!(op.get_one::<String>("query:voucherStatus").unwrap(), "open,overdue");
        assert!(op.get_flag("all"));
        let fields: Vec<&String> = op.get_many::<String>("fields").unwrap().collect();
        assert_eq!(fields, ["id", "voucherNumber"]);
    }
}
