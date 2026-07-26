use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString, c_void};
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{ConnectionCancel, Transport};
use crate::ipc::DisplayMetrics;
use sha2::{Digest, Sha256};
use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_BROKEN_PIPE, ERROR_IO_INCOMPLETE,
    ERROR_IO_PENDING, ERROR_OPERATION_ABORTED, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED,
    ERROR_SEM_TIMEOUT, GENERIC_ALL, GENERIC_READ, GENERIC_WRITE, HANDLE, HANDLE_FLAG_INHERIT,
    INVALID_HANDLE_VALUE, LocalFree, SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, CreateWellKnownSid,
    DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorControl,
    GetTokenInformation, INHERITED_ACE, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser, WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateDirectoryW, CreateFileW, DELETE, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
    FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_FIRST_PIPE_INSTANCE,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_OVERLAPPED, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FileAttributeTagInfo, FileDispositionInfo, GetFileInformationByHandleEx,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, SetFileInformationByHandle, WriteFile,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Console::{
    AllocConsole, CONSOLE_CURSOR_INFO, CONSOLE_FONT_INFOEX, CONSOLE_SCREEN_BUFFER_INFO, COORD,
    DISABLE_NEWLINE_AUTO_RETURN, ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS, ENABLE_LINE_INPUT,
    ENABLE_PROCESSED_INPUT, ENABLE_QUICK_EDIT_MODE, ENABLE_VIRTUAL_TERMINAL_INPUT,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING, ENABLE_WINDOW_INPUT, FreeConsole, GetConsoleCP,
    GetConsoleCursorInfo, GetConsoleMode, GetConsoleOutputCP, GetConsoleScreenBufferInfo,
    GetConsoleTitleW, GetCurrentConsoleFontEx, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE, SetConsoleCP, SetConsoleCtrlHandler, SetConsoleCursorInfo,
    SetConsoleCursorPosition, SetConsoleMode, SetConsoleOutputCP, SetConsoleTitleW, SetStdHandle,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, CreatePipe, GetNamedPipeClientProcessId,
    GetNamedPipeServerProcessId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateEventW, CreateProcessW, DETACHED_PROCESS,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
    InitializeProcThreadAttributeList, OpenProcess, OpenProcessToken,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, SetEvent, UpdateProcThreadAttribute,
    WaitForMultipleObjects,
};
use windows_sys::Win32::UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath};

pub type SessionEndpoint = String;
pub type VirtualPresenterEndpoint = PathBuf;

const ENTER_TERMINAL: &[u8] =
    b"\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?1004h\x1b[?2004l";
const LEAVE_TERMINAL: &[u8] =
    b"\x1b[0m\x1b[?2004l\x1b[?1004l\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?25h\x1b[?1049l";
const DEFAULT_CELL_WIDTH_PX: u16 = 10;
const DEFAULT_CELL_HEIGHT_PX: u16 = 20;

static CONSOLE_SHUTDOWN: AtomicBool = AtomicBool::new(false);
static CONSOLE_WAKE_EVENT: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

pub fn windows_fallback_shell() -> OsString {
    let mut buffer = vec![0_u16; 32_768];
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if length != 0 && length < buffer.len() {
        return PathBuf::from(OsString::from_wide(&buffer[..length]))
            .join("cmd.exe")
            .into_os_string();
    }
    OsString::from(r"C:\Windows\System32\cmd.exe")
}

pub struct ClientTerminal {
    input: HANDLE,
    output: File,
    input_mode: u32,
    output_mode: u32,
    input_code_page: u32,
    output_code_page: u32,
    title: Vec<u16>,
    cursor: CONSOLE_CURSOR_INFO,
    cursor_position: COORD,
    wake: OwnedHandle,
}

impl ClientTerminal {
    pub fn enter() -> io::Result<Self> {
        let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let output_handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        require_handle(input, "stdin is not an interactive Windows console")?;
        require_handle(
            output_handle,
            "stdout is not an interactive Windows console",
        )?;

        let mut input_mode = 0;
        let mut output_mode = 0;
        if unsafe { GetConsoleMode(input, &mut input_mode) } == 0
            || unsafe { GetConsoleMode(output_handle, &mut output_mode) } == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "vvmux attach requires an interactive Windows console",
            ));
        }

        let mut cursor = CONSOLE_CURSOR_INFO::default();
        if unsafe { GetConsoleCursorInfo(output_handle, &mut cursor) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut screen = CONSOLE_SCREEN_BUFFER_INFO::default();
        if unsafe { GetConsoleScreenBufferInfo(output_handle, &mut screen) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut title = vec![0u16; 32_768];
        let title_length = unsafe { GetConsoleTitleW(title.as_mut_ptr(), title.len() as u32) };
        title.truncate(title_length as usize);

        let raw_input = client_input_mode(input_mode);
        let vt_output =
            output_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN;

        if unsafe { SetConsoleMode(input, raw_input) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { SetConsoleMode(output_handle, vt_output) } == 0 {
            unsafe { SetConsoleMode(input, input_mode) };
            return Err(io::Error::last_os_error());
        }

        let input_code_page = unsafe { GetConsoleCP() };
        let output_code_page = unsafe { GetConsoleOutputCP() };
        if unsafe { SetConsoleCP(65001) } == 0 || unsafe { SetConsoleOutputCP(65001) } == 0 {
            let error = io::Error::last_os_error();
            unsafe {
                SetConsoleCP(input_code_page);
                SetConsoleOutputCP(output_code_page);
                SetConsoleMode(input, input_mode);
                SetConsoleMode(output_handle, output_mode);
            }
            return Err(error);
        }

        let mut output = match duplicate_file(output_handle) {
            Ok(output) => output,
            Err(error) => {
                unsafe {
                    SetConsoleMode(input, input_mode);
                    SetConsoleMode(output_handle, output_mode);
                    SetConsoleCP(input_code_page);
                    SetConsoleOutputCP(output_code_page);
                }
                return Err(error);
            }
        };
        if let Err(error) = output
            .write_all(ENTER_TERMINAL)
            .and_then(|()| output.flush())
        {
            unsafe {
                SetConsoleMode(input, input_mode);
                SetConsoleMode(output_handle, output_mode);
                SetConsoleCP(input_code_page);
                SetConsoleOutputCP(output_code_page);
            }
            return Err(error);
        }

        let wake = match OwnedHandle::new(unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) }) {
            Ok(wake) => wake,
            Err(error) => {
                let _ = output.write_all(LEAVE_TERMINAL);
                unsafe {
                    SetConsoleMode(input, input_mode);
                    SetConsoleMode(output_handle, output_mode);
                    SetConsoleCP(input_code_page);
                    SetConsoleOutputCP(output_code_page);
                }
                return Err(error);
            }
        };
        let terminal = Self {
            input,
            output,
            input_mode,
            output_mode,
            input_code_page,
            output_code_page,
            title,
            cursor,
            cursor_position: screen.dwCursorPosition,
            wake,
        };
        CONSOLE_SHUTDOWN.store(false, Ordering::Release);
        CONSOLE_WAKE_EVENT.store(terminal.wake.raw(), Ordering::Release);
        if unsafe { SetConsoleCtrlHandler(Some(console_control_handler), 1) } == 0 {
            CONSOLE_WAKE_EVENT.store(ptr::null_mut(), Ordering::Release);
            let error = io::Error::last_os_error();
            drop(terminal);
            return Err(error);
        }
        Ok(terminal)
    }

    pub fn display_metrics(&self) -> io::Result<DisplayMetrics> {
        current_display_metrics()
    }

    pub fn output(&self) -> io::Result<Box<dyn Write + Send>> {
        Ok(Box::new(self.output.try_clone()?))
    }

    pub fn read_input(&self, buffer: &mut [u8], timeout: Duration) -> io::Result<Option<usize>> {
        let wait_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        let handles = [self.input, self.wake.raw()];
        match unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, wait_ms) } {
            WAIT_OBJECT_0 => io::stdin().read(buffer).map(Some),
            result if result == WAIT_OBJECT_0 + 1 => Ok(Some(0)),
            WAIT_TIMEOUT => Ok(None),
            _ => Err(io::Error::last_os_error()),
        }
    }
}

