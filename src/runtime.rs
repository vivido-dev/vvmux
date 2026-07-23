use std::fs;
#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ipc;
use crate::platform::{SessionEndpoint, VirtualPresenterEndpoint};

const REGISTRY_SCHEMA: u32 = 2;
const MAX_REGISTRY_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub socket: SessionEndpoint,
    pub vivid_socket: VirtualPresenterEndpoint,
    pub registry: PathBuf,
    endpoint_id: String,
    #[cfg(unix)]
    legacy_registry: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "platform", rename_all = "snake_case")]
pub enum ProcessBirth {
    Windows {
        creation_filetime: u64,
    },
    Linux {
        start_ticks: u64,
    },
    Macos {
        start_seconds: u64,
        start_microseconds: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRegistry {
    #[serde(default)]
    pub schema: u32,
    pub name: String,
    pub pid: u32,
    pub instance_nonce: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vvmx_version: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_birth: Option<ProcessBirth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<PathBuf>,
}

impl RuntimePaths {
    pub fn for_session(name: &str) -> io::Result<Self> {
        validate_session_name(name)?;
        let root = runtime_root()?;
        let registry_hash =
            hex(&domain_hash(b"vvmux registry filename v1\0", name.as_bytes())[..16]);
        #[cfg(unix)]
        let legacy_hash = hex(&Sha256::digest(name.as_bytes())[..16]);

        #[cfg(unix)]
        let socket = root.join(format!("session-{legacy_hash}.sock"));
        #[cfg(windows)]
        let socket = crate::platform::windows_pipe_name(name)?;
        #[cfg(unix)]
        let vivid_socket = root.join(format!("vivid-{legacy_hash}.sock"));
        #[cfg(windows)]
        let vivid_socket = PathBuf::new();

        #[cfg(unix)]
        let endpoint_bytes = {
            use std::os::unix::ffi::OsStrExt;
            socket.as_os_str().as_bytes()
        };
        #[cfg(windows)]
        let endpoint_bytes = socket.as_bytes();
        let endpoint_id = hex(&domain_hash(
            b"vvmux endpoint identity v1\0",
            endpoint_bytes,
        ));

        Ok(Self {
            socket,
            vivid_socket,
            registry: root.join(format!("session-{registry_hash}.json")),
            endpoint_id,
            #[cfg(unix)]
            legacy_registry: root.join(format!("session-{legacy_hash}.json")),
        })
    }

    pub fn write_registry(&self, name: &str, nonce: &[u8; 32]) -> io::Result<SessionRegistry> {
        let registry = SessionRegistry {
            schema: REGISTRY_SCHEMA,
            name: name.to_owned(),
            pid: std::process::id(),
            instance_nonce: hex(nonce),
            vvmx_version: Some(ipc::VERSION),
            endpoint_id: Some(self.endpoint_id.clone()),
            process_birth: Some(process_birth(std::process::id())?),
            #[cfg(unix)]
            socket: Some(self.socket.clone()),
            #[cfg(windows)]
            socket: None,
        };
        let bytes = serde_json::to_vec(&registry).map_err(io::Error::other)?;
        let temporary = self.registry.with_extension(format!(
            "json.{}.{}.tmp",
            std::process::id(),
            &registry.instance_nonce[..16]
        ));
        #[cfg(unix)]
        let mut file = {
            let mut options = OpenOptions::new();
            options.create_new(true).write(true).mode(0o600);
            options.open(&temporary)?
        };
        #[cfg(windows)]
        let mut file = crate::platform::create_secure_windows_registry_file(&temporary)?;
        let write_result = file.write_all(&bytes).and_then(|()| file.sync_all());
        drop(file);
        let write_result = write_result.and_then(|()| atomic_replace(&temporary, &self.registry));
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result?;
        #[cfg(windows)]
        crate::platform::validate_windows_registry_file(&self.registry)?;
        Ok(registry)
    }

    pub fn read_registry(&self) -> io::Result<SessionRegistry> {
        match read_registry_file(&self.registry) {
            Ok(registry) => Ok(registry),
            #[cfg(unix)]
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                read_registry_file(&self.legacy_registry)
            }
            Err(error) => Err(error),
        }
    }

    pub fn remove_instance(&self, expected: &SessionRegistry) {
        #[cfg(windows)]
        {
            let _ = remove_windows_registry_if_same(&self.registry, expected);
        }
        #[cfg(unix)]
        {
            let Ok(actual) = self.read_registry() else {
                return;
            };
            if !same_instance(&actual, expected) {
                return;
            }
            let _ = fs::remove_file(&self.socket);
            let _ = fs::remove_file(&self.vivid_socket);
            if self.registry.exists()
                && read_registry_file(&self.registry)
                    .is_ok_and(|registry| same_instance(&registry, expected))
            {
                let _ = fs::remove_file(&self.registry);
            }
            if self.legacy_registry.exists()
                && read_registry_file(&self.legacy_registry)
                    .is_ok_and(|registry| same_instance(&registry, expected))
            {
                let _ = fs::remove_file(&self.legacy_registry);
            }
        }
    }

    pub fn artifacts_exist(&self) -> bool {
        #[cfg(unix)]
        {
            self.socket.exists() || self.registry.exists() || self.legacy_registry.exists()
        }
        #[cfg(windows)]
        {
            self.registry.exists()
        }
    }

    #[cfg(unix)]
    pub fn remove_stale_endpoints(&self) {
        let _ = fs::remove_file(&self.socket);
        let _ = fs::remove_file(&self.vivid_socket);
    }
}

pub fn validate_session_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > 64
        || name.starts_with('.')
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session name must be 1-64 ASCII letters, digits, '.', '-' or '_' and not start '.'",
        ));
    }
    Ok(())
}

