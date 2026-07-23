use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString, c_void};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{
    CloseHandle, E_INVALIDARG, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Console::{COORD, HPCON};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
    STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows_sys::core::HRESULT;

use super::{PtyParts, input};

type CreatePseudoConsoleFn =
    unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> HRESULT;
type ResizePseudoConsoleFn = unsafe extern "system" fn(HPCON, COORD) -> HRESULT;
type ClosePseudoConsoleFn = unsafe extern "system" fn(HPCON);

#[derive(Clone, Copy)]
struct ConptyApi {
    create: CreatePseudoConsoleFn,
    resize: ResizePseudoConsoleFn,
    close: ClosePseudoConsoleFn,
}

impl ConptyApi {
    fn load() -> io::Result<Self> {
        type LoadedFn = unsafe extern "system" fn() -> isize;
        let kernel32 = unsafe { GetModuleHandleW(wide(OsStr::new("kernel32.dll"))?.as_ptr()) };
        if kernel32.is_null() {
            return Err(unsupported());
        }
        let create = unsafe { GetProcAddress(kernel32, c"CreatePseudoConsole".as_ptr().cast()) }
            .ok_or_else(unsupported)?;
        let resize = unsafe { GetProcAddress(kernel32, c"ResizePseudoConsole".as_ptr().cast()) }
            .ok_or_else(unsupported)?;
        let close = unsafe { GetProcAddress(kernel32, c"ClosePseudoConsole".as_ptr().cast()) }
            .ok_or_else(unsupported)?;
        Ok(Self {
            create: unsafe { std::mem::transmute::<LoadedFn, CreatePseudoConsoleFn>(create) },
            resize: unsafe { std::mem::transmute::<LoadedFn, ResizePseudoConsoleFn>(resize) },
            close: unsafe { std::mem::transmute::<LoadedFn, ClosePseudoConsoleFn>(close) },
        })
    }
}

#[derive(Clone)]
pub struct PtyControl {
    inner: Arc<ControlInner>,
}

pub struct PtyWaiter {
    process: OwnedHandle,
}

#[derive(Debug, Clone, Copy)]
pub struct PtyExitStatus {
    pub code: Option<i64>,
    pub signal: Option<i32>,
    pub success: bool,
}

struct ControlInner {
    api: ConptyApi,
    pseudoconsole: Mutex<Option<HPCON>>,
    process: OwnedHandle,
    job: OwnedHandle,
    closing: AtomicBool,
}

unsafe impl Send for ControlInner {}
unsafe impl Sync for ControlInner {}

impl PtyControl {
    pub fn resize(&self, columns: u16, rows: u16) -> io::Result<()> {
        if columns == 0 || rows == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ConPTY dimensions must be nonzero",
            ));
        }
        if self.inner.closing.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ConPTY is closing",
            ));
        }
        let guard = self
            .inner
            .pseudoconsole
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(handle) = *guard else {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ConPTY is closed",
            ));
        };
        let result = unsafe {
            (self.inner.api.resize)(
                handle,
                COORD {
                    X: columns as i16,
                    Y: rows as i16,
                },
            )
        };
        if result >= 0 || (result == E_INVALIDARG && self.inner.closing.load(Ordering::Acquire)) {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "ResizePseudoConsole failed with HRESULT 0x{:08x}",
                result as u32
            )))
        }
    }

    pub fn terminate(&self) {
        if self.inner.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        let inner = self.inner.clone();
        let _ = std::thread::Builder::new()
            .name("vvmux-conpty-cleanup".into())
            .spawn(move || cleanup(inner));
    }

    pub fn terminate_blocking(&self) {
        self.terminate();
        let result = unsafe { WaitForSingleObject(self.inner.process.raw(), 500) };
        if result == WAIT_TIMEOUT {
            unsafe {
                TerminateJobObject(self.inner.job.raw(), 1);
            }
            let _ = unsafe { WaitForSingleObject(self.inner.process.raw(), 2_000) };
        }
    }
}