/// Query the attached console without borrowing `ClientTerminal`, allowing the Windows client to
/// observe viewport changes on a thread that cannot be stalled by console input semantics.
pub fn current_display_metrics() -> io::Result<DisplayMetrics> {
    let output = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
    if unsafe { GetConsoleScreenBufferInfo(output, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let columns = u16::try_from(info.srWindow.Right - info.srWindow.Left + 1)
        .map_err(|_| io::Error::other("console width is invalid"))?;
    let rows = u16::try_from(info.srWindow.Bottom - info.srWindow.Top + 1)
        .map_err(|_| io::Error::other("console height is invalid"))?;
    if columns == 0 || rows == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "console has zero dimensions",
        ));
    }
    let mut font = CONSOLE_FONT_INFOEX {
        cbSize: std::mem::size_of::<CONSOLE_FONT_INFOEX>() as u32,
        ..CONSOLE_FONT_INFOEX::default()
    };
    let has_font = unsafe { GetCurrentConsoleFontEx(output, 0, &mut font) } != 0;
    let (cell_width, cell_height) = console_cell_size(has_font.then_some(font.dwFontSize));
    Ok(DisplayMetrics {
        columns,
        rows,
        cell_width,
        cell_height,
    })
}

fn client_input_mode(original: u32) -> u32 {
    // ReadFile only returns character/VT input, while a console input handle is signaled for
    // every queued record. Leaving window input enabled lets a resize record wake read_input()
    // and then strand it inside the blocking byte read until a key or mouse event arrives.
    // Viewport changes are polled independently by the client loop, so do not queue records that
    // this input path cannot consume.
    (original | ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_EXTENDED_FLAGS)
        & !(ENABLE_LINE_INPUT
            | ENABLE_ECHO_INPUT
            | ENABLE_PROCESSED_INPUT
            | ENABLE_QUICK_EDIT_MODE
            | ENABLE_WINDOW_INPUT)
}

fn console_cell_size(size: Option<COORD>) -> (u16, u16) {
    // Pseudoconsole hosts may not expose a legacy console font. Vivid 1.0 requires nonzero cell
    // and viewport dimensions, so use the same fallback geometry as Vivi on Windows.
    let width = size
        .and_then(|size| u16::try_from(size.X).ok())
        .filter(|width| *width != 0)
        .unwrap_or(DEFAULT_CELL_WIDTH_PX);
    let height = size
        .and_then(|size| u16::try_from(size.Y).ok())
        .filter(|height| *height != 0)
        .unwrap_or(DEFAULT_CELL_HEIGHT_PX);
    (width, height)
}

impl Drop for ClientTerminal {
    fn drop(&mut self) {
        unsafe {
            SetConsoleCtrlHandler(Some(console_control_handler), 0);
        }
        CONSOLE_WAKE_EVENT.store(ptr::null_mut(), Ordering::Release);
        let _ = self
            .output
            .write_all(LEAVE_TERMINAL)
            .and_then(|()| self.output.flush());
        let output = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        unsafe {
            SetConsoleTitleW(wide_nul(&self.title).as_ptr());
            SetConsoleCursorInfo(output, &self.cursor);
            SetConsoleCursorPosition(output, self.cursor_position);
            SetConsoleCP(self.input_code_page);
            SetConsoleOutputCP(self.output_code_page);
            SetConsoleMode(output, self.output_mode);
            SetConsoleMode(self.input, self.input_mode);
        }
    }
}

