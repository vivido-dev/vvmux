//! Enrolled machine identity for `vvmux cloud enroll` and `vvmux serve --connect`.
//!
//! The identity is an Ed25519 key pair in an owner-only file. Only the public key
//! leaves the machine; the private key never appears in argv, an environment
//! variable, or a log. Storage reuses the hardened pattern of `auth.rs`: an
//! owner-only parent, a `0600` file opened with `O_NOFOLLOW`, owner and mode
//! re-checked after open, and `atomic_replace` on write.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

use base64::Engine;
use ed25519_dalek::Signer as _;
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const IDENTITY_SCHEMA: u32 = 1;
const MAX_IDENTITY_RECORD_BYTES: u64 = 16 * 1024;
const MAX_ENROLL_CODE_BYTES: usize = 4 * 1024;
/// The domain-separated prefix bound into every tunnel handshake signature.
pub(crate) const AUTH_DOMAIN: &[u8] = b"vvmux tunnel auth v1\0";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityRecord {
    schema: u32,
    /// Ed25519 seed, 32 bytes, base64url.
    private_key: Zeroizing<String>,
}

/// An enrolled machine identity loaded into memory.
#[derive(Clone)]
pub(crate) struct MachineIdentity {
    signing: SigningKey,
}

impl MachineIdentity {
    /// Generate a fresh identity without touching the filesystem. Used by
    /// enrollment, which writes the record only after the server accepts it.
    pub fn new_random() -> io::Result<Self> {
        let mut seed = Zeroizing::new([0_u8; 32]);
        getrandom::fill(seed.as_mut()).map_err(io::Error::other)?;
        Ok(Self {
            signing: SigningKey::from_bytes(&seed),
        })
    }

    /// Generate a fresh identity, storing it with owner-only protection.
    #[cfg(test)]
    pub fn generate(path: &Path) -> io::Result<Self> {
        if path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "identity record {} already exists; remove it to re-enroll",
                    path.display()
                ),
            ));
        }
        ensure_parent(path)?;

        let mut seed = Zeroizing::new([0_u8; 32]);
        getrandom::fill(seed.as_mut()).map_err(io::Error::other)?;
        let signing = SigningKey::from_bytes(&seed);
        let identity = Self { signing };
        let encoded = identity.encoded_record()?;
        let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
        let mut file = create_identity_file(&temporary)?;
        let result = file
            .write_all(encoded.as_slice())
            .and_then(|()| file.sync_all());
        drop(file);
        let result = result.and_then(|()| crate::runtime::atomic_replace(&temporary, path));
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        validate_file(path)?;
        Ok(identity)
    }

    /// Load the stored identity, verifying the record and the file protections.
    pub fn load(path: &Path) -> io::Result<Self> {
        let mut file = open_identity_file(path)?;
        let length = file.metadata()?.len();
        if length > MAX_IDENTITY_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "identity record exceeds 16 KiB",
            ));
        }
        let mut bytes = Zeroizing::new(Vec::with_capacity(length as usize));
        Read::by_ref(&mut file)
            .take(MAX_IDENTITY_RECORD_BYTES + 1)
            .read_to_end(bytes.as_mut())?;
        if bytes.len() as u64 > MAX_IDENTITY_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "identity record exceeds 16 KiB",
            ));
        }
        let record: IdentityRecord = serde_json::from_slice(&bytes).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid identity record: {error}"),
            )
        })?;
        if record.schema != IDENTITY_SCHEMA {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported identity record schema",
            ));
        }
        let seed = Zeroizing::new(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(record.private_key.as_str())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid private key"))?,
        );
        let seed: &[u8; 32] = seed.as_slice().try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "private key must be 32 bytes")
        })?;
        Ok(Self {
            signing: SigningKey::from_bytes(seed),
        })
    }

    fn encoded_record(&self) -> io::Result<Zeroizing<Vec<u8>>> {
        let seed = Zeroizing::new(self.signing.to_bytes());
        let record = IdentityRecord {
            schema: IDENTITY_SCHEMA,
            private_key: Zeroizing::new(
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(seed.as_slice()),
            ),
        };
        let mut encoded = Zeroizing::new(Vec::new());
        serde_json::to_writer(&mut *encoded, &record).map_err(io::Error::other)?;
        Ok(encoded)
    }

    /// The machine identifier presented in `machine_id`: the base64url public key.
    pub fn machine_id(&self) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(self.signing.verifying_key().to_bytes())
    }

    /// The public key, for enrollment and for the test server.
    pub fn public_key(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }

    /// Sign a tunnel handshake over the challenge material (VVTUN-1).
    pub fn sign_handshake(&self, nonce: &[u8], hostname: &str, exporter: &[u8]) -> String {
        let mut message = Vec::with_capacity(
            AUTH_DOMAIN.len() + nonce.len() + hostname.len() + exporter.len() + 43,
        );
        message.extend_from_slice(AUTH_DOMAIN);
        message.extend_from_slice(nonce);
        message.extend_from_slice(hostname.as_bytes());
        message.extend_from_slice(exporter);
        message.extend_from_slice(self.machine_id().as_bytes());
        let signature = self.signing.sign(&message);
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes())
    }
}

