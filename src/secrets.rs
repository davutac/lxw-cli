//! OS credential store for profile secrets (API key, OAuth client secret and tokens).
//!
//! - macOS: the login Keychain (Security framework). Items are readable only by
//!   the `lxw` binary that created them; other programs, including
//!   `security find-generic-password`, need the user's approval.
//! - Linux: the Secret Service (GNOME Keyring, KWallet, ...) via libsecret's
//!   `secret-tool`; secrets travel over stdin/stdout, never in process arguments.
//! - Windows: the Credential Manager (generic credentials named
//!   `lxw-cli:profile:<name>`), encrypted with the user's login.
//! - Elsewhere, or without a running keyring: unavailable. Use `LXW_API_KEY`
//!   or opt into the plaintext file store explicitly (`auth login --store file`).
//!
//! One item per profile holds all of its secrets as JSON, so a command reads
//! the store at most once.

use crate::error::{CliError, Kind};
use serde::{Deserialize, Serialize};

const SERVICE: &str = "lxw-cli";

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Secrets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

/// Human-readable name of the platform store, e.g. for messages.
pub fn store_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macOS Keychain"
    } else if cfg!(target_os = "linux") {
        "Secret Service (via secret-tool)"
    } else if cfg!(windows) {
        "Windows Credential Manager"
    } else {
        "OS credential store"
    }
}

/// Whether `LXW_CREDENTIAL_STORE=file` disables the OS store (headless boxes, tests).
pub fn disabled_by_env() -> bool {
    std::env::var("LXW_CREDENTIAL_STORE").is_ok_and(|v| v.eq_ignore_ascii_case("file"))
}

/// The profile's secrets, or `None` when the store has no item for it.
pub fn read(profile: &str) -> Result<Option<Secrets>, CliError> {
    let Some(raw) = backend::read(&account(profile))? else {
        return Ok(None);
    };
    serde_json::from_slice(&raw)
        .map(Some)
        .map_err(|e| unavailable(format!("unreadable {} entry for profile {profile}: {e}", store_name())))
}

pub fn write(profile: &str, secrets: &Secrets) -> Result<(), CliError> {
    let raw = serde_json::to_vec(secrets).map_err(|e| CliError::internal(e.to_string()))?;
    backend::write(&account(profile), &raw)
}

pub fn delete(profile: &str) -> Result<(), CliError> {
    backend::delete(&account(profile))
}

/// Checks that the store can be used, without touching any existing item.
pub fn check_available() -> Result<(), CliError> {
    // Looking up an item that never exists is harmless on every backend.
    backend::read("lxw-cli-availability-probe").map(|_| ())
}

fn account(profile: &str) -> String {
    format!("profile:{profile}")
}

fn unavailable(message: impl Into<String>) -> CliError {
    CliError::new(Kind::AuthMissing, message).with_hint(
        "Use LXW_API_KEY / LXW_API_KEY_FILE, or store credentials in the config file \
         instead with `lxw auth login --store file` (plaintext, mode 0600).",
    )
}

#[cfg(target_os = "macos")]
mod backend {
    use super::{SERVICE, unavailable};
    use crate::error::{CliError, Kind};
    use security_framework::os::macos::keychain::SecKeychain;
    use security_framework::passwords;
    use std::io::IsTerminal;

    const ITEM_NOT_FOUND: i32 = -25300;
    const INTERACTION_NOT_ALLOWED: i32 = -25308;
    const USER_CANCELED: i32 = -128;
    const AUTH_FAILED: i32 = -25293;

    fn interactive() -> bool {
        std::io::stderr().is_terminal()
    }

    /// Without a terminal nobody may be there to answer macOS's approval
    /// dialog, so fail fast instead of blocking an agent indefinitely.
    fn guard() -> Option<security_framework::os::macos::keychain::KeychainUserInteractionLock> {
        if interactive() {
            None
        } else {
            SecKeychain::disable_user_interaction().ok()
        }
    }