pub fn console_restoration_self_test() -> io::Result<()> {
    unsafe {
        FreeConsole();
    }
    if unsafe { AllocConsole() } == 0 {
        return Err(io::Error::last_os_error());
    }
    let result = (|| {
        let input = open_console_device("CONIN$")?;
        let output = open_console_device("CONOUT$")?;
        if unsafe { SetStdHandle(STD_INPUT_HANDLE, input.raw()) } == 0
            || unsafe { SetStdHandle(STD_OUTPUT_HANDLE, output.raw()) } == 0
            || unsafe { SetStdHandle(STD_ERROR_HANDLE, output.raw()) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let before = ConsoleSnapshot::capture(input.raw(), output.raw())?;
        let entered = Arc::new(AtomicBool::new(false));
        let unwind_entered = entered.clone();
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let unwind = std::panic::catch_unwind(move || {
            let _terminal = ClientTerminal::enter().expect("console self-test could not enter");
            unwind_entered.store(true, Ordering::Release);
            panic!("injected console restoration self-test panic");
        });
        std::panic::set_hook(previous_hook);
        if !entered.load(Ordering::Acquire) || unwind.is_ok() {
            return Err(io::Error::other(
                "console restoration self-test did not reach the injected unwind",
            ));
        }
        let after = ConsoleSnapshot::capture(input.raw(), output.raw())?;
        if before != after {
            return Err(io::Error::other(
                "Windows console state changed across ClientTerminal unwind",
            ));
        }
        Ok(())
    })();
    unsafe {
        FreeConsole();
    }
    result
}

#[derive(PartialEq, Eq)]
struct ConsoleSnapshot {
    input_mode: u32,
    output_mode: u32,
    input_code_page: u32,
    output_code_page: u32,
    title: Vec<u16>,
    cursor_size: u32,
    cursor_visible: i32,
    cursor_x: i16,
    cursor_y: i16,
}

impl ConsoleSnapshot {
    fn capture(input: HANDLE, output: HANDLE) -> io::Result<Self> {
        let mut input_mode = 0;
        let mut output_mode = 0;
        let mut cursor = CONSOLE_CURSOR_INFO::default();
        let mut screen = CONSOLE_SCREEN_BUFFER_INFO::default();
        if unsafe { GetConsoleMode(input, &mut input_mode) } == 0
            || unsafe { GetConsoleMode(output, &mut output_mode) } == 0
            || unsafe { GetConsoleCursorInfo(output, &mut cursor) } == 0
            || unsafe { GetConsoleScreenBufferInfo(output, &mut screen) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut title = vec![0u16; 32_768];
        let length = unsafe { GetConsoleTitleW(title.as_mut_ptr(), title.len() as u32) };
        title.truncate(length as usize);
        Ok(Self {
            input_mode,
            output_mode,
            input_code_page: unsafe { GetConsoleCP() },
            output_code_page: unsafe { GetConsoleOutputCP() },
            title,
            cursor_size: cursor.dwSize,
            cursor_visible: cursor.bVisible,
            cursor_x: screen.dwCursorPosition.X,
            cursor_y: screen.dwCursorPosition.Y,
        })
    }
}

fn open_console_device(name: &str) -> io::Result<OwnedHandle> {
    let name = wide_string(name)?;
    OwnedHandle::new(unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    })
}

unsafe extern "system" fn console_control_handler(_event: u32) -> i32 {
    CONSOLE_SHUTDOWN.store(true, Ordering::Release);
    let wake = CONSOLE_WAKE_EVENT.load(Ordering::Acquire);
    if !wake.is_null() {
        unsafe {
            SetEvent(wake);
        }
    }
    1
}

pub struct SessionListener {
    endpoint: Vec<u16>,
    security: Arc<SecurityDescriptor>,
    pending: Mutex<Option<Box<PendingPipe>>>,
}

impl SessionListener {
    pub fn bind(endpoint: &str) -> io::Result<Self> {
        let endpoint = wide_string(endpoint)?;
        let security = Arc::new(SecurityDescriptor::for_current_user(false)?);
        let pending = create_pending_pipe(&endpoint, &security, true)?;
        Ok(Self {
            endpoint,
            security,
            pending: Mutex::new(Some(Box::new(pending))),
        })
    }

    pub fn accept(&self) -> io::Result<Transport> {
        let mut slot = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_none() {
            *slot = Some(Box::new(create_pending_pipe(
                &self.endpoint,
                &self.security,
                false,
            )?));
        }
        let pipe = slot.as_mut().expect("pending pipe was created");
        if !pipe.poll_connect()? {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        let connected = slot.take().expect("connected pipe is present");
        *slot = Some(Box::new(create_pending_pipe(
            &self.endpoint,
            &self.security,
            false,
        )?));
        drop(slot);

        require_pipe_client_owner(connected.handle.raw())?;
        split_pipe(connected.handle.clone())
    }
}

pub fn connect_session(endpoint: &str) -> io::Result<Transport> {
    let endpoint = wide_string(endpoint)?;
    if unsafe { WaitNamedPipeW(endpoint.as_ptr(), 3_000) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let handle = unsafe {
        CreateFileW(
            endpoint.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            ptr::null_mut(),
        )
    };
    let handle = OwnedHandle::new(handle)?;
    require_pipe_server_owner(handle.raw())?;
    split_pipe(Arc::new(handle))
}

pub fn session_is_connectable(endpoint: &str) -> bool {
    connect_session(endpoint).is_ok()
}

pub struct VirtualPresenterListener {
    inner: TcpListener,
    endpoint: String,
}

impl VirtualPresenterListener {
    pub fn bind(_endpoint: PathBuf) -> io::Result<Self> {
        let inner = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        inner.set_nonblocking(true)?;
        let address = inner.local_addr()?;
        let SocketAddr::V4(address) = address else {
            return Err(io::Error::other("Windows Vivid listener is not IPv4"));
        };
        if *address.ip() != Ipv4Addr::LOCALHOST {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows Vivid listener is not bound to exact IPv4 loopback",
            ));
        }
        Ok(Self {
            inner,
            endpoint: format!("tcp:127.0.0.1:{}", address.port()),
        })
    }

    pub fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    pub fn accept(&self) -> io::Result<Transport> {
        let (stream, peer) = self.inner.accept()?;
        stream.set_nonblocking(false)?;
        match peer {
            SocketAddr::V4(address) if *address.ip() == Ipv4Addr::LOCALHOST => split_tcp(stream),
            _ => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows Vivid peer is not exact IPv4 loopback",
            )),
        }
    }
}

pub fn windows_config_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(|root| root.join("vvmux/config.toml"))
}

pub fn windows_runtime_root() -> io::Result<PathBuf> {
    let mut raw = ptr::null_mut();
    let result =
        unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, 0, ptr::null_mut(), &mut raw) };
    if result < 0 || raw.is_null() {
        return Err(io::Error::other(format!(
            "SHGetKnownFolderPath(FOLDERID_LocalAppData) failed with HRESULT 0x{:08x}",
            result as u32
        )));
    }
    let mut length = 0;
    unsafe {
        while *raw.add(length) != 0 {
            length += 1;
        }
    }
    let local = OsString::from_wide(unsafe { std::slice::from_raw_parts(raw, length) });
    unsafe { CoTaskMemFree(raw.cast()) };

    let security = SecurityDescriptor::for_current_user(true)?;
    let root = PathBuf::from(local).join("vvmux");
    ensure_secure_directory(&root, &security)?;
    let runtime = root.join("runtime");
    ensure_secure_directory(&runtime, &security)?;
    Ok(runtime)
}