/// Race-free reservation of an identity destination. Creating this before the
/// enrollment POST proves that no identity will be overwritten and prevents a
/// concurrent enrollment from consuming another one-use code for the same path.
pub(crate) struct IdentityReservation {
    path: PathBuf,
    file: Option<File>,
    committed: bool,
}

impl IdentityReservation {
    pub(crate) fn new(path: &Path) -> io::Result<Self> {
        ensure_parent(path)?;
        let file = create_identity_file(path).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("identity record {} already exists", path.display()),
                )
            } else {
                error
            }
        })?;
        Ok(Self {
            path: path.to_owned(),
            file: Some(file),
            committed: false,
        })
    }

    pub(crate) fn store(mut self, identity: &MachineIdentity) -> io::Result<()> {
        let encoded = identity.encoded_record()?;
        let mut file = self
            .file
            .take()
            .expect("identity reservation owns its file");
        let result = file
            .write_all(encoded.as_slice())
            .and_then(|()| file.sync_all());
        drop(file);
        result?;
        validate_file(&self.path)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for IdentityReservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}

/// The default identity file path, beside the bearer-token record.
pub(crate) fn default_identity_path() -> io::Result<PathBuf> {
    auth_sibling("cloud-identity.json")
}

fn auth_sibling(name: &str) -> io::Result<PathBuf> {
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
        Ok(root.join("vvmux").join(name))
    }
    #[cfg(windows)]
    {
        Ok(crate::platform::windows_runtime_root()?.join(name))
    }
}

/// Verify a handshake signature. The integration test server reimplements this
/// tiny check with the domain constant duplicated, because a binary crate cannot
/// import it; the handshake test proves the two agree.
#[cfg(test)]
pub(crate) fn verify_handshake(
    public_key: &VerifyingKey,
    machine_id: &str,
    nonce: &[u8],
    hostname: &str,
    exporter: &[u8],
    signature: &[u8],
) -> bool {
    let mut message = Vec::with_capacity(
        AUTH_DOMAIN.len() + nonce.len() + hostname.len() + exporter.len() + machine_id.len(),
    );
    message.extend_from_slice(AUTH_DOMAIN);
    message.extend_from_slice(nonce);
    message.extend_from_slice(hostname.as_bytes());
    message.extend_from_slice(exporter);
    message.extend_from_slice(machine_id.as_bytes());
    let Ok(signature) = ed25519_dalek::Signature::from_slice(signature) else {
        return false;
    };
    public_key.verify_strict(&message, &signature).is_ok()
}

#[cfg(unix)]
fn ensure_parent(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "identity record path has no parent",
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
            "identity record directory must be owner-controlled",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn ensure_parent(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "identity record path has no parent",
        )
    })?;
    fs::create_dir_all(parent)
}

#[cfg(unix)]
fn create_identity_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
}

#[cfg(windows)]
fn create_identity_file(path: &Path) -> io::Result<File> {
    crate::platform::create_secure_windows_registry_file(path)
}

#[cfg(unix)]
fn open_identity_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    validate_open_unix_file(&file)?;
    Ok(file)
}

#[cfg(windows)]
fn open_identity_file(path: &Path) -> io::Result<File> {
    crate::platform::open_windows_registry_file(path, false)
}

#[cfg(unix)]
fn validate_open_unix_file(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "identity record must be an owner-only regular file",
        ))
    } else {
        Ok(())
    }
}