pub fn list_registries() -> io::Result<Vec<SessionRegistry>> {
    let root = runtime_root()?;
    let mut sessions = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("session-") || !name.ends_with(".json") {
            continue;
        }
        let Ok(registry) = read_registry_file(&entry.path()) else {
            continue;
        };
        if registry_process_matches(&registry) {
            sessions.push(registry);
        }
    }
    sessions.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(sessions)
}

pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(pid as i32, 0) };
        result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            false
        } else {
            unsafe { CloseHandle(handle) };
            true
        }
    }
}

pub fn registry_process_matches(registry: &SessionRegistry) -> bool {
    match registry.process_birth.as_ref() {
        Some(expected) => process_birth(registry.pid).is_ok_and(|actual| &actual == expected),
        None => process_alive(registry.pid),
    }
}

fn read_registry_file(path: &Path) -> io::Result<SessionRegistry> {
    let bytes = read_registry_bytes(path, false)?;
    decode_registry(&bytes)
}

fn decode_registry(bytes: &[u8]) -> io::Result<SessionRegistry> {
    let registry: SessionRegistry = serde_json::from_slice(bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid session registry: {error}"),
        )
    })?;
    validate_registry(&registry)?;
    Ok(registry)
}

#[cfg(unix)]
fn read_registry_bytes(path: &Path, _delete_access: bool) -> io::Result<Vec<u8>> {
    let uid = unsafe { libc::geteuid() };
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe Unix session registry",
        ));
    }
    read_bounded_registry(&mut file, metadata.len())
}

#[cfg(windows)]
fn read_registry_bytes(path: &Path, delete_access: bool) -> io::Result<Vec<u8>> {
    let mut file = crate::platform::open_windows_registry_file(path, delete_access)?;
    let length = file.metadata()?.len();
    read_bounded_registry(&mut file, length)
}

fn read_bounded_registry(reader: &mut impl Read, length: u64) -> io::Result<Vec<u8>> {
    if length > MAX_REGISTRY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session registry exceeds 16 KiB",
        ));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    reader
        .take(MAX_REGISTRY_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_REGISTRY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session registry exceeds 16 KiB",
        ));
    }
    Ok(bytes)
}