impl PtyWaiter {
    pub fn wait(self) -> io::Result<PtyExitStatus> {
        if unsafe { WaitForSingleObject(self.process.raw(), u32::MAX) } == WAIT_OBJECT_0 {
            let mut code = 0;
            if unsafe { GetExitCodeProcess(self.process.raw(), &mut code) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(PtyExitStatus {
                code: Some(i64::from(code)),
                signal: None,
                success: code == 0,
            })
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

pub(super) fn spawn(
    shell: &OsStr,
    cwd: &Path,
    columns: u16,
    rows: u16,
    environment: &[(String, String)],
) -> io::Result<PtyParts> {
    if columns == 0 || rows == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ConPTY dimensions must be nonzero",
        ));
    }
    if !cwd.is_absolute() || !Path::new(shell).is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows shell and working directory paths must be absolute",
        ));
    }

    let api = ConptyApi::load()?;
    let (output_reader, output_child) = anonymous_pipe()?;
    let (input_child, input_writer) = anonymous_pipe()?;
    set_inheritable(output_reader.raw(), false)?;
    set_inheritable(input_writer.raw(), false)?;

    let mut pseudoconsole = 0;
    let result = unsafe {
        (api.create)(
            COORD {
                X: columns as i16,
                Y: rows as i16,
            },
            input_child.raw(),
            output_child.raw(),
            0,
            &mut pseudoconsole,
        )
    };
    if result < 0 || pseudoconsole == 0 {
        return Err(if result == E_INVALIDARG {
            unsupported()
        } else {
            io::Error::other(format!(
                "CreatePseudoConsole failed with HRESULT 0x{:08x}",
                result as u32
            ))
        });
    }
    let mut pseudoconsole_guard = PseudoconsoleGuard {
        api,
        handle: Some(pseudoconsole),
    };
    drop(input_child);
    drop(output_child);

    let mut attributes = AttributeList::new(1)?;
    attributes.set_pseudoconsole(pseudoconsole)?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.lpAttributeList = attributes.pointer();

    let application = wide(shell)?;
    let mut command_line = wide(OsStr::new(&quote_argument(&shell.to_string_lossy())))?;
    let cwd = wide(cwd.as_os_str())?;
    let environment = environment_block(environment)?;

    let job = create_kill_job()?;
    let mut process_info = PROCESS_INFORMATION::default();
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            // No CREATE_NEW_PROCESS_GROUP: it starts the pane tree with Ctrl+C disabled
            // (an implicit SetConsoleCtrlHandler(NULL, TRUE) inherited by every child), so
            // the 0x03 the multiplexer forwards would never interrupt pane processes.
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
            environment.as_ptr().cast(),
            cwd.as_ptr(),
            &startup.StartupInfo,
            &mut process_info,
        )
    };
    if created == 0 {
        return Err(io::Error::last_os_error());
    }
    let process = OwnedHandle::new(process_info.hProcess)?;
    let thread = OwnedHandle::new(process_info.hThread)?;

    if unsafe { AssignProcessToJobObject(job.raw(), process.raw()) } == 0 {
        unsafe {
            TerminateProcess(process.raw(), 1);
        }
        return Err(io::Error::last_os_error());
    }
    if unsafe { ResumeThread(thread.raw()) } == u32::MAX {
        unsafe {
            TerminateJobObject(job.raw(), 1);
        }
        return Err(io::Error::last_os_error());
    }
    drop(thread);

    let waiter = PtyWaiter {
        process: process.duplicate()?,
    };
    let control = PtyControl {
        inner: Arc::new(ControlInner {
            api,
            pseudoconsole: Mutex::new(pseudoconsole_guard.handle.take()),
            process,
            job,
            closing: AtomicBool::new(false),
        }),
    };
    let reader = output_reader.into_file();
    let writer = input_writer.into_file();
    Ok(PtyParts {
        reader,
        input: input(writer)?,
        control,
        waiter,
    })
}

fn cleanup(inner: Arc<ControlInner>) {
    if let Some(handle) = inner
        .pseudoconsole
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    {
        let api = inner.api;
        let _ = std::thread::Builder::new()
            .name("vvmux-conpty-close".into())
            .spawn(move || unsafe { (api.close)(handle) });
    }
    if unsafe { WaitForSingleObject(inner.process.raw(), 250) } == WAIT_TIMEOUT {
        unsafe {
            TerminateJobObject(inner.job.raw(), 1);
        }
        let _ = unsafe { WaitForSingleObject(inner.process.raw(), 2_000) };
    }
}

impl Drop for ControlInner {
    fn drop(&mut self) {
        unsafe {
            TerminateJobObject(self.job.raw(), 1);
        }
        if let Some(handle) = self
            .pseudoconsole
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let api = self.api;
            let _ = std::thread::Builder::new()
                .name("vvmux-conpty-drop".into())
                .spawn(move || unsafe { (api.close)(handle) });
        }
    }
}

struct PseudoconsoleGuard {
    api: ConptyApi,
    handle: Option<HPCON>,
}

impl Drop for PseudoconsoleGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            unsafe { (self.api.close)(handle) };
        }
    }
}

struct OwnedHandle(HANDLE);

unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl OwnedHandle {
    fn new(handle: HANDLE) -> io::Result<Self> {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(handle))
        }
    }

    fn raw(&self) -> HANDLE {
        self.0
    }

    fn duplicate(&self) -> io::Result<Self> {
        use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let process = unsafe { GetCurrentProcess() };
        let mut duplicate = ptr::null_mut();
        if unsafe {
            DuplicateHandle(
                process,
                self.0,
                process,
                &mut duplicate,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        } == 0
        {
            Err(io::Error::last_os_error())
        } else {
            Self::new(duplicate)
        }
    }

    fn into_file(self) -> File {
        let raw = self.0;
        std::mem::forget(self);
        unsafe { File::from_raw_handle(raw as RawHandle) }
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn anonymous_pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read = ptr::null_mut();
    let mut write = ptr::null_mut();
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 1,
    };
    if unsafe { CreatePipe(&mut read, &mut write, &attributes, 64 * 1024) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok((OwnedHandle::new(read)?, OwnedHandle::new(write)?))
    }
}

