mod auth;
mod catalog;
mod cli;
mod client;
mod config;
mod discovery;
mod error;
mod exec;
mod jsonx;
mod output;
mod ratelimit;
mod secrets;

use clap::error::ErrorKind;
use cli::Globals;
use error::CliError;
use serde_json::{Value, json};
use std::io::IsTerminal;

fn main() {
    let code = match run() {
        Ok(()) => 0,
        Err((err, pretty)) => {
            output::print_error(&err, pretty);
            err.exit_code()
        }
    };
    std::process::exit(code);
}

fn run() -> Result<(), (CliError, bool)> {
    let stderr_tty = std::io::stderr().is_terminal();
    let matches = match cli::build().try_get_matches() {
        Ok(m) => m,
        Err(e) => match e.kind() {
            ErrorKind::DisplayHelp
            | ErrorKind::DisplayVersion
            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
                // Help is a successful answer, on stdout.
                output::print_text(&e.render().to_string());
                return Ok(());
            }
            _ => {
                let msg = e.render().to_string();
                let msg = msg.trim().trim_start_matches("error: ").to_string();
                return Err((
                    CliError::usage(msg).with_hint(
                        "Find commands with `lxw cli search \"<task>\"`; inspect one with `lxw schema <command>`.",
                    ),
                    stderr_tty,
                ));
            }
        },
    };
    let g = Globals::from_matches(&matches);
    let pretty_err = stderr_tty || g.pretty;
    let result = match matches.subcommand() {
        Some(("cli", m)) => match m.subcommand() {
            Some(("search", sub)) => discovery::run_search(sub, &g),
            _ => {
                output::print_text(discovery::GUIDE);
                Ok(())
            }
        },
        Some(("schema", m)) => discovery::run_schema(m, &g),
        Some(("auth", m)) => auth::run(m, &g).map(|v| output::print_json(&v, g.pretty)),
        Some(("config", m)) => run_config(m, &g),
        Some(("request", m)) => exec::run_raw(m, &g),
        Some((resource, m)) => exec::dispatch(resource, m, &g),
        None => Ok(()),
    };
    result.map_err(|e| (e, pretty_err))
}

fn run_config(m: &clap::ArgMatches, g: &Globals) -> Result<(), CliError> {
    match m.subcommand() {
        Some(("path", _)) => {
            output::print_text(&config::config_path().display().to_string());
            Ok(())
        }
        Some(("show", _)) => {
            let cfg = config::Config::load()?;
            let mut v = serde_json::to_value(&cfg).map_err(|e| CliError::internal(e.to_string()))?;
            if let Some(Value::Object(profiles)) = v.get_mut("profiles") {
                for p in profiles.values_mut().filter_map(Value::as_object_mut) {
                    for secret in ["api_key", "client_secret", "access_token", "refresh_token"] {
                        if p.contains_key(secret) {
                            p.insert(secret.into(), json!("<redacted>"));
                        }
                    }
                }
            }
            v["path"] = json!(config::config_path());
            v["activeProfile"] = json!(cfg.profile_name(g.profile.as_deref()));
            output::print_json(&v, g.pretty);
            Ok(())
        }
        Some(("set", sub)) => {
            let key = sub.get_one::<String>("key").expect("required").clone();
            let value = sub.get_one::<String>("value").expect("required").clone();
            let profile = config::update(|cfg| {
                let name = cfg.profile_name(g.profile.as_deref());
                if key == "default_profile" {
                    cfg.default_profile = Some(value.clone());
                    return Ok(name);
                }
                let p = cfg.profiles.entry(name.clone()).or_default();
                let bad = |what: &str| CliError::usage(format!("{key} must be {what}"));
                match key.as_str() {
                    "base_url" => p.base_url = Some(value.trim_end_matches('/').to_string()),
                    "requests_per_second" => p.requests_per_second = Some(value.parse().map_err(|_| bad("a number"))?),
                    "burst" => p.burst = Some(value.parse().map_err(|_| bad("a positive integer"))?),
                    _ => unreachable!("clap restricts keys"),
                }
                Ok(name)
            })?;
            output::print_json(&json!({ "updated": key, "value": value, "profile": profile }), g.pretty);
            Ok(())
        }
        _ => Err(CliError::usage("unknown config command")),
    }
}