    fn map(e: security_framework::base::Error) -> CliError {
        match e.code() {
            // With dialogs disabled, macOS reports an untrusted binary as "auth failed".
            INTERACTION_NOT_ALLOWED | AUTH_FAILED if !interactive() => CliError::new(
                Kind::AuthMissing,
                "this lxw binary needs your one-time approval to read its credentials from the macOS Keychain",
            )
            .with_hint(
                "This happens once after the lxw binary changes (update or rebuild). Run `lxw auth status` \
                 in a terminal and click \"Always Allow\" in the Keychain dialog.",
            ),
            USER_CANCELED | AUTH_FAILED => CliError::new(Kind::AuthMissing, "access to the macOS Keychain was denied")
                .with_hint(
                    "Run `lxw auth status` again and click \"Always Allow\", or log in again with `lxw auth login`.",
                ),
            code => unavailable(format!("macOS Keychain error {code}: {e}")),
        }
    }

    pub fn read(account: &str) -> Result<Option<Vec<u8>>, CliError> {
        let _guard = guard();
        match passwords::get_generic_password(SERVICE, account) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.code() == ITEM_NOT_FOUND => Ok(None),
            Err(e) => Err(map(e)),
        }
    }

    pub fn write(account: &str, secret: &[u8]) -> Result<(), CliError> {
        let _guard = guard();
        passwords::set_generic_password(SERVICE, account, secret).map_err(map)
    }

    pub fn delete(account: &str) -> Result<(), CliError> {
        let _guard = guard();
        match passwords::delete_generic_password(SERVICE, account) {
            Err(e) if e.code() != ITEM_NOT_FOUND => Err(map(e)),
            _ => Ok(()),
        }
    }
}

#[cfg(target_os = "linux")]
mod backend {
    use super::{SERVICE, unavailable};
    use crate::error::CliError;
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// Runs `secret-tool` with `input` on stdin; returns (success, stdout, stderr).
    fn secret_tool(args: &[&str], input: Option<&[u8]>) -> Result<(bool, Vec<u8>, String), CliError> {
        let mut child = Command::new("secret-tool")
            .args(args)
            .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| unavailable(format!("cannot run secret-tool (install libsecret-tools): {e}")))?;
        if let (Some(data), Some(mut stdin)) = (input, child.stdin.take()) {
            stdin
                .write_all(data)
                .map_err(|e| unavailable(format!("secret-tool: {e}")))?;
        }
        let out = child
            .wait_with_output()
            .map_err(|e| unavailable(format!("secret-tool: {e}")))?;
        Ok((
            out.status.success(),
            out.stdout,
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }

    pub fn read(account: &str) -> Result<Option<Vec<u8>>, CliError> {
        let (ok, out, err) = secret_tool(&["lookup", "service", SERVICE, "account", account], None)?;
        match (ok, err.is_empty()) {
            (true, _) => Ok(Some(out)),
            // "Not found" exits 1 without a message; anything else is a store problem.
            (false, true) => Ok(None),
            (false, false) => Err(unavailable(format!("Secret Service unavailable: {err}"))),
        }
    }

    pub fn write(account: &str, secret: &[u8]) -> Result<(), CliError> {
        let label = format!("{SERVICE} ({account})");
        let (ok, _, err) = secret_tool(
            &["store", "--label", &label, "service", SERVICE, "account", account],
            Some(secret),
        )?;
        if ok {
            Ok(())
        } else {
            Err(unavailable(format!("cannot store secret: {err}")))
        }
    }

    pub fn delete(account: &str) -> Result<(), CliError> {
        let (ok, _, err) = secret_tool(&["clear", "service", SERVICE, "account", account], None)?;
        if ok || err.is_empty() {
            Ok(())
        } else {
            Err(unavailable(format!("cannot delete secret: {err}")))
        }
    }
}

#[cfg(windows)]
mod backend {
    use super::{SERVICE, unavailable};
    use crate::error::CliError;
    use std::io::Error;
    use windows_sys::Win32::Foundation::ERROR_NOT_FOUND;
    use windows_sys::Win32::Security::Credentials::{
        CRED_MAX_CREDENTIAL_BLOB_SIZE, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW,
        CredFree, CredReadW, CredWriteW,
    };

    /// A credential holds at most 2560 bytes, less than OAuth tokens may need,
    /// so longer secrets continue in `<target>#2`, `<target>#3`, ...
    const CHUNK: usize = CRED_MAX_CREDENTIAL_BLOB_SIZE as usize;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    fn target(account: &str, part: usize) -> Vec<u16> {
        match part {
            0 => wide(&format!("{SERVICE}:{account}")),
            n => wide(&format!("{SERVICE}:{account}#{}", n + 1)),
        }
    }