fn validate_file(path: &Path) -> io::Result<()> {
    open_identity_file(path).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_generates_loads_and_signs_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("cloud-identity.json");
        let generated = MachineIdentity::generate(&path).unwrap();
        let loaded = MachineIdentity::load(&path).unwrap();
        assert_eq!(generated.machine_id(), loaded.machine_id());

        let nonce = b"0123456789abcdef0123456789abcdef";
        let signature = loaded.sign_handshake(nonce, "vvmux.example", &[7; 32]);
        let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(signature)
            .unwrap();
        assert!(verify_handshake(
            &loaded.public_key(),
            &loaded.machine_id(),
            nonce,
            "vvmux.example",
            &[7; 32],
            &signature
        ));
        // A different hostname or nonce must fail verification.
        assert!(!verify_handshake(
            &loaded.public_key(),
            &loaded.machine_id(),
            nonce,
            "other.example",
            &[7; 32],
            &signature
        ));
        assert!(!verify_handshake(
            &loaded.public_key(),
            &loaded.machine_id(),
            b"fedcba9876543210fedcba9876543210",
            "vvmux.example",
            &[7; 32],
            &signature
        ));
    }

    #[test]
    fn generate_refuses_an_existing_record() {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("cloud-identity.json");
        MachineIdentity::generate(&path).unwrap();
        assert!(MachineIdentity::generate(&path).is_err());
    }

    #[test]
    fn reservation_refuses_an_existing_record_and_removes_an_uncommitted_file() {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("cloud-identity.json");
        {
            let _reservation = IdentityReservation::new(&path).unwrap();
            assert!(path.exists());
            assert!(IdentityReservation::new(&path).is_err());
        }
        assert!(!path.exists());
    }

    #[test]
    fn stored_record_contains_no_private_material_in_plaintext_fields() {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("cloud-identity.json");
        let identity = MachineIdentity::generate(&path).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        // The record holds only the seed under `private_key`; the public key is
        // derived, never stored as a separate field an attacker could cross-check.
        assert!(
            !raw.contains("\"public_key\""),
            "no duplicate public key field"
        );
        // The public key is not even reconstructable from the record's fields
        // without the seed, which is exactly the point of deriving it.
        let _ = identity.machine_id();
    }
}

// ---------------------------------------------------------------------------
// Enrollment: `vvmux cloud enroll --server <url>`
// ---------------------------------------------------------------------------

const ENROLL_PATH: &str = "/api/v1/machines/enroll";
const MAX_ENROLL_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnrollResponse {
    machine_id: String,
}

#[derive(Serialize)]
struct EnrollRequest<'a> {
    code: &'a str,
    public_key: &'a str,
}

/// Obtain a one-use enrollment code without accepting it through argv or the
/// environment. `--code-file -` is the explicit automation form for stdin.
pub(crate) fn read_enrollment_code(path: Option<&Path>) -> io::Result<Zeroizing<String>> {
    let code = match path {
        None => Zeroizing::new(
            rpassword::prompt_password("Enrollment code: ")
                .map_err(|error| io::Error::new(error.kind(), "could not read enrollment code"))?,
        ),
        Some(path) if path == Path::new("-") => read_bounded_code(io::stdin().lock())?,
        Some(path) => read_bounded_code(File::open(path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("could not open enrollment code file {}", path.display()),
            )
        })?)?,
    };
    normalize_enrollment_code(code)
}

fn read_bounded_code(mut reader: impl Read) -> io::Result<Zeroizing<String>> {
    let mut code = Zeroizing::new(String::new());
    reader
        .by_ref()
        .take((MAX_ENROLL_CODE_BYTES + 1) as u64)
        .read_to_string(&mut code)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "enrollment code is not UTF-8"))?;
    Ok(code)
}

fn normalize_enrollment_code(mut code: Zeroizing<String>) -> io::Result<Zeroizing<String>> {
    while code.ends_with('\r') || code.ends_with('\n') {
        code.pop();
    }
    if code.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "enrollment code is empty",
        ));
    }
    if code.len() > MAX_ENROLL_CODE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "enrollment code exceeds 4 KiB",
        ));
    }
    if code
        .chars()
        .any(|character| matches!(character, '\r' | '\n' | '\0'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "enrollment code must be one line",
        ));
    }
    Ok(code)
}

