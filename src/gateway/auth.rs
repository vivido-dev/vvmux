#[cfg(unix)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

const AUTH_SCHEMA: u32 = 1;
/// Schema 2 adds scoped automation tokens beside the full-authority bearer token.
const AUTH_SCHEMA_SCOPED: u32 = 2;
/// Scoped tokens one record may hold.
const MAX_SCOPED_TOKENS: usize = 32;
const MAX_AUTH_RECORD_BYTES: u64 = 16 * 1024;
const TOKEN_DOMAIN: &[u8] = b"vvmux network authentication v1\0";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthRecord {
    schema: u32,
    token_sha256: String,
    /// Additional tokens that carry less than full authority.
    ///
    /// Separate from `token_sha256` rather than replacing it: the bearer token is what an
    /// interactive client attaches with, and narrowing it would break every existing deployment.
    /// A scoped token is a second credential for a different job — driving automation — and the
    /// whole point is that it cannot do the first job.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scoped: Vec<ScopedToken>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopedToken {
    sha256: String,
    scopes: Vec<TokenScope>,
    /// Free-text note naming who this was issued to. Never a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

/// What a token is allowed to do over the gateway.
///
/// Deliberately coarse and deliberately additive: a token either may run observations, or may also
/// run mutations, or is the full-authority bearer that can attach a terminal. Splitting further
/// would multiply the ways a deployment can be subtly wrong without making any of them safer.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "kebab-case")]
pub(crate) enum TokenScope {
    /// Run automation methods that observe. Cannot type, resize, close, or signal anything.
    AutomationRead,
    /// Run automation methods that change something, as well as observations.
    AutomationInput,
}

/// What a successful `hello` established about the caller.
#[derive(Debug, Clone, Default)]
pub(crate) struct Authorization {
    /// The full-authority bearer token, which may attach a terminal and do everything else.
    pub full: bool,
    scopes: Vec<TokenScope>,
}

impl Authorization {
    pub(crate) fn full() -> Self {
        Self {
            full: true,
            scopes: Vec::new(),
        }
    }

    /// Whether this caller may run an automation method of the given class.
    ///
    /// The class comes from the same `METHOD_CAPABILITIES` table `capabilities` advertises, so a
    /// method added without a class cannot slip past this check — it would not compile.
    pub(crate) fn allows(&self, class: crate::ipc::MethodClass) -> bool {
        if self.full {
            return true;
        }
        if class.mutating() {
            return self.scopes.contains(&TokenScope::AutomationInput);
        }
        self.scopes.contains(&TokenScope::AutomationRead)
            || self.scopes.contains(&TokenScope::AutomationInput)
    }

    /// Whether this caller may attach a terminal, which a scoped token never may.
    pub(crate) fn may_attach(&self) -> bool {
        self.full
    }
}

pub(crate) fn default_auth_path() -> io::Result<PathBuf> {
    #[cfg(unix)]
    {
        let root = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".config"))
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no configuration directory"))?;
        Ok(root.join("vvmux/server-auth.json"))
    }
    #[cfg(windows)]
    {
        Ok(crate::platform::windows_runtime_root()?.join("server-auth.json"))
    }
}

pub(crate) fn create_token(path: Option<&Path>, rotate: bool) -> io::Result<Zeroizing<String>> {
    let path = match path {
        Some(path) => path.to_owned(),
        None => default_auth_path()?,
    };
    if path.exists() && !rotate {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "authentication record {} already exists; use --rotate to replace it",
                path.display()
            ),
        ));
    }
    ensure_auth_parent(&path)?;

    let mut bytes = Zeroizing::new([0_u8; 32]);
    getrandom::fill(bytes.as_mut()).map_err(|error| {
        io::Error::other(format!("could not generate authentication token: {error}"))
    })?;
    let token = Zeroizing::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(*bytes));
    let record = AuthRecord {
        schema: AUTH_SCHEMA,
        token_sha256: hex(&token_hash(token.as_bytes())),
        scoped: Vec::new(),
    };
    write_record(&path, &record)?;
    Ok(token)
}

/// Replace the record on disk atomically, keeping its owner-only permissions.
fn write_record(path: &Path, record: &AuthRecord) -> io::Result<()> {
    let encoded = serde_json::to_vec(record).map_err(io::Error::other)?;
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut file = create_auth_file(&temporary)?;
    let result = file.write_all(&encoded).and_then(|()| file.sync_all());
    drop(file);
    let result = result.and_then(|()| crate::runtime::atomic_replace(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    validate_auth_file(path)
}

/// What the submitted token authorizes, or `None` when it authorizes nothing.
///
/// Every candidate is compared in constant time, and every candidate is compared: returning as
/// soon as one matches would leak, through timing, which token in the record was presented.
pub(crate) fn authorize(path: &Path, submitted: &str) -> io::Result<Option<Authorization>> {
    let decoded = Zeroizing::new(
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(submitted)
            .map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid authentication token")
            })?,
    );
    if decoded.len() != 32 {
        return Ok(None);
    }
    let record = read_record(path)?;
    let actual = token_hash(submitted.as_bytes());
    let mut authorization = None;
    if bool::from(actual.ct_eq(&decode_hex_32(&record.token_sha256)?)) {
        authorization = Some(Authorization::full());
    }
    for scoped in &record.scoped {
        let expected = decode_hex_32(&scoped.sha256)?;
        if bool::from(actual.ct_eq(&expected)) && authorization.is_none() {
            authorization = Some(Authorization {
                full: false,
                scopes: scoped.scopes.clone(),
            });
        }
    }
    Ok(authorization)
}