#[cfg(windows)]
fn remove_windows_registry_if_same(path: &Path, expected: &SessionRegistry) -> io::Result<()> {
    let mut file = crate::platform::open_windows_registry_file(path, true)?;
    let length = file.metadata()?.len();
    let bytes = read_bounded_registry(&mut file, length)?;
    let actual = decode_registry(&bytes)?;
    if !same_instance(&actual, expected) {
        return Ok(());
    }
    crate::platform::delete_open_windows_registry_file(&file)
}

fn validate_registry(registry: &SessionRegistry) -> io::Result<()> {
    validate_session_name(&registry.name)?;
    if registry.pid == 0 || !is_lower_hex(&registry.instance_nonce, 64) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session registry has an invalid PID or nonce",
        ));
    }
    if registry.schema == 0 {
        #[cfg(windows)]
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows requires session registry schema 2",
        ));
        #[cfg(unix)]
        return Ok(());
    }
    if registry.schema != REGISTRY_SCHEMA
        || registry.vvmx_version != Some(ipc::VERSION)
        || registry
            .endpoint_id
            .as_ref()
            .is_none_or(|value| !is_lower_hex(value, 64))
        || registry.process_birth.is_none()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session registry schema 2 fields are invalid",
        ));
    }
    Ok(())
}

fn same_instance(left: &SessionRegistry, right: &SessionRegistry) -> bool {
    left.schema == right.schema
        && left.name == right.name
        && left.pid == right.pid
        && left.instance_nonce == right.instance_nonce
        && left.endpoint_id == right.endpoint_id
        && left.process_birth == right.process_birth
}

fn process_birth(pid: u32) -> io::Result<ProcessBirth> {
    #[cfg(target_os = "linux")]
    {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let end = stat.rfind(") ").ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "malformed Linux process stat")
        })?;
        let start_ticks = stat[end + 2..]
            .split_whitespace()
            .nth(19)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "Linux process start is missing")
            })?
            .parse::<u64>()
            .map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "Linux process start is invalid")
            })?;
        Ok(ProcessBirth::Linux { start_ticks })
    }
    #[cfg(target_os = "macos")]
    {
        macos_process_birth(pid)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
        use windows_sys::Win32::System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let result =
            unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) };
        unsafe { CloseHandle(process) };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ProcessBirth::Windows {
            creation_filetime: (u64::from(creation.dwHighDateTime) << 32)
                | u64::from(creation.dwLowDateTime),
        })
    }
}

#[cfg(target_os = "macos")]
fn macos_process_birth(pid: u32) -> io::Result<ProcessBirth> {
    const PROC_PIDTBSDINFO: i32 = 3;
    #[repr(C)]
    #[derive(Default)]
    struct ProcBsdInfo {
        flags: u32,
        status: u32,
        xstatus: u32,
        pid: u32,
        ppid: u32,
        uid: u32,
        gid: u32,
        ruid: u32,
        rgid: u32,
        svuid: u32,
        svgid: u32,
        rfu_1: u32,
        comm: [u8; 16],
        name: [u8; 32],
        nfiles: u32,
        pgid: u32,
        pjobc: u32,
        e_tdev: u32,
        e_tpgid: u32,
        nice: i32,
        start_seconds: u64,
        start_microseconds: u64,
    }
    unsafe extern "C" {
        fn proc_pidinfo(
            pid: i32,
            flavor: i32,
            arg: u64,
            buffer: *mut core::ffi::c_void,
            buffer_size: i32,
        ) -> i32;
    }
    let mut info = ProcBsdInfo::default();
    let size = std::mem::size_of::<ProcBsdInfo>();
    let result = unsafe {
        proc_pidinfo(
            pid as i32,
            PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut ProcBsdInfo).cast(),
            size as i32,
        )
    };
    if result != size as i32 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ProcessBirth::Macos {
            start_seconds: info.start_seconds,
            start_microseconds: info.start_microseconds,
        })
    }
}