pub fn validate_windows_registry_file(path: &std::path::Path) -> io::Result<()> {
    open_windows_registry_file(path, false).map(drop)
}

pub fn create_secure_windows_registry_file(path: &std::path::Path) -> io::Result<File> {
    let path = wide_os(path.as_os_str())?;
    let security = SecurityDescriptor::for_current_user(false)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.pointer.cast(),
        bInheritHandle: 0,
    };
    let handle = OwnedHandle::new(unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE,
            0,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    })?;
    validate_handle(handle.raw(), false)?;
    Ok(handle.into_file())
}

pub fn open_windows_registry_file(path: &std::path::Path, delete_access: bool) -> io::Result<File> {
    let path = wide_os(path.as_os_str())?;
    let access = GENERIC_READ | if delete_access { DELETE } else { 0 };
    let handle = OwnedHandle::new(unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    })?;
    validate_handle(handle.raw(), false)?;
    Ok(handle.into_file())
}

pub fn delete_open_windows_registry_file(file: &File) -> io::Result<()> {
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileDispositionInfo,
            (&disposition as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn windows_pipe_name(session_name: &str) -> io::Result<String> {
    let sid = ProcessToken::current()?.sid_string()?;
    let sid_hash = domain_hash(b"vvmux windows sid v1\0", sid.as_bytes());
    let session_hash = domain_hash(b"vvmux windows session v1\0", session_name.as_bytes());
    Ok(format!(
        r"\\.\pipe\vvmux-{}-{}",
        hex_prefix(&sid_hash, 16),
        hex_prefix(&session_hash, 16)
    ))
}

pub struct DaemonLauncher;

impl DaemonLauncher {
    pub fn launch(name: &str, config_path: Option<&std::path::Path>) -> io::Result<()> {
        let executable = std::env::current_exe()?;
        let (ready_reader, ready_writer) = inheritable_pipe()?;
        set_handle_inheritance(ready_reader.raw(), false)?;
        let null = inheritable_null()?;

        let mut command_line = quote_windows(&executable.to_string_lossy());
        command_line.push_str(" __server --session ");
        command_line.push_str(&quote_windows(name));
        command_line.push_str(" --ready-handle ");
        command_line.push_str(&(ready_writer.raw() as usize).to_string());
        if let Some(path) = config_path {
            command_line.push_str(" --config ");
            command_line.push_str(&quote_windows(&path.to_string_lossy()));
        }
        let application = wide_os(executable.as_os_str())?;
        let mut command_line = wide_os(OsStr::new(&command_line))?;
        let environment = daemon_environment()?;

        let mut attributes = ProcessAttributeList::new(1)?;
        attributes.set_handle_list(&[ready_writer.raw(), null.raw()])?;
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = null.raw();
        startup.StartupInfo.hStdOutput = null.raw();
        startup.StartupInfo.hStdError = null.raw();
        startup.lpAttributeList = attributes.pointer();
        let mut process = PROCESS_INFORMATION::default();
        if unsafe {
            CreateProcessW(
                application.as_ptr(),
                command_line.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                1,
                // No CREATE_NEW_PROCESS_GROUP: it would start the daemon with Ctrl+C
                // disabled, and that state is inherited by every pane process the daemon
                // spawns, making ^C unable to interrupt anything inside vvmux. The daemon
                // is detached from any console, so the group isolation buys nothing.
                DETACHED_PROCESS | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
                environment.as_ptr().cast(),
                ptr::null(),
                &startup.StartupInfo,
                &mut process,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let process_handle = OwnedHandle::new(process.hProcess)?;
        let thread_handle = OwnedHandle::new(process.hThread)?;
        drop(thread_handle);
        drop(ready_writer);
        drop(null);
        drop(attributes);

        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("vvmux-startup-readiness".into())
            .spawn(move || {
                let mut reader = ready_reader.into_file();
                let mut bytes = Vec::new();
                let result = Read::by_ref(&mut reader).take(4097).read_to_end(&mut bytes);
                let _ = sender.send(result.map(|_| bytes));
            })?;
        let result = receiver
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "vvmux server startup timed out")
            })??;
        drop(process_handle);
        if result == b"OK\n" {
            Ok(())
        } else if result.starts_with(b"ERR\n") {
            let diagnostic = String::from_utf8_lossy(&result[4..]);
            Err(io::Error::other(format!(
                "vvmux server startup failed: {diagnostic}"
            )))
        } else if result.len() > 4096 {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "vvmux server startup diagnostic exceeded 4 KiB",
            ))
        } else {
            Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "vvmux server exited without a readiness result",
            ))
        }
    }
}

/// Restore normal Ctrl+C processing for the server and everything it spawns. The
/// disabled state set by a launcher's CREATE_NEW_PROCESS_GROUP is inherited by every
/// child, which would leave pane processes unable to be interrupted.
pub fn prepare_server_process() {
    unsafe {
        SetConsoleCtrlHandler(None, 0);
    }
}

pub struct ReadinessWriter {
    file: Option<File>,
}

impl ReadinessWriter {
    pub fn from_metadata(handle: Option<usize>) -> io::Result<Self> {
        let file = handle
            .filter(|handle| *handle != 0 && *handle != INVALID_HANDLE_VALUE as usize)
            .map(|handle| unsafe { File::from_raw_handle(handle as RawHandle) });
        Ok(Self { file })
    }

    pub fn success(&mut self) -> io::Result<()> {
        self.write_result(b"OK\n")
    }

    pub fn failure(&mut self, error: &io::Error) {
        let mut message = format!("ERR\n{error}").into_bytes();
        message.truncate(4096);
        let _ = self.write_result(&message);
    }

    fn write_result(&mut self, bytes: &[u8]) -> io::Result<()> {
        let Some(mut file) = self.file.take() else {
            return Ok(());
        };
        file.write_all(bytes)?;
        file.flush()
    }
}

struct PendingPipe {
    handle: Arc<OwnedHandle>,
    event: OwnedHandle,
    overlapped: OVERLAPPED,
    connecting: bool,
}

unsafe impl Send for PendingPipe {}

