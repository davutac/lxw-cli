//! stdout carries results (JSON); stderr carries errors (JSON) and notes.

use crate::error::CliError;
use serde_json::Value;
use std::io::{BufWriter, Write};

/// Serializes straight into the stream. Write errors (e.g. a closed pipe
/// after `| head`) are not worth reporting.
fn write_json(out: impl Write, value: &Value, pretty: bool) {
    let mut out = BufWriter::new(out);
    let _ = if pretty {
        serde_json::to_writer_pretty(&mut out, value)
    } else {
        serde_json::to_writer(&mut out, value)
    };
    let _ = writeln!(out);
    let _ = out.flush();
}

pub fn print_json(value: &Value, pretty: bool) {
    write_json(std::io::stdout().lock(), value, pretty);
}

pub fn print_error(err: &CliError, pretty: bool) {
    write_json(std::io::stderr().lock(), &err.to_json(), pretty);
}

pub fn print_text(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "{text}");
    if !text.ends_with('\n') {
        let _ = writeln!(out);
    }
}

pub fn note(msg: impl AsRef<str>) {
    eprintln!("lxw: {}", msg.as_ref());
}