    /// The error of the call that just failed, or `None` if the entry does not exist.
    fn last_error() -> Option<Error> {
        let e = Error::last_os_error();
        (e.raw_os_error() != Some(ERROR_NOT_FOUND as i32)).then_some(e)
    }

    fn failed(what: &str, e: Error) -> CliError {
        unavailable(format!("Windows Credential Manager: cannot {what}: {e}"))
    }

    fn read_part(account: &str, part: usize) -> Result<Option<Vec<u8>>, CliError> {
        let name = target(account, part);
        let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: `name` is NUL-terminated; on success `cred` is valid until CredFree.
        unsafe {
            if CredReadW(name.as_ptr(), CRED_TYPE_GENERIC, 0, &mut cred) == 0 {
                return last_error().map_or(Ok(None), |e| Err(failed("read secret", e)));
            }
            let c = &*cred;
            let blob = if c.CredentialBlob.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec()
            };
            CredFree(cred.cast());
            Ok(Some(blob))
        }
    }

    fn write_part(account: &str, part: usize, data: &[u8]) -> Result<(), CliError> {
        let mut name = target(account, part);
        let mut user = wide(account);
        let cred = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: name.as_mut_ptr(),
            UserName: user.as_mut_ptr(),
            CredentialBlobSize: data.len() as u32,
            CredentialBlob: data.as_ptr().cast_mut(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            ..Default::default()
        };
        // SAFETY: every pointer outlives the call; CredWriteW copies the data.
        if unsafe { CredWriteW(&cred, 0) } == 0 {
            return Err(failed("store secret", Error::last_os_error()));
        }
        Ok(())
    }

    /// Deletes the parts from `first` on; returns once one does not exist.
    fn delete_from(account: &str, first: usize) -> Result<(), CliError> {
        for part in first.. {
            let name = target(account, part);
            // SAFETY: `name` is NUL-terminated.
            if unsafe { CredDeleteW(name.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
                return last_error().map_or(Ok(()), |e| Err(failed("delete secret", e)));
            }
        }
        Ok(())
    }

    pub fn read(account: &str) -> Result<Option<Vec<u8>>, CliError> {
        let Some(mut secret) = read_part(account, 0)? else {
            return Ok(None);
        };
        for part in 1.. {
            match read_part(account, part)? {
                Some(more) => secret.extend(more),
                None => break,
            }
        }
        Ok(Some(secret))
    }

    pub fn write(account: &str, secret: &[u8]) -> Result<(), CliError> {
        for (part, chunk) in secret.chunks(CHUNK).enumerate() {
            write_part(account, part, chunk)?;
        }
        // Remove parts left over from a longer secret.
        delete_from(account, secret.len().div_ceil(CHUNK))
    }

    pub fn delete(account: &str) -> Result<(), CliError> {
        delete_from(account, 0)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod backend {
    use super::unavailable;
    use crate::error::CliError;

    fn none() -> CliError {
        unavailable("no OS credential store support on this platform yet")
    }

    pub fn read(_: &str) -> Result<Option<Vec<u8>>, CliError> {
        Err(none())
    }

    pub fn write(_: &str, _: &[u8]) -> Result<(), CliError> {
        Err(none())
    }

    pub fn delete(_: &str) -> Result<(), CliError> {
        Err(none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_serialize_compactly() {
        let s = Secrets {
            api_key: Some("k".into()),
            ..Default::default()
        };
        assert_eq!(serde_json::to_string(&s).unwrap(), r#"{"api_key":"k"}"#);
        assert_eq!(serde_json::to_string(&Secrets::default()).unwrap(), "{}");
    }

    /// Exercises the real OS store; run manually: `cargo test os_store_round_trip -- --ignored`.
    #[test]
    #[ignore]
    fn os_store_round_trip() {
        let profile = format!("test-{}", std::process::id());
        let small = Secrets {
            api_key: Some("round-trip".into()),
            ..Default::default()
        };
        // Larger than one Windows credential (2560 bytes).
        let large = Secrets {
            access_token: Some("t".repeat(6000)),
            ..small.clone()
        };
        for s in [&large, &small] {
            write(&profile, s).unwrap();
            assert_eq!(read(&profile).unwrap().as_ref(), Some(s));
        }
        delete(&profile).unwrap();
        assert_eq!(read(&profile).unwrap(), None);
    }
}