impl PendingPipe {
    fn poll_connect(&mut self) -> io::Result<bool> {
        if !self.connecting {
            let result = unsafe { ConnectNamedPipe(self.handle.raw(), &mut self.overlapped) };
            if result != 0 {
                return Ok(true);
            }
            match last_error_code() {
                ERROR_IO_PENDING => self.connecting = true,
                ERROR_PIPE_CONNECTED => return Ok(true),
                _ => return Err(io::Error::last_os_error()),
            }
        }
        let mut transferred = 0;
        let complete = unsafe {
            GetOverlappedResult(self.handle.raw(), &self.overlapped, &mut transferred, 0)
        };
        if complete != 0 {
            Ok(true)
        } else if last_error_code() == ERROR_IO_INCOMPLETE {
            Ok(false)
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

impl Drop for PendingPipe {
    fn drop(&mut self) {
        unsafe {
            CancelIoEx(self.handle.raw(), &self.overlapped);
        }
        let _ = self.event.raw();
    }
}

fn create_pending_pipe(
    endpoint: &[u16],
    security: &SecurityDescriptor,
    first: bool,
) -> io::Result<PendingPipe> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.pointer.cast(),
        bInheritHandle: 0,
    };
    let first_flag = if first {
        FILE_FLAG_FIRST_PIPE_INSTANCE
    } else {
        0
    };
    let handle = unsafe {
        CreateNamedPipeW(
            endpoint.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | first_flag,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            64 * 1024,
            64 * 1024,
            0,
            &attributes,
        )
    };
    let handle = Arc::new(OwnedHandle::new(handle)?);
    let event = OwnedHandle::new(unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) })?;
    let overlapped = OVERLAPPED {
        hEvent: event.raw(),
        ..OVERLAPPED::default()
    };
    Ok(PendingPipe {
        handle,
        event,
        overlapped,
        connecting: false,
    })
}

#[derive(Clone)]
struct PipeReader {
    handle: Arc<OwnedHandle>,
    timeout: Arc<Mutex<Option<Duration>>>,
}

struct PipeWriter {
    handle: Arc<OwnedHandle>,
}

impl Read for PipeReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let event = OwnedHandle::new(unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) })?;
        let mut overlapped = OVERLAPPED {
            hEvent: event.raw(),
            ..OVERLAPPED::default()
        };
        let mut transferred = 0;
        let result = unsafe {
            ReadFile(
                self.handle.raw(),
                buffer.as_mut_ptr(),
                u32::try_from(buffer.len()).unwrap_or(u32::MAX),
                &mut transferred,
                &mut overlapped,
            )
        };
        if result == 0 && last_error_code() != ERROR_IO_PENDING {
            return pipe_read_error();
        }
        if result != 0 {
            return Ok(transferred as usize);
        }

        let timeout = *self
            .timeout
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let complete = if let Some(timeout) = timeout {
            unsafe {
                GetOverlappedResultEx(
                    self.handle.raw(),
                    &overlapped,
                    &mut transferred,
                    u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1),
                    0,
                )
            }
        } else {
            unsafe { GetOverlappedResult(self.handle.raw(), &overlapped, &mut transferred, 1) }
        };
        if complete != 0 {
            Ok(transferred as usize)
        } else if last_error_code() == ERROR_SEM_TIMEOUT {
            unsafe {
                CancelIoEx(self.handle.raw(), &overlapped);
                GetOverlappedResult(self.handle.raw(), &overlapped, &mut transferred, 1);
            }
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "named-pipe read timed out",
            ))
        } else {
            pipe_read_error()
        }
    }
}

impl Write for PipeWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let event = OwnedHandle::new(unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) })?;
        let mut overlapped = OVERLAPPED {
            hEvent: event.raw(),
            ..OVERLAPPED::default()
        };
        let mut transferred = 0;
        let result = unsafe {
            WriteFile(
                self.handle.raw(),
                buffer.as_ptr(),
                u32::try_from(buffer.len()).unwrap_or(u32::MAX),
                &mut transferred,
                &mut overlapped,
            )
        };
        if result == 0 && last_error_code() != ERROR_IO_PENDING {
            return Err(pipe_write_error());
        }
        if result == 0
            && unsafe { GetOverlappedResult(self.handle.raw(), &overlapped, &mut transferred, 1) }
                == 0
        {
            return Err(pipe_write_error());
        }
        Ok(transferred as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn split_pipe(handle: Arc<OwnedHandle>) -> io::Result<Transport> {
    let timeout = Arc::new(Mutex::new(None));
    let reader = PipeReader {
        handle: handle.clone(),
        timeout: timeout.clone(),
    };
    let writer = PipeWriter {
        handle: handle.clone(),
    };
    let cancel_handle = handle.clone();
    let cancel = ConnectionCancel::new(move || unsafe {
        CancelIoEx(cancel_handle.raw(), ptr::null());
    });
    let set_timeout = Arc::new(move |duration| {
        *timeout
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = duration;
        Ok(())
    });
    Ok(Transport::new(
        Box::new(reader),
        Box::new(writer),
        cancel,
        set_timeout,
    ))
}

fn pipe_read_error() -> io::Result<usize> {
    let error = io::Error::last_os_error();
    match error.raw_os_error().map(|code| code as u32) {
        Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_OPERATION_ABORTED) => Ok(0),
        _ => Err(error),
    }
}

fn pipe_write_error() -> io::Error {
    let error = io::Error::last_os_error();
    match error.raw_os_error().map(|code| code as u32) {
        Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_OPERATION_ABORTED) => {
            io::Error::new(io::ErrorKind::BrokenPipe, "named-pipe connection closed")
        }
        _ => error,
    }
}

struct OwnedHandle(HANDLE);

unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl OwnedHandle {
    fn new(handle: HANDLE) -> io::Result<Self> {
        require_handle(handle, "Windows returned an invalid handle")?;
        Ok(Self(handle))
    }

    fn raw(&self) -> HANDLE {
        self.0
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

struct SecurityDescriptor {
    pointer: PSECURITY_DESCRIPTOR,
}

unsafe impl Send for SecurityDescriptor {}
unsafe impl Sync for SecurityDescriptor {}

impl SecurityDescriptor {
    fn for_current_user(inherit_to_children: bool) -> io::Result<Self> {
        let token = ProcessToken::current()?;
        let sid = token.sid_string()?;
        let flags = if inherit_to_children { "OICI" } else { "" };
        let sddl = format!("O:{sid}G:{sid}D:P(A;{flags};GA;;;SY)(A;{flags};GA;;;{sid})");
        let sddl = wide_string(&sddl)?;
        let mut pointer = ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut pointer,
                ptr::null_mut(),
            )
        } == 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self { pointer })
        }
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.pointer.cast());
        }
    }
}