#[cfg(unix)]
fn runtime_root() -> io::Result<PathBuf> {
    let uid = unsafe { libc::geteuid() };
    let base = if let Some(path) = std::env::var_os("XDG_RUNTIME_DIR") {
        let path = PathBuf::from(path);
        validate_parent(&path, uid)?;
        path.join("vvmux")
    } else {
        PathBuf::from(format!("/tmp/vvmux-{uid}"))
    };
    ensure_private_directory(&base, uid)?;
    Ok(base)
}

#[cfg(windows)]
fn runtime_root() -> io::Result<PathBuf> {
    crate::platform::windows_runtime_root()
}

#[cfg(unix)]
fn validate_parent(path: &Path, uid: u32) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() || metadata.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "XDG_RUNTIME_DIR must be an owner-controlled directory",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_private_directory(path: &Path, uid: u32) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || metadata.uid() != uid
                || metadata.mode() & 0o077 != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("unsafe vvmux runtime directory {}", path.display()),
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            let metadata = fs::symlink_metadata(path)?;
            if metadata.file_type().is_symlink() || metadata.uid() != uid {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "runtime directory ownership changed during creation",
                ));
            }
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
pub(crate) fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
#[allow(dead_code)]
fn _assert_file_is_owned(file: &File, uid: u32) -> io::Result<()> {
    let metadata = file.metadata()?;
    if metadata.uid() == uid {
        Ok(())
    } else {
        Err(io::ErrorKind::PermissionDenied.into())
    }
}

fn domain_hash(domain: &[u8], value: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(domain);
    hash.update(value);
    hash.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn is_lower_hex(value: &str, expected_length: usize) -> bool {
    value.len() == expected_length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_names_cannot_be_paths() {
        for invalid in ["", "../x", "/tmp/x", ".hidden", "contains space"] {
            assert!(validate_session_name(invalid).is_err(), "{invalid}");
        }
        for valid in ["default", "work-1", "project.dev"] {
            validate_session_name(valid).unwrap();
        }
    }

    #[test]
    fn domain_hashes_do_not_alias_raw_session_hashes() {
        let registry = domain_hash(b"vvmux registry filename v1\0", b"work");
        let endpoint = domain_hash(b"vvmux endpoint identity v1\0", b"work");
        assert_ne!(registry, endpoint);
        assert_eq!(hex(&registry).len(), 64);
    }

    #[test]
    fn current_process_birth_is_stable() {
        let first = process_birth(std::process::id()).unwrap();
        let second = process_birth(std::process::id()).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn schema_two_requires_birth_version_and_endpoint_identity() {
        let mut registry = test_registry();
        validate_registry(&registry).unwrap();
        registry.process_birth = None;
        assert!(validate_registry(&registry).is_err());
        registry = test_registry();
        registry.vvmx_version = Some(ipc::VERSION + 1);
        assert!(validate_registry(&registry).is_err());
        registry = test_registry();
        registry.endpoint_id = Some("A".repeat(64));
        assert!(validate_registry(&registry).is_err());
    }

    #[test]
    fn exact_instance_comparison_rejects_pid_reuse() {
        let original = test_registry();
        let mut reused = original.clone();
        reused.process_birth = Some(match original.process_birth.clone().unwrap() {
            ProcessBirth::Windows { creation_filetime } => ProcessBirth::Windows {
                creation_filetime: creation_filetime.wrapping_add(1),
            },
            ProcessBirth::Linux { start_ticks } => ProcessBirth::Linux {
                start_ticks: start_ticks.wrapping_add(1),
            },
            ProcessBirth::Macos {
                start_seconds,
                start_microseconds,
            } => ProcessBirth::Macos {
                start_seconds,
                start_microseconds: start_microseconds.wrapping_add(1),
            },
        });
        assert!(!same_instance(&original, &reused));
    }

    fn test_registry() -> SessionRegistry {
        SessionRegistry {
            schema: REGISTRY_SCHEMA,
            name: "test".into(),
            pid: std::process::id(),
            instance_nonce: "01".repeat(32),
            vvmx_version: Some(ipc::VERSION),
            endpoint_id: Some("02".repeat(32)),
            process_birth: Some(process_birth(std::process::id()).unwrap()),
            socket: None,
        }
    }
}