fn set_inheritable(handle: HANDLE, inheritable: bool) -> io::Result<()> {
    let flags = if inheritable { HANDLE_FLAG_INHERIT } else { 0 };
    if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, flags) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn create_kill_job() -> io::Result<OwnedHandle> {
    let job = OwnedHandle::new(unsafe { CreateJobObjectW(ptr::null(), ptr::null()) })?;
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            job.raw(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(job)
    }
}

struct AttributeList {
    storage: Vec<usize>,
}

impl AttributeList {
    fn new(count: u32) -> io::Result<Self> {
        let mut bytes = 0;
        unsafe {
            InitializeProcThreadAttributeList(ptr::null_mut(), count, 0, &mut bytes);
        }
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let words = bytes.div_ceil(std::mem::size_of::<usize>());
        let mut result = Self {
            storage: vec![0usize; words],
        };
        if unsafe { InitializeProcThreadAttributeList(result.pointer(), count, 0, &mut bytes) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(result)
    }

    fn pointer(&mut self) -> *mut c_void {
        self.storage.as_mut_ptr().cast()
    }

    fn set_pseudoconsole(&mut self, handle: HPCON) -> io::Result<()> {
        if unsafe {
            UpdateProcThreadAttribute(
                self.pointer(),
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                handle as *const c_void,
                std::mem::size_of::<HPCON>(),
                ptr::null_mut(),
                ptr::null(),
            )
        } == 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.pointer()) };
    }
}

fn environment_block(overrides: &[(String, String)]) -> io::Result<Vec<u16>> {
    let mut values = BTreeMap::<String, (OsString, OsString)>::new();
    for (key, value) in std::env::vars_os() {
        let folded = key.to_string_lossy().to_uppercase();
        // cmd.exe stores hidden per-drive working directories under names that
        // begin with '=' ("=C:=C:\..."), and std::env::vars_os surfaces them;
        // they are shell bookkeeping, not variables a pane should inherit.
        if folded.starts_with('=') {
            continue;
        }
        validate_environment_pair(&key, &value)?;
        if !folded.starts_with("VIVID_") {
            values.entry(folded).or_insert((key, value));
        }
    }
    for (key, value) in overrides {
        let key = OsString::from(key);
        let value = OsString::from(value);
        validate_environment_pair(&key, &value)?;
        values.insert(key.to_string_lossy().to_uppercase(), (key, value));
    }
    let mut block = Vec::new();
    for (_, (key, value)) in values {
        block.extend(OsStr::new(&key).encode_wide());
        block.push(b'=' as u16);
        block.extend(OsStr::new(&value).encode_wide());
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

fn validate_environment_pair(key: &OsStr, value: &OsStr) -> io::Result<()> {
    let key_units = key.encode_wide().collect::<Vec<_>>();
    let value_units = value.encode_wide().collect::<Vec<_>>();
    if key_units.is_empty()
        || key_units
            .iter()
            .any(|unit| *unit == 0 || *unit == b'=' as u16)
        || value_units.contains(&0)
        || key_units.len() + value_units.len() > 32_767
    {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Windows pane environment entry",
        ))
    } else {
        Ok(())
    }
}

fn quote_argument(argument: &str) -> String {
    if !argument.is_empty()
        && !argument
            .bytes()
            .any(|byte| matches!(byte, b' ' | b'\t' | b'"'))
    {
        return argument.to_owned();
    }
    let mut result = String::from('"');
    let mut backslashes = 0;
    for character in argument.chars() {
        if character == '\\' {
            backslashes += 1;
        } else {
            result.extend(std::iter::repeat_n(
                '\\',
                if character == '"' {
                    backslashes * 2 + 1
                } else {
                    backslashes
                },
            ));
            backslashes = 0;
            result.push(character);
        }
    }
    result.extend(std::iter::repeat_n('\\', backslashes * 2));
    result.push('"');
    result
}

fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut result = Vec::new();
    for unit in value.encode_wide() {
        if unit == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows path contains NUL",
            ));
        }
        result.push(unit);
    }
    result.push(0);
    Ok(result)
}

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "Windows 10 build 17763 or newer with ConPTY is required",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_microsoft_command_line_arguments() {
        assert_eq!(quote_argument("cmd.exe"), "cmd.exe");
        assert_eq!(
            quote_argument(r"C:\Program Files\shell.exe"),
            r#""C:\Program Files\shell.exe""#
        );
        assert_eq!(quote_argument(r#"C:\a "b"\"#), r#""C:\a \"b\"\\""#);
        assert_eq!(quote_argument(r#"a\"b"#), r#""a\\\"b""#);
    }

    #[test]
    fn environment_overrides_are_case_insensitive_and_double_terminated() {
        let block =
            environment_block(&[("Path".into(), "one".into()), ("PATH".into(), "two".into())])
                .unwrap();
        assert!(block.ends_with(&[0, 0]));
        let decoded = String::from_utf16_lossy(&block);
        assert!(decoded.contains("PATH=two\0"));
        assert!(!decoded.contains("Path=one\0"));
    }
}
