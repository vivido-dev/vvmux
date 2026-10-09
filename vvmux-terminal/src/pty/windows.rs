use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString, c_void};
use std::fmt;
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
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
    STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows_sys::core::HRESULT;

use super::{PtyParts, input};

/// How long `terminate_blocking` waits for a pane to exit on its own, in milliseconds.
///
/// Matches the blocking path's purpose: give the console host time to deliver the close event
/// before the job is killed. Raising it lengthens synchronous session teardown.
const EXIT_GRACE_MS: u32 = 500;

/// How long background cleanup waits for a pane to exit after closing its console.
const CLOSE_GRACE_MS: u32 = 250;

/// How long to wait for a killed job's process to be reaped before giving up.
///
/// Termination is asynchronous in the kernel; this bounds the wait so a stuck process cannot hang
/// teardown, at the cost of possibly returning before the process object is signaled.
const KILL_WAIT_MS: u32 = 2_000;

/// Buffer size for each anonymous pipe between vvmux and the pseudoconsole.
///
/// 64 KiB holds several full-screen repaints, so a briefly slow reader does not stall the console
/// host; it is the size most terminal emulators use for ConPTY pipes.
const PIPE_BUFFER_BYTES: u32 = 64 * 1024;

/// The longest `NAME=value` entry the Windows environment block accepts, in UTF-16 code units.
const MAX_ENVIRONMENT_ENTRY_UNITS: usize = 32_767;

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
    /// Resolve the ConPTY entry points, which older Windows builds do not export.
    fn load() -> io::Result<Self> {
        type LoadedFn = unsafe extern "system" fn() -> isize;
        let module = wide(OsStr::new("kernel32.dll"))?;
        // SAFETY: `module` is a NUL-terminated UTF-16 string that outlives the call, and
        // kernel32 is always loaded, so the returned module handle stays valid for the process.
        let kernel32 = unsafe { GetModuleHandleW(module.as_ptr()) };
        if kernel32.is_null() {
            return Err(unsupported());
        }
        // SAFETY: `kernel32` is a loaded module handle and each name is a NUL-terminated C string.
        let create = unsafe { GetProcAddress(kernel32, c"CreatePseudoConsole".as_ptr().cast()) }
            .ok_or_else(unsupported)?;
        // SAFETY: as above.
        let resize = unsafe { GetProcAddress(kernel32, c"ResizePseudoConsole".as_ptr().cast()) }
            .ok_or_else(unsupported)?;
        // SAFETY: as above.
        let close = unsafe { GetProcAddress(kernel32, c"ClosePseudoConsole".as_ptr().cast()) }
            .ok_or_else(unsupported)?;
        // SAFETY: each pointer is the export of that exact name, and the target types match the
        // documented Win32 signatures and the `system` calling convention; transmuting between
        // function-pointer types of the same size only restores the real signature.
        let create = unsafe { std::mem::transmute::<LoadedFn, CreatePseudoConsoleFn>(create) };
        // SAFETY: as above, for `ResizePseudoConsole`.
        let resize = unsafe { std::mem::transmute::<LoadedFn, ResizePseudoConsoleFn>(resize) };
        // SAFETY: as above, for `ClosePseudoConsole`.
        let close = unsafe { std::mem::transmute::<LoadedFn, ClosePseudoConsoleFn>(close) };
        Ok(Self {
            create,
            resize,
            close,
        })
    }
}

/// Shared handle for resizing and terminating one ConPTY pane.
///
/// Clones share the same pane. Dropping the last clone terminates the pane's job object, so an
/// abandoned pane never keeps its processes alive.
#[derive(Clone)]
pub struct PtyControl {
    inner: Arc<ControlInner>,
}