struct ProcessToken {
    _token: OwnedHandle,
    buffer: Vec<u8>,
}

impl ProcessToken {
    fn current() -> io::Result<Self> {
        Self::from_process(unsafe { GetCurrentProcess() })
    }

    fn for_pid(pid: u32) -> io::Result<Self> {
        let process =
            OwnedHandle::new(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) })?;
        Self::from_process(process.raw())
    }

    fn from_process(process: HANDLE) -> io::Result<Self> {
        let mut token = ptr::null_mut();
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle::new(token)?;
        let mut length = 0;
        unsafe {
            GetTokenInformation(token.raw(), TokenUser, ptr::null_mut(), 0, &mut length);
        }
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0u8; length as usize];
        if unsafe {
            GetTokenInformation(
                token.raw(),
                TokenUser,
                buffer.as_mut_ptr().cast(),
                length,
                &mut length,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            _token: token,
            buffer,
        })
    }

    fn sid(&self) -> *mut core::ffi::c_void {
        unsafe { (*(self.buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid }
    }

    fn sid_string(&self) -> io::Result<String> {
        let mut string = ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(self.sid(), &mut string) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut length = 0;
        unsafe {
            while *string.add(length) != 0 {
                length += 1;
            }
        }
        let value = String::from_utf16(unsafe { std::slice::from_raw_parts(string, length) })
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "user SID is not UTF-16"));
        unsafe {
            LocalFree(string.cast());
        }
        value
    }
}

fn require_pipe_client_owner(handle: HANDLE) -> io::Result<()> {
    let mut pid = 0;
    if unsafe { GetNamedPipeClientProcessId(handle, &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    require_process_owner(pid)
}

fn require_pipe_server_owner(handle: HANDLE) -> io::Result<()> {
    let mut pid = 0;
    if unsafe { GetNamedPipeServerProcessId(handle, &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    require_process_owner(pid)
}

fn require_process_owner(pid: u32) -> io::Result<()> {
    let current = ProcessToken::current()?;
    let peer = ProcessToken::for_pid(pid)?;
    if unsafe { EqualSid(current.sid(), peer.sid()) } == 0 {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "named-pipe peer belongs to a different Windows user",
        ))
    } else {
        Ok(())
    }
}

fn wide_string(value: &str) -> io::Result<Vec<u16>> {
    if value.encode_utf16().any(|unit| unit == 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows string contains NUL",
        ));
    }
    Ok(value.encode_utf16().chain(std::iter::once(0)).collect())
}

fn last_error_code() -> u32 {
    io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or_default() as u32
}

fn inheritable_pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 1,
    };
    let mut reader = ptr::null_mut();
    let mut writer = ptr::null_mut();
    if unsafe { CreatePipe(&mut reader, &mut writer, &attributes, 4096) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok((OwnedHandle::new(reader)?, OwnedHandle::new(writer)?))
    }
}

fn inheritable_null() -> io::Result<OwnedHandle> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 1,
    };
    let name = wide_string("NUL")?;
    OwnedHandle::new(unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    })
}

fn set_handle_inheritance(handle: HANDLE, inheritable: bool) -> io::Result<()> {
    if unsafe {
        SetHandleInformation(
            handle,
            HANDLE_FLAG_INHERIT,
            if inheritable { HANDLE_FLAG_INHERIT } else { 0 },
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

struct ProcessAttributeList {
    storage: Vec<usize>,
    handles: Vec<HANDLE>,
}

impl ProcessAttributeList {
    fn new(count: u32) -> io::Result<Self> {
        let mut bytes = 0;
        unsafe {
            InitializeProcThreadAttributeList(ptr::null_mut(), count, 0, &mut bytes);
        }
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut list = Self {
            storage: vec![0; bytes.div_ceil(std::mem::size_of::<usize>())],
            handles: Vec::new(),
        };
        if unsafe { InitializeProcThreadAttributeList(list.pointer(), count, 0, &mut bytes) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(list)
        }
    }

    fn pointer(&mut self) -> *mut core::ffi::c_void {
        self.storage.as_mut_ptr().cast()
    }

    fn set_handle_list(&mut self, handles: &[HANDLE]) -> io::Result<()> {
        // The attribute list stores the buffer pointer, not a copy: the handle
        // array must stay alive until DeleteProcThreadAttributeList, so it is
        // owned here rather than borrowed from the caller.
        self.handles = handles.to_vec();
        if unsafe {
            UpdateProcThreadAttribute(
                self.pointer(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                self.handles.as_ptr().cast(),
                std::mem::size_of_val(self.handles.as_slice()),
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

impl Drop for ProcessAttributeList {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.pointer()) };
    }
}

fn daemon_environment() -> io::Result<Vec<u16>> {
    daemon_environment_from(std::env::vars_os())
}

fn daemon_environment_from(
    environment: impl IntoIterator<Item = (OsString, OsString)>,
) -> io::Result<Vec<u16>> {
    let mut variables = BTreeMap::<String, (OsString, OsString)>::new();
    for (key, value) in environment {
        let folded = key.to_string_lossy().to_uppercase();
        // cmd.exe stores hidden per-drive working directories under names that
        // begin with '=' ("=C:=C:\..."), and std::env::vars_os surfaces them;
        // they are shell bookkeeping, not variables the daemon should inherit.
        if folded.starts_with("VIVID_")
            || folded.starts_with('=')
            || matches!(folded.as_str(), "SSH_AUTH_SOCK" | "SSH_AGENT_PID")
        {
            continue;
        }
        if key
            .encode_wide()
            .any(|unit| unit == 0 || unit == b'=' as u16)
            || value.encode_wide().any(|unit| unit == 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid variable in Windows daemon environment",
            ));
        }
        variables.entry(folded).or_insert((key, value));
    }
    let mut block = Vec::new();
    for (_, (key, value)) in variables {
        block.extend(key.encode_wide());
        block.push(b'=' as u16);
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

fn quote_windows(argument: &str) -> String {
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

fn wide_os(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut result = Vec::new();
    for unit in value.encode_wide() {
        if unit == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows value contains NUL",
            ));
        }
        result.push(unit);
    }
    result.push(0);
    Ok(result)
}

fn ensure_secure_directory(
    path: &std::path::Path,
    security: &SecurityDescriptor,
) -> io::Result<()> {
    let path_wide = wide_os(path.as_os_str())?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.pointer.cast(),
        bInheritHandle: 0,
    };
    if unsafe { CreateDirectoryW(path_wide.as_ptr(), &attributes) } == 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists && error.raw_os_error() != Some(183) {
            return Err(error);
        }
    }
    validate_file_attributes(path, true)?;
    validate_security(path, true)
}

fn validate_file_attributes(path: &std::path::Path, directory: bool) -> io::Result<()> {
    let handle = open_runtime_object(path, directory)?;
    validate_handle_attributes(handle.raw(), directory)
}

fn validate_handle_attributes(handle: HANDLE, directory: bool) -> io::Result<()> {
    let mut attributes = FILE_ATTRIBUTE_TAG_INFO::default();
    if unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileAttributeTagInfo,
            (&mut attributes as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let is_directory = attributes.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if is_directory != directory || attributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe Windows runtime object type or reparse point",
        ))
    } else {
        Ok(())
    }
}