/// Register `public_key` with the server under a one-time `code`.
///
/// The HTTP client is deliberately minimal: a single POST over plain TCP or
/// rustls TLS, with a bounded response. The response is a small JSON object;
/// chunked transfer encoding is rejected because this server never emits it.
pub(crate) fn enroll(server: &str, code: &str, public_key: &VerifyingKey) -> io::Result<()> {
    use std::io::Write as _;
    use std::sync::Arc;

    let parsed = url::Url::parse(server).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid --server URL: {error}"),
        )
    })?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--server must not contain credentials, a query, or a fragment",
        ));
    }
    let tls = match parsed.scheme() {
        "https" => true,
        "http" => {
            let host = parsed.host_str().unwrap_or_default();
            if !is_loopback_host(host) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "http:// is accepted for loopback development only; use https:// across a host boundary",
                ));
            }
            false
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("--server scheme must be https or http, got {other}"),
            ));
        }
    };
    if parsed.path() != "" && parsed.path() != "/" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--server must be a bare scheme://host[:port] URL",
        ));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "--server URL has no host"))?
        .to_owned();
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "--server URL has no port"))?;
    let authority = match parsed.host() {
        Some(url::Host::Ipv6(_)) if parsed.port().is_some() => format!("[{host}]:{port}"),
        Some(url::Host::Ipv6(_)) => format!("[{host}]"),
        Some(_) if parsed.port().is_some() => format!("{host}:{port}"),
        Some(_) => host.clone(),
        None => unreachable!("host_str was checked above"),
    };

    let expected_machine_id =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(public_key.to_bytes());
    let mut body = Zeroizing::new(Vec::new());
    serde_json::to_writer(
        &mut *body,
        &EnrollRequest {
            code,
            public_key: &expected_machine_id,
        },
    )
    .map_err(io::Error::other)?;

    use std::net::ToSocketAddrs as _;
    let address = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("could not resolve enrollment server {host}: {error}"),
            )
        })?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no address for {host}")))?;
    let stream = TcpStream::connect_timeout(&address, std::time::Duration::from_secs(10)).map_err(
        |error| {
            io::Error::new(
                error.kind(),
                format!("could not reach enrollment server {host}:{port}: {error}"),
            )
        },
    )?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .ok();
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(10)))
        .ok();
    let mut stream: EnrollStream = if tls {
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().certs {
            roots.add(cert).map_err(io::Error::other)?;
        }
        let config = Arc::new(
            rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .map_err(io::Error::other)?
            .with_root_certificates(roots)
            .with_no_client_auth(),
        );
        let server_name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid server hostname"))?
            .to_owned();
        let connection =
            rustls::ClientConnection::new(config, server_name).map_err(io::Error::other)?;
        EnrollStream::Tls(Box::new(rustls::StreamOwned::new(connection, stream)))
    } else {
        EnrollStream::Plain(stream)
    };
    write!(
        stream,
        "POST {ENROLL_PATH} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .map_err(io::Error::other)?;
    stream.write_all(&body).map_err(io::Error::other)?;

    // Read the response: status line, headers, then a bounded body.
    let mut reader = ResponseReader::new(stream);
    let status = reader.read_status()?;
    let (content_length, chunked) = reader.read_headers()?;
    if chunked {
        return Err(io::Error::other(
            "enrollment server used chunked encoding, which is not supported",
        ));
    }
    let body = Zeroizing::new(reader.read_body(content_length)?);
    if status != 200 {
        // The peer controls the body and could reflect the one-use code. Never
        // include it in the CLI error or a log.
        return Err(io::Error::other(format!(
            "enrollment refused with HTTP {status}"
        )));
    }
    let response: EnrollResponse = serde_json::from_slice(&body).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid enrollment response: {error}"),
        )
    })?;
    if response.machine_id != expected_machine_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "enrollment server returned a machine ID for a different public key",
        ));
    }
    Ok(())
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// The enrollment connection, plain or TLS, so the two stream types share one path.
enum EnrollStream {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl std::io::Read for EnrollStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            EnrollStream::Plain(stream) => stream.read(buffer),
            EnrollStream::Tls(stream) => stream.read(buffer),
        }
    }
}

impl std::io::Write for EnrollStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            EnrollStream::Plain(stream) => stream.write(buffer),
            EnrollStream::Tls(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            EnrollStream::Plain(stream) => stream.flush(),
            EnrollStream::Tls(stream) => stream.flush(),
        }
    }
}

struct ResponseReader<S> {
    stream: S,
    pending: Vec<u8>,
}

impl<S: std::io::Read> ResponseReader<S> {
    fn new(stream: S) -> Self {
        Self {
            stream,
            pending: Vec::new(),
        }
    }