impl fmt::Debug for PtyControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PtyControl")
            .field("closing", &self.inner.closing.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

/// Owner of the pane's process handle, used to collect its exit status.
#[derive(Debug)]
pub struct PtyWaiter {
    process: OwnedHandle,
}

/// How a pane's process ended.
#[derive(Debug, Clone, Copy)]
pub struct PtyExitStatus {
    /// The process exit code.
    pub code: Option<i64>,
    /// Always `None`: Windows processes do not end by signal.
    pub signal: Option<i32>,
    /// Whether the process exited with code zero.
    pub success: bool,
}

struct ControlInner {
    api: ConptyApi,
    pseudoconsole: Mutex<Option<HPCON>>,
    process: OwnedHandle,
    job: OwnedHandle,
    closing: AtomicBool,
}

impl PtyControl {
    /// Always `None`: Windows has no foreground process group.
    #[must_use]
    pub fn foreground_process_group_id(&self) -> Option<u32> {
        None
    }

    /// Resize the pseudoconsole to `columns` by `rows` cells.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] for a zero dimension,
    /// [`io::ErrorKind::BrokenPipe`] once the pane is closing, or an error carrying the
    /// `ResizePseudoConsole` failure.
    pub fn resize(&self, columns: u16, rows: u16) -> io::Result<()> {
        self.resize_with_pixels(columns, rows, 0, 0)
    }

    /// Resize the pseudoconsole; ConPTY has no pixel size, so those arguments are ignored.
    ///
    /// # Errors
    ///
    /// The same as [`PtyControl::resize`].
    pub fn resize_with_pixels(
        &self,
        columns: u16,
        rows: u16,
        _pixel_width: u16,
        _pixel_height: u16,
    ) -> io::Result<()> {
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
        // SAFETY: `handle` is a live pseudoconsole: it is only closed after being taken out of
        // this mutex, which the guard holds for the duration of the call.
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

    /// Windows has no POSIX signals and no foreground process group.
    ///
    /// A ConPTY pane's children live in a job object, which supports termination but not the
    /// selective delivery a signal expresses; there is no equivalent of "interrupt the foreground
    /// job and leave its shell alone". Refused here rather than approximated, because a caller
    /// asking for `INT` and silently getting a job-wide kill would be worse than being told no.
    /// `Ctrl+C` is still available as ordinary input through `key`.
    ///
    /// # Errors
    ///
    /// Always returns [`io::ErrorKind::Unsupported`].
    pub fn signal(&self, _signal: i32) -> io::Result<u32> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows panes have no process group to signal",
        ))
    }

    /// Start closing the pane in the background, killing its job if it does not exit promptly.
    ///
    /// Returns immediately. Only the first termination request on a pane has any effect.
    pub fn terminate(&self) {
        if self.inner.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let _ = std::thread::Builder::new()
            .name("vvmux-conpty-cleanup".into())
            .spawn(move || cleanup(inner));
    }

    /// Close the pane and wait for its process, killing the job if it outlives the grace period.
    pub fn terminate_blocking(&self) {
        self.terminate();
        // SAFETY: `process` is an open process handle owned by `inner` for this whole call.
        let result = unsafe { WaitForSingleObject(self.inner.process.raw(), EXIT_GRACE_MS) };
        if result == WAIT_TIMEOUT {
            // SAFETY: `job` is an open job handle owned by `inner`.
            unsafe { TerminateJobObject(self.inner.job.raw(), 1) };
            // SAFETY: as for the first wait.
            let _ = unsafe { WaitForSingleObject(self.inner.process.raw(), KILL_WAIT_MS) };
        }
    }
}