fn validate_security(path: &std::path::Path, directory: bool) -> io::Result<()> {
    let handle = open_runtime_object(path, directory)?;
    validate_handle_security(handle.raw())
}

fn validate_handle_security(handle: HANDLE) -> io::Result<()> {
    let mut owner = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let result = validate_security_parts(owner, dacl, descriptor);
    unsafe { LocalFree(descriptor.cast()) };
    result
}

fn validate_handle(handle: HANDLE, directory: bool) -> io::Result<()> {
    validate_handle_attributes(handle, directory)?;
    validate_handle_security(handle)
}

fn open_runtime_object(path: &std::path::Path, directory: bool) -> io::Result<OwnedHandle> {
    let path = wide_os(path.as_os_str())?;
    let flags = FILE_FLAG_OPEN_REPARSE_POINT
        | if directory {
            FILE_FLAG_BACKUP_SEMANTICS
        } else {
            0
        };
    OwnedHandle::new(unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            flags,
            ptr::null_mut(),
        )
    })
}

fn validate_security_parts(
    owner: *mut c_void,
    dacl: *mut ACL,
    descriptor: PSECURITY_DESCRIPTOR,
) -> io::Result<()> {
    if owner.is_null() || dacl.is_null() || descriptor.is_null() {
        return Err(unsafe_runtime_security());
    }
    let current = ProcessToken::current()?;
    if unsafe { EqualSid(owner, current.sid()) } == 0 {
        return Err(unsafe_runtime_security());
    }
    let mut control = 0;
    let mut revision = 0;
    if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0
        || control & SE_DACL_PROTECTED == 0
    {
        return Err(unsafe_runtime_security());
    }

    let system = local_system_sid()?;
    let mut information = ACL_SIZE_INFORMATION::default();
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut information as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
        || information.AceCount != 2
    {
        return Err(unsafe_runtime_security());
    }
    let mut current_seen = false;
    let mut system_seen = false;
    for index in 0..information.AceCount {
        let mut raw = ptr::null_mut();
        if unsafe { GetAce(dacl, index, &mut raw) } == 0 || raw.is_null() {
            return Err(unsafe_runtime_security());
        }
        let ace = unsafe { &*(raw.cast::<ACCESS_ALLOWED_ACE>()) };
        if ace.Header.AceType != 0
            || ace.Header.AceFlags & INHERITED_ACE as u8 != 0
            || ace.Mask != GENERIC_ALL && ace.Mask & 0x001f_01ff != 0x001f_01ff
        {
            return Err(unsafe_runtime_security());
        }
        let sid = (&ace.SidStart as *const u32).cast_mut().cast();
        if unsafe { EqualSid(sid, current.sid()) } != 0 {
            if current_seen {
                return Err(unsafe_runtime_security());
            }
            current_seen = true;
        } else if unsafe { EqualSid(sid, system.as_ptr().cast_mut().cast()) } != 0 {
            if system_seen {
                return Err(unsafe_runtime_security());
            }
            system_seen = true;
        } else {
            return Err(unsafe_runtime_security());
        }
    }
    if current_seen && system_seen {
        Ok(())
    } else {
        Err(unsafe_runtime_security())
    }
}

fn local_system_sid() -> io::Result<Vec<u8>> {
    let mut length = 68;
    let mut sid = vec![0u8; length as usize];
    if unsafe {
        CreateWellKnownSid(
            WinLocalSystemSid,
            ptr::null_mut(),
            sid.as_mut_ptr().cast(),
            &mut length,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        sid.truncate(length as usize);
        Ok(sid)
    }
}

fn unsafe_runtime_security() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Windows vvmux runtime object has an unsafe owner or DACL",
    )
}

fn domain_hash(domain: &[u8], value: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(domain);
    hash.update(value);
    hash.finalize().into()
}

fn hex_prefix(bytes: &[u8], length: usize) -> String {
    bytes[..length]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn split_tcp(stream: TcpStream) -> io::Result<Transport> {
    stream.set_nodelay(true)?;
    let reader = stream.try_clone()?;
    let timeout_stream = stream.try_clone()?;
    let cancel_reader = reader.try_clone()?;
    let cancel_writer = stream.try_clone()?;
    let cancel = ConnectionCancel::new(move || {
        let _ = cancel_reader.shutdown(Shutdown::Both);
        let _ = cancel_writer.shutdown(Shutdown::Both);
    });
    let timeout = Arc::new(move |duration| timeout_stream.set_read_timeout(duration));
    Ok(Transport::new(
        Box::new(reader),
        Box::new(stream),
        cancel,
        timeout,
    ))
}

fn require_handle(handle: HANDLE, message: &'static str) -> io::Result<()> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(io::Error::new(io::ErrorKind::InvalidInput, message))
    } else {
        Ok(())
    }
}