/// Add a scoped automation token to an existing record, and return it once.
pub(crate) fn create_scoped_token(
    path: Option<&Path>,
    scopes: Vec<TokenScope>,
    label: Option<String>,
) -> io::Result<Zeroizing<String>> {
    let path = match path {
        Some(path) => path.to_owned(),
        None => default_auth_path()?,
    };
    let mut record = read_record(&path)?;
    if record.scoped.len() >= MAX_SCOPED_TOKENS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("this record already holds {MAX_SCOPED_TOKENS} scoped tokens"),
        ));
    }
    if scopes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a scoped token needs at least one scope",
        ));
    }
    let mut bytes = Zeroizing::new([0_u8; 32]);
    getrandom::fill(bytes.as_mut()).map_err(|error| {
        io::Error::other(format!("could not generate authentication token: {error}"))
    })?;
    let token = Zeroizing::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(*bytes));
    record.scoped.push(ScopedToken {
        sha256: hex(&token_hash(token.as_bytes())),
        scopes,
        label,
    });
    // Written at the scoped schema so an older binary refuses the file outright rather than
    // reading it, dropping the scoped tokens it does not understand, and rewriting it without
    // them — which would silently widen every scoped credential to nothing at all.
    record.schema = AUTH_SCHEMA_SCOPED;
    write_record(&path, &record)?;
    Ok(token)
}

pub(crate) fn validate_record(path: &Path) -> io::Result<()> {
    read_record(path).map(drop)
}

fn read_record(path: &Path) -> io::Result<AuthRecord> {
    let mut file = open_auth_file(path)?;
    let length = file.metadata()?.len();
    if length > MAX_AUTH_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "authentication record exceeds 16 KiB",
        ));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    Read::by_ref(&mut file)
        .take(MAX_AUTH_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_AUTH_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "authentication record exceeds 16 KiB",
        ));
    }
    let record: AuthRecord = serde_json::from_slice(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid authentication record: {error}"),
        )
    })?;
    if record.schema != AUTH_SCHEMA && record.schema != AUTH_SCHEMA_SCOPED {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported authentication record schema",
        ));
    }
    if record.scoped.len() > MAX_SCOPED_TOKENS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "authentication record holds too many scoped tokens",
        ));
    }
    let _ = decode_hex_32(&record.token_sha256)?;
    Ok(record)
}

/// Whether the submitted token is the full-authority bearer.
#[cfg(test)]
fn authenticate(path: &Path, submitted: &str) -> io::Result<bool> {
    Ok(authorize(path, submitted)?.is_some_and(|authorization| authorization.full))
}

fn token_hash(token: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(TOKEN_DOMAIN);
    hasher.update(token);
    hasher.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn decode_hex_32(value: &str) -> io::Result<[u8; 32]> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "authentication record has an invalid token hash",
        ));
    }
    let mut output = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        output[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Ok(output)
}

fn hex_nibble(value: u8) -> io::Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "authentication record has an invalid token hash",
        )),
    }
}

#[cfg(unix)]
fn ensure_auth_parent(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "authentication record path has no parent",
        )
    })?;
    if !parent.exists() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let metadata = fs::symlink_metadata(parent)?;
    let uid = unsafe { libc::geteuid() };
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != uid
        || metadata.mode() & 0o022 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "authentication record directory must be owner-controlled",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn ensure_auth_parent(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "authentication record path has no parent",
        )
    })?;
    fs::create_dir_all(parent)
}

#[cfg(unix)]
fn create_auth_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
}

#[cfg(windows)]
fn create_auth_file(path: &Path) -> io::Result<File> {
    crate::platform::create_secure_windows_registry_file(path)
}

#[cfg(unix)]
fn open_auth_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    validate_open_unix_file(&file)?;
    Ok(file)
}

#[cfg(windows)]
fn open_auth_file(path: &Path) -> io::Result<File> {
    crate::platform::open_windows_registry_file(path, false)
}

#[cfg(unix)]
fn validate_open_unix_file(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "authentication record must be an owner-only regular file",
        ))
    } else {
        Ok(())
    }
}

fn validate_auth_file(path: &Path) -> io::Result<()> {
    open_auth_file(path).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_record_authenticates_and_rotation_invalidates_new_attempts() {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("auth.json");
        let first = create_token(Some(&path), false).unwrap();
        assert!(authenticate(&path, &first).unwrap());
        assert!(!authenticate(&path, &"A".repeat(43)).unwrap_or(false));
        assert!(create_token(Some(&path), false).is_err());

        let second = create_token(Some(&path), true).unwrap();
        assert!(!authenticate(&path, &first).unwrap());
        assert!(authenticate(&path, &second).unwrap());
        let source = fs::read_to_string(path).unwrap();
        assert!(!source.contains(first.as_str()));
        assert!(!source.contains(second.as_str()));
    }

    #[test]
    fn malformed_hash_is_rejected() {
        assert!(decode_hex_32("00").is_err());
        assert!(decode_hex_32(&"z".repeat(64)).is_err());
    }
}