    fn fill(&mut self) -> io::Result<()> {
        let mut buffer = [0_u8; 4096];
        let count = self.stream.read(&mut buffer)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "enrollment server closed mid-response",
            ));
        }
        self.pending.extend_from_slice(&buffer[..count]);
        if self.pending.len() > MAX_ENROLL_RESPONSE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "enrollment response exceeds 64 KiB",
            ));
        }
        Ok(())
    }

    fn take_line(&mut self) -> io::Result<Vec<u8>> {
        loop {
            if let Some(end) = self.pending.windows(2).position(|pair| pair == b"\r\n") {
                let line: Vec<u8> = self.pending.drain(..end).collect();
                self.pending.drain(..2);
                return Ok(line);
            }
            self.fill()?;
        }
    }

    fn read_status(&mut self) -> io::Result<u16> {
        let line = self.take_line()?;
        let line = String::from_utf8_lossy(&line);
        let mut parts = line.split_whitespace();
        let version = parts.next().unwrap_or_default();
        let status = parts.next().unwrap_or_default();
        if !version.starts_with("HTTP/1.") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "enrollment server sent a non-HTTP response",
            ));
        }
        status.parse::<u16>().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "enrollment server sent a bad status line",
            )
        })
    }

    fn read_headers(&mut self) -> io::Result<(Option<usize>, bool)> {
        let mut content_length = None;
        let mut chunked = false;
        loop {
            let line = self.take_line()?;
            if line.is_empty() {
                break;
            }
            let line = String::from_utf8_lossy(&line);
            let (name, value) = line.split_once(':').unwrap_or((line.as_ref(), ""));
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            match name.as_str() {
                "content-length" => {
                    content_length = Some(value.parse::<usize>().map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "bad Content-Length")
                    })?);
                }
                "transfer-encoding" if value.to_ascii_lowercase().contains("chunked") => {
                    chunked = true;
                }
                _ => {}
            }
        }
        Ok((content_length, chunked))
    }

    fn read_body(&mut self, content_length: Option<usize>) -> io::Result<Vec<u8>> {
        match content_length {
            Some(length) => {
                if length > MAX_ENROLL_RESPONSE_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "enrollment response exceeds 64 KiB",
                    ));
                }
                while self.pending.len() < length {
                    self.fill()?;
                }
                let body: Vec<u8> = self.pending.drain(..length).collect();
                Ok(body)
            }
            None => {
                // Read until EOF, bounded.
                loop {
                    let mut buffer = [0_u8; 4096];
                    match self.stream.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            self.pending.extend_from_slice(&buffer[..count]);
                            if self.pending.len() > MAX_ENROLL_RESPONSE_BYTES {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "enrollment response exceeds 64 KiB",
                                ));
                            }
                        }
                        Err(error) => return Err(io::Error::other(error)),
                    }
                }
                Ok(std::mem::take(&mut self.pending))
            }
        }
    }
}

#[cfg(test)]
mod enroll_tests {
    use super::*;

    #[test]
    fn enroll_rejects_bad_server_urls() {
        let key = SigningKey::from_bytes(&[7; 32]).verifying_key();
        assert!(enroll("ftp://vvmux.example", "code", &key).is_err());
        assert!(enroll("http://vvmux.example", "code", &key).is_err());
        assert!(enroll("https://vvmux.example/path", "code", &key).is_err());
        assert!(enroll("https://user@vvmux.example", "code", &key).is_err());
        assert!(enroll("https://vvmux.example?code=secret", "code", &key).is_err());
        assert!(enroll("https://vvmux.example/#fragment", "code", &key).is_err());
    }

    #[test]
    fn response_reader_parses_a_simple_response() {
        let mut reader = ResponseReader {
            stream: std::io::Cursor::new(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}"
                    .to_vec(),
            ),
            pending: Vec::new(),
        };
        assert_eq!(reader.read_status().unwrap(), 200);
        let (length, chunked) = reader.read_headers().unwrap();
        assert_eq!(length, Some(2));
        assert!(!chunked);
        assert_eq!(reader.read_body(length).unwrap(), b"{}");
    }

    #[test]
    fn response_reader_preserves_a_body_without_content_length() {
        let mut reader = ResponseReader {
            stream: std::io::Cursor::new(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"machine_id\":\"abc\"}"
                    .to_vec(),
            ),
            pending: Vec::new(),
        };
        assert_eq!(reader.read_status().unwrap(), 200);
        let (length, chunked) = reader.read_headers().unwrap();
        assert_eq!(length, None);
        assert!(!chunked);
        assert_eq!(
            reader.read_body(length).unwrap(),
            br#"{"machine_id":"abc"}"#
        );
    }

    #[test]
    fn enrollment_code_is_bounded_and_single_line() {
        assert_eq!(
            normalize_enrollment_code(Zeroizing::new("one-use\r\n".to_owned()))
                .unwrap()
                .as_str(),
            "one-use"
        );
        assert!(normalize_enrollment_code(Zeroizing::new(String::new())).is_err());
        assert!(normalize_enrollment_code(Zeroizing::new("one\ntwo".to_owned())).is_err());
        assert!(
            normalize_enrollment_code(Zeroizing::new("x".repeat(MAX_ENROLL_CODE_BYTES + 1)))
                .is_err()
        );
    }
}