fn duplicate_file(handle: HANDLE) -> io::Result<File> {
    let mut duplicate = ptr::null_mut();
    let process = unsafe { GetCurrentProcess() };
    if unsafe {
        DuplicateHandle(
            process,
            handle,
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
        Ok(unsafe { File::from_raw_handle(duplicate as RawHandle) })
    }
}

fn wide_nul(value: &[u16]) -> Vec<u16> {
    let mut result = Vec::with_capacity(value.len() + 1);
    result.extend(value.iter().copied().filter(|unit| *unit != 0));
    result.push(0);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn microsoft_quoting_handles_spaces_quotes_and_trailing_slashes() {
        assert_eq!(quote_windows("plain"), "plain");
        assert_eq!(
            quote_windows(r"C:\Program Files\vvmux.exe"),
            r#""C:\Program Files\vvmux.exe""#
        );
        assert_eq!(quote_windows(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(
            quote_windows(r"C:\path with space\"),
            r#""C:\path with space\\""#
        );
    }

    #[test]
    fn unavailable_console_font_uses_valid_vivid_cell_dimensions() {
        assert_eq!(
            console_cell_size(None),
            (DEFAULT_CELL_WIDTH_PX, DEFAULT_CELL_HEIGHT_PX)
        );
        assert_eq!(
            console_cell_size(Some(COORD { X: 0, Y: -1 })),
            (DEFAULT_CELL_WIDTH_PX, DEFAULT_CELL_HEIGHT_PX)
        );

        let (cell_width, cell_height) = console_cell_size(None);
        let display = vivid_protocol::messages::DisplayChanged {
            display_generation: 1,
            viewport_width: 80 * u32::from(cell_width),
            viewport_height: 22 * u32::from(cell_height),
            grid_columns: 80,
            grid_rows: 22,
            cell_width: u32::from(cell_width),
            cell_height: u32::from(cell_height),
            settled: true,
        };
        let welcome = vivid_protocol::messages::welcome(1, 1, &[1; 16], 1, display, &[]);
        vivid_protocol::messages::parse_welcome(&welcome).unwrap();
    }

    #[test]
    fn client_input_mode_does_not_queue_unreadable_resize_records() {
        let original = ENABLE_LINE_INPUT
            | ENABLE_ECHO_INPUT
            | ENABLE_PROCESSED_INPUT
            | ENABLE_QUICK_EDIT_MODE
            | ENABLE_WINDOW_INPUT;
        let mode = client_input_mode(original);

        assert_eq!(
            mode & (ENABLE_LINE_INPUT
                | ENABLE_ECHO_INPUT
                | ENABLE_PROCESSED_INPUT
                | ENABLE_QUICK_EDIT_MODE
                | ENABLE_WINDOW_INPUT),
            0
        );
        assert_ne!(mode & ENABLE_VIRTUAL_TERMINAL_INPUT, 0);
        assert_ne!(mode & ENABLE_EXTENDED_FLAGS, 0);
    }

    #[test]
    fn client_starts_with_outer_bracketed_paste_disabled() {
        assert!(ENTER_TERMINAL.ends_with(b"\x1b[?2004l"));
        assert!(
            !ENTER_TERMINAL
                .windows(b"\x1b[?2004h".len())
                .any(|window| window == b"\x1b[?2004h")
        );
    }

    #[test]
    fn daemon_environment_is_sorted_deduplicated_and_secret_free() {
        let block = daemon_environment_from([
            (OsString::from("=C:"), OsString::from(r"C:\Users\someone")),
            (OsString::from("=ExitCode"), OsString::from("00000000")),
            (OsString::from("zebra"), OsString::from("last")),
            (OsString::from("Path"), OsString::from("first")),
            (OsString::from("PATH"), OsString::from("ignored")),
            (OsString::from("VIVID_TOKEN"), OsString::from("secret")),
            (OsString::from("vivid_endpoint"), OsString::from("secret")),
            (OsString::from("SSH_AUTH_SOCK"), OsString::from("secret")),
            (OsString::from("alpha"), OsString::from("first")),
        ])
        .unwrap();
        assert!(block.ends_with(&[0, 0]));
        let entries = decode_environment_block(&block);
        assert_eq!(entries, ["alpha=first", "Path=first", "zebra=last"]);
    }

    #[test]
    fn daemon_environment_rejects_malformed_variables() {
        let interior_equals =
            daemon_environment_from([(OsString::from("a=b"), OsString::from("value"))]);
        assert_eq!(
            interior_equals.unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let nul_value = daemon_environment_from([(OsString::from("a"), OsString::from("v\0"))]);
        assert_eq!(nul_value.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn pipe_names_are_domain_separated_and_hide_identity_text() {
        let name = windows_pipe_name("private-session").unwrap();
        assert!(name.starts_with(r"\\.\pipe\vvmux-"));
        assert_eq!(name.len(), r"\\.\pipe\vvmux-".len() + 32 + 1 + 32);
        assert!(!name.contains("private-session"));
        assert_eq!(name, windows_pipe_name("private-session").unwrap());
        assert_ne!(name, windows_pipe_name("private-session-2").unwrap());
    }

    #[test]
    fn owner_only_named_pipe_round_trip_and_cancel() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let endpoint = format!(r"\\.\pipe\vvmux-test-{}-{unique}", std::process::id());
        let listener = SessionListener::bind(&endpoint).unwrap();
        let server = std::thread::spawn(move || {
            let mut transport = loop {
                match listener.accept() {
                    Ok(transport) => break transport,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::yield_now();
                    }
                    Err(error) => panic!("named-pipe accept failed: {error}"),
                }
            };
            let mut request = [0; 6];
            transport.reader.read_exact(&mut request).unwrap();
            assert_eq!(&request, b"abcdef");
            transport.writer.write_all(b"reply").unwrap();
            let mut cancelled = [0; 1];
            assert_eq!(transport.reader.read(&mut cancelled).unwrap(), 0);
        });

        let mut client = connect_session(&endpoint).unwrap();
        client.writer.write_all(b"abc").unwrap();
        client.writer.write_all(b"def").unwrap();
        let mut reply = [0; 5];
        client.reader.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"reply");
        client.cancel().cancel();
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn runtime_directories_have_the_expected_owner_dacl() {
        let runtime = windows_runtime_root().unwrap();
        validate_file_attributes(&runtime, true).unwrap();
        validate_security(&runtime, true).unwrap();
    }

    fn decode_environment_block(block: &[u16]) -> Vec<String> {
        block
            .split(|unit| *unit == 0)
            .take_while(|entry| !entry.is_empty())
            .map(String::from_utf16_lossy)
            .collect()
    }
}