impl PtyWaiter {
    /// Block until the pane's process exits and report its exit code.
    ///
    /// # Errors
    ///
    /// Returns the OS error when waiting fails or the exit code cannot be read.
    pub fn wait(self) -> io::Result<PtyExitStatus> {
        // SAFETY: `process` is an open process handle owned by `self`.
        if unsafe { WaitForSingleObject(self.process.raw(), INFINITE) } == WAIT_OBJECT_0 {
            let mut code = 0;
            // SAFETY: `process` is open, and `code` is a live local the call writes once.
            if unsafe { GetExitCodeProcess(self.process.raw(), &raw mut code) } == 0 {
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
    command: Option<&OsStr>,
    cwd: &Path,
    columns: u16,
    rows: u16,
    environment: &[(String, String)],
) -> io::Result<PtyParts> {
    spawn_prepared(
        shell,
        build_command_line(shell, command),
        cwd,
        columns,
        rows,
        environment,
    )
}

pub(super) fn spawn_argv(
    program: &OsStr,
    arguments: &[impl AsRef<OsStr>],
    cwd: &Path,
    columns: u16,
    rows: u16,
    environment: &[(String, String)],
) -> io::Result<PtyParts> {
    let mut command_line = quote_argument(&program.to_string_lossy());
    for argument in arguments {
        command_line.push(' ');
        command_line.push_str(&quote_argument(&argument.as_ref().to_string_lossy()));
    }
    spawn_prepared(program, command_line, cwd, columns, rows, environment)
}

fn spawn_prepared(
    program: &OsStr,
    command_line: String,
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
    if !cwd.is_absolute() || !Path::new(program).is_absolute() {
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
    // SAFETY: both pipe handles are open for the duration of the call, and `pseudoconsole` is a
    // live local that the call writes once.
    let result = unsafe {
        (api.create)(
            COORD {
                X: columns as i16,
                Y: rows as i16,
            },
            input_child.raw(),
            output_child.raw(),
            0,
            &raw mut pseudoconsole,
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

    let application = wide(program)?;
    let mut command_line = wide(OsStr::new(&command_line))?;
    let cwd = wide(cwd.as_os_str())?;
    let environment = environment_block(environment)?;

    let job = create_kill_job()?;
    let mut process_info = PROCESS_INFORMATION::default();
    // SAFETY: `application`, `cwd`, and `environment` are NUL-terminated UTF-16 buffers and
    // `command_line` is a mutable NUL-terminated buffer, all outliving the call; `startup` and its
    // attribute list stay alive until after the call; and `process_info` is written once.
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
            &raw const startup.StartupInfo,
            &raw mut process_info,
        )
    };
    if created == 0 {
        return Err(io::Error::last_os_error());
    }
    let process = OwnedHandle::new(process_info.hProcess)?;
    let thread = OwnedHandle::new(process_info.hThread)?;

    // SAFETY: `job` and `process` are open handles owned by this function.
    if unsafe { AssignProcessToJobObject(job.raw(), process.raw()) } == 0 {
        let error = io::Error::last_os_error();
        // SAFETY: `process` is an open, still-suspended process handle.
        unsafe { TerminateProcess(process.raw(), 1) };
        return Err(error);
    }
    // SAFETY: `thread` is the open handle of the suspended primary thread.
    if unsafe { ResumeThread(thread.raw()) } == u32::MAX {
        let error = io::Error::last_os_error();
        // SAFETY: `job` is an open job handle that now contains the process.
        unsafe { TerminateJobObject(job.raw(), 1) };
        return Err(error);
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
        child_pid: process_info.dwProcessId,
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
        // `ClosePseudoConsole` blocks until the console host drains, so it runs off this thread.
        let _ = std::thread::Builder::new()
            .name("vvmux-conpty-close".into())
            // SAFETY: `handle` was taken out of the mutex, so this is its only close.
            .spawn(move || unsafe { (api.close)(handle) });
    }
    // SAFETY: `process` is an open process handle owned by `inner`, which this function holds.
    if unsafe { WaitForSingleObject(inner.process.raw(), CLOSE_GRACE_MS) } == WAIT_TIMEOUT {
        // SAFETY: `job` is an open job handle owned by `inner`.
        unsafe { TerminateJobObject(inner.job.raw(), 1) };
        // SAFETY: as for the first wait.
        let _ = unsafe { WaitForSingleObject(inner.process.raw(), KILL_WAIT_MS) };
    }
}

impl Drop for ControlInner {
    fn drop(&mut self) {
        // SAFETY: `job` is still open; it is closed only when this value's fields drop.
        unsafe { TerminateJobObject(self.job.raw(), 1) };
        if let Some(handle) = self
            .pseudoconsole
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let api = self.api;
            let _ = std::thread::Builder::new()
                .name("vvmux-conpty-drop".into())
                // SAFETY: `handle` was taken out of the mutex, so this is its only close.
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
            // SAFETY: the guard still owns `handle`; taking it out makes this the only close.
            unsafe { (self.api.close)(handle) };
        }
    }
}

/// A kernel handle closed exactly once, on drop.
#[derive(Debug)]
struct OwnedHandle(HANDLE);

// SAFETY: a Win32 kernel handle is a process-wide table index, valid from any thread. Every use
// here goes through thread-safe kernel calls, and the handle is closed only once, by `Drop`.
unsafe impl Send for OwnedHandle {}
// SAFETY: as above; `&OwnedHandle` only exposes the raw value for those thread-safe calls.
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
        // SAFETY: `GetCurrentProcess` has no preconditions and returns a pseudo-handle.
        let process = unsafe { GetCurrentProcess() };
        let mut duplicate = ptr::null_mut();
        // SAFETY: `self.0` is an open handle, and `duplicate` is a live local written once.
        if unsafe {
            DuplicateHandle(
                process,
                self.0,
                process,
                &raw mut duplicate,
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
        // SAFETY: ownership of the open handle moves from the forgotten `OwnedHandle` to the file,
        // so it is still closed exactly once.
        unsafe { File::from_raw_handle(raw as RawHandle) }
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle is open and owned by `self`; this is its only close.
        unsafe { CloseHandle(self.0) };
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
    // SAFETY: the two handle slots are live locals written once, and `attributes` outlives the
    // call.
    if unsafe {
        CreatePipe(
            &raw mut read,
            &raw mut write,
            &raw const attributes,
            PIPE_BUFFER_BYTES,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok((OwnedHandle::new(read)?, OwnedHandle::new(write)?))
    }
}

fn set_inheritable(handle: HANDLE, inheritable: bool) -> io::Result<()> {
    let flags = if inheritable { HANDLE_FLAG_INHERIT } else { 0 };
    // SAFETY: callers pass an open handle that they own.
    if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, flags) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn create_kill_job() -> io::Result<OwnedHandle> {
    // SAFETY: null attributes and a null name request an unnamed job with default security.
    let job = OwnedHandle::new(unsafe { CreateJobObjectW(ptr::null(), ptr::null()) })?;
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: `job` is open, and the pointer and length describe the live `limits` structure.
    if unsafe {
        SetInformationJobObject(
            job.raw(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
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
        // SAFETY: a null list asks only for the required size, which is written to `bytes`.
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), count, 0, &raw mut bytes) };
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let words = bytes.div_ceil(std::mem::size_of::<usize>());
        let mut result = Self {
            storage: vec![0usize; words],
        };
        // SAFETY: `storage` is pointer-aligned and at least `bytes` long, as the sizing call
        // required.
        if unsafe { InitializeProcThreadAttributeList(result.pointer(), count, 0, &raw mut bytes) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(result)
    }

    fn pointer(&mut self) -> *mut c_void {
        self.storage.as_mut_ptr().cast()
    }

    fn set_pseudoconsole(&mut self, handle: HPCON) -> io::Result<()> {
        // SAFETY: the list was initialized by `new`. For the pseudoconsole attribute, the value
        // is the `HPCON` itself, not a pointer to it, as `CreateProcessW` documents.
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
        // SAFETY: the list was initialized by `new` and is deleted exactly once.
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
        block.push(u16::from(b'='));
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
            .any(|unit| *unit == 0 || *unit == u16::from(b'='))
        || value_units.contains(&0)
        || key_units.len() + value_units.len() > MAX_ENVIRONMENT_ENTRY_UNITS
    {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Windows pane environment entry",
        ))
    } else {
        Ok(())
    }
}

/// The flag that makes `shell` run a single command string and exit.
///
/// `cmd.exe` and the PowerShell hosts each spell this differently, and a shell we do not
/// recognize is most likely POSIX-flavored (Git Bash, busybox), which takes `-c`.
fn command_flag(shell: &OsStr) -> &'static str {
    let stem = Path::new(shell)
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match stem.as_str() {
        "cmd" => "/C",
        "powershell" | "pwsh" => "-Command",
        _ => "-c",
    }
}

/// Build the `CreateProcessW` command line for a pane.
///
/// Without a command this is just the quoted shell, exactly as before. With one, the command is
/// passed as a single quoted argument so the shell — not `CreateProcessW` — does the parsing.
fn build_command_line(shell: &OsStr, command: Option<&OsStr>) -> String {
    let shell_argument = quote_argument(&shell.to_string_lossy());
    match command {
        Some(command) => format!(
            "{shell_argument} {} {}",
            command_flag(shell),
            quote_argument(&command.to_string_lossy())
        ),
        None => shell_argument,
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
    fn a_command_pane_uses_the_shell_specific_run_flag() {
        assert_eq!(
            command_flag(OsStr::new(r"C:\Windows\system32\cmd.exe")),
            "/C"
        );
        assert_eq!(
            command_flag(OsStr::new(r"C:\Windows\System32\CMD.EXE")),
            "/C",
            "the shell name is matched case-insensitively"
        );
        assert_eq!(command_flag(OsStr::new(r"C:\pwsh.exe")), "-Command");
        assert_eq!(command_flag(OsStr::new(r"C:\powershell.exe")), "-Command");
        assert_eq!(
            command_flag(OsStr::new(r"C:\Program Files\Git\bin\bash.exe")),
            "-c",
            "an unrecognized shell is assumed POSIX-flavored"
        );
    }

    #[test]
    fn a_command_line_quotes_the_command_as_one_argument() {
        let shell = OsStr::new(r"C:\Windows\system32\cmd.exe");

        assert_eq!(
            build_command_line(shell, None),
            r"C:\Windows\system32\cmd.exe",
            "a shell pane keeps the previous command line exactly"
        );
        assert_eq!(
            build_command_line(shell, Some(OsStr::new("echo hello world"))),
            r#"C:\Windows\system32\cmd.exe /C "echo hello world""#,
            "the whole command stays one argument so the shell parses it"
        );
        assert_eq!(
            build_command_line(
                OsStr::new(r"C:\Program Files\Git\bin\bash.exe"),
                Some(OsStr::new("printf 'a b'"))
            ),
            r#""C:\Program Files\Git\bin\bash.exe" -c "printf 'a b'""#
        );
    }

    #[test]
    fn argv_command_line_quotes_each_argument_independently() {
        let program = OsStr::new(r"C:\Program Files\tool.exe");
        let arguments = [
            OsStr::new("plain"),
            OsStr::new("two words"),
            OsStr::new(r#"a\"b"#),
        ];
        let mut line = quote_argument(&program.to_string_lossy());
        for argument in arguments {
            line.push(' ');
            line.push_str(&quote_argument(&argument.to_string_lossy()));
        }
        assert_eq!(
            line,
            r#""C:\Program Files\tool.exe" plain "two words" "a\\\"b""#
        );
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
