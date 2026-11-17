//! Native Windows Service Control Manager integration for the client supervisor.
//! The system service always requires an explicit non-LocalSystem identity.
use crate::Result;
use serde_json::{Value, json};
use std::{
    ffi::c_void,
    io,
    path::Path,
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        ERROR_CALL_NOT_IMPLEMENTED, ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST,
        ERROR_SERVICE_NOT_ACTIVE, ERROR_SERVICE_SPECIFIC_ERROR, LocalFree,
    },
    Security::{Authorization::ConvertSidToStringSidW, LookupAccountNameW},
    Storage::FileSystem::DELETE,
    System::Services::*,
};

const NAME: &str = "ssmcp";
const DESCRIPTION: &str = "ssmcp client session manager v1";
static STOP: OnceLock<Arc<AtomicBool>> = OnceLock::new();
static STATUS: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static STATE: AtomicU32 = AtomicU32::new(SERVICE_START_PENDING);
static CHECKPOINT: AtomicU32 = AtomicU32::new(0);
static REPORT_LOCK: Mutex<()> = Mutex::new(());
static RUN_ERROR: Mutex<Option<String>> = Mutex::new(None);

fn wide(text: &str) -> Result<Vec<u16>> {
    if text.contains('\0') {
        return Err("Windows service strings must not contain NUL".into());
    }
    Ok(text.encode_utf16().chain(Some(0)).collect())
}

fn os_error(action: &str) -> crate::Error {
    format!("{action}: {}", io::Error::last_os_error()).into()
}

/// Called from the normal async CLI, so it does not block that runtime's thread.
pub async fn dispatch() -> Result<()> {
    tokio::task::spawn_blocking(dispatch_blocking)
        .await
        .map_err(|_| "Windows service dispatcher task stopped unexpectedly")?
}

fn dispatch_blocking() -> Result<()> {
    let mut name = wide(NAME)?;
    let entries = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_mut_ptr(),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: null_mut(),
            lpServiceProc: None,
        },
    ];
    // SAFETY: the table and both callbacks remain valid until the dispatcher returns.
    if unsafe { StartServiceCtrlDispatcherW(entries.as_ptr()) } == 0 {
        return Err(os_error(
            "connect to Windows SCM (service run --scm must be launched by SCM)",
        ));
    }
    if let Some(error) = RUN_ERROR.lock().unwrap_or_else(|e| e.into_inner()).take() {
        return Err(error.into());
    }
    Ok(())
}

unsafe extern "system" fn service_main(_count: u32, _args: *mut *mut u16) {
    // Never unwind through the operating-system callback boundary.
    let result = std::panic::catch_unwind(|| -> Result<()> {
        let name = wide(NAME)?;
        // SAFETY: static callback and NUL-terminated name are valid for this call.
        let handle =
            unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control_handler), null()) };
        if handle.is_null() {
            return Err(os_error("register Windows service control handler"));
        }
        STATUS.store(handle, Ordering::Release);
        let stop = STOP
            .get_or_init(|| Arc::new(AtomicBool::new(false)))
            .clone();
        report(SERVICE_START_PENDING, false);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(run_supervisor(stop))
    })
    .unwrap_or_else(|_| Err("client service supervisor panicked".into()));
    let failed = result.is_err();
    if let Err(error) = result {
        *RUN_ERROR.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.to_string());
    }
    report(SERVICE_STOPPED, failed);
}

async fn run_supervisor(stop: Arc<AtomicBool>) -> Result<()> {
    let supervisor = super::service::run_with_shutdown(stop);
    tokio::pin!(supervisor);
    let readiness = async {
        loop {
            if super::service::available().await {
                return;
            }
            report(SERVICE_START_PENDING, false);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    };
    tokio::pin!(readiness);
    tokio::select! {
        biased;
        result = &mut supervisor => result,
        _ = &mut readiness => {
            // SCM running means the supervisor can actually accept requests.
            report(SERVICE_RUNNING, false);
            supervisor.await
        }
    }
}

unsafe extern "system" fn control_handler(
    control: u32,
    _kind: u32,
    _data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            STOP.get_or_init(|| Arc::new(AtomicBool::new(false)))
                .store(true, Ordering::Release);
            report(SERVICE_STOP_PENDING, false);
            0
        }
        SERVICE_CONTROL_INTERROGATE => {
            report(STATE.load(Ordering::Acquire), false);
            0
        }
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

fn report(mut state: u32, failed: bool) {
    let _guard = REPORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let handle = STATUS.load(Ordering::Acquire);
    if handle.is_null() {
        return;
    }
    if matches!(state, SERVICE_RUNNING | SERVICE_START_PENDING)
        && STOP.get().is_some_and(|stop| stop.load(Ordering::Acquire))
    {
        state = SERVICE_STOP_PENDING;
    }
    STATE.store(state, Ordering::Release);
    let pending = matches!(state, SERVICE_START_PENDING | SERVICE_STOP_PENDING);
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        },
        dwWin32ExitCode: if failed {
            ERROR_SERVICE_SPECIFIC_ERROR
        } else {
            0
        },
        dwServiceSpecificExitCode: u32::from(failed),
        dwCheckPoint: if pending {
            CHECKPOINT.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            0
        },
        dwWaitHint: if pending { 30_000 } else { 0 },
    };
    // SAFETY: the SCM owns this live status handle, and status is fully initialized.
    unsafe {
        SetServiceStatus(handle, &status);
    }
}

struct ServiceHandle(SC_HANDLE);
impl Drop for ServiceHandle {
    fn drop(&mut self) {
        // SAFETY: this RAII value uniquely owns a successfully opened SCM handle.
        unsafe {
            CloseServiceHandle(self.0);
        }
    }
}

fn manager(access: u32) -> Result<ServiceHandle> {
    // SAFETY: null selects the local SCM database; no pointers are retained.
    let handle = unsafe { OpenSCManagerW(null(), null(), access) };
    if handle.is_null() {
        Err(os_error(
            "open Windows service manager (system changes need elevation)",
        ))
    } else {
        Ok(ServiceHandle(handle))
    }
}

fn service(manager: &ServiceHandle, access: u32) -> Result<Option<ServiceHandle>> {
    let name = wide(NAME)?;
    // SAFETY: manager is live and name is NUL terminated.
    let handle = unsafe { OpenServiceW(manager.0, name.as_ptr(), access) };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32) {
            Ok(None)
        } else {
            Err(format!("open ssmcp Windows service: {error}").into())
        }
    } else {
        Ok(Some(ServiceHandle(handle)))
    }
}

/// Resolve the actual account SID, so aliases of LocalSystem cannot pass policy.
pub fn account_sid(account: &str) -> Result<String> {
    let account = wide(account)?;
    let mut sid_size = 0;
    let mut domain_size = 0;
    let mut kind = 0;
    // SAFETY: a first bounded-size query uses null output buffers.
    unsafe {
        LookupAccountNameW(
            null(),
            account.as_ptr(),
            null_mut(),
            &mut sid_size,
            null_mut(),
            &mut domain_size,
            &mut kind,
        );
    }
    if sid_size == 0 || sid_size > 65536 || domain_size > 65536 {
        return Err(os_error("resolve Windows service account"));
    }
    // usize backing gives the SID storage appropriate alignment.
    let mut sid = vec![0usize; (sid_size as usize).div_ceil(std::mem::size_of::<usize>())];
    let mut domain = vec![0u16; domain_size as usize];
    // SAFETY: both mutable buffers have the capacities returned by the first query.
    if unsafe {
        LookupAccountNameW(
            null(),
            account.as_ptr(),
            sid.as_mut_ptr().cast(),
            &mut sid_size,
            domain.as_mut_ptr(),
            &mut domain_size,
            &mut kind,
        )
    } == 0
    {
        return Err(os_error("resolve Windows service account"));
    }
    let mut text = null_mut();
    // SAFETY: sid contains a successful OS-returned SID; Windows allocates text.
    if unsafe { ConvertSidToStringSidW(sid.as_ptr().cast_mut().cast(), &mut text) } == 0 {
        return Err(os_error("format Windows service account SID"));
    }
    let value = (|| {
        let mut length = 0usize;
        // SAFETY: the OS returns a NUL-terminated SID string, well below this bound.
        while length < 1024 && unsafe { *text.add(length) } != 0 {
            length += 1;
        }
        if length == 1024 {
            return Err("Windows SID text exceeds its bound".into());
        }
        Ok(String::from_utf16(unsafe {
            std::slice::from_raw_parts(text, length)
        })?)
    })();
    // SAFETY: ConvertSidToStringSidW requires LocalFree for this allocated string.
    unsafe {
        LocalFree(text.cast());
    }
    value
}

pub fn validate_account(account: &str, sid: &str) -> Result<()> {
    if account.is_empty() || account.contains(['\0', '\r', '\n']) {
        return Err("Windows system service requires an explicit account".into());
    }
    if sid == "S-1-5-18" {
        return Err("LocalSystem is not allowed for client session workers; select a dedicated or named non-system account".into());
    }
    Ok(())
}

fn quote_argument(value: &str) -> String {
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in value.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        if c == '"' {
            out.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
        } else {
            out.extend(std::iter::repeat_n('\\', slashes));
        }
        slashes = 0;
        out.push(c);
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
}

pub fn install(
    account: &str,
    expected_sid: &str,
    password: Option<&str>,
    executable: &Path,
) -> Result<Value> {
    let sid = account_sid(account)?;
    if sid != expected_sid {
        return Err("Windows service account changed during installation".into());
    }
    validate_account(account, &sid)?;
    let no_password = matches!(sid.as_str(), "S-1-5-19" | "S-1-5-20")
        || account.to_ascii_lowercase().starts_with("nt service\\")
        || account.ends_with('$');
    if password.is_none() && !no_password {
        return Err("named Windows service accounts require --password-stdin; the password is passed directly to SCM and never written by ssmcp".into());
    }
    let exe = executable
        .to_str()
        .filter(|s| !s.contains(['\0', '\r', '\n']))
        .ok_or("service executable path must be valid UTF-8")?;
    let binpath = wide(&format!("{} service run --scm", quote_argument(exe)))?;
    let name = wide(NAME)?;
    let display = wide("ssmcp client session manager")?;
    let account_wide = wide(account)?;
    let mut password_wide = password.map(wide).transpose()?;
    let manager = manager(SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE)?;
    if service(&manager, SERVICE_QUERY_STATUS)?.is_some() {
        return Err("ssmcp system service is already installed; use restart or uninstall before replacing its account or binary".into());
    }
    // SAFETY: all arguments are NUL-terminated and live through this synchronous call.
    let handle = unsafe {
        CreateServiceW(
            manager.0,
            name.as_ptr(),
            display.as_ptr(),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            binpath.as_ptr(),
            null(),
            null_mut(),
            null(),
            account_wide.as_ptr(),
            password_wide
                .as_ref()
                .map_or(null(), |password| password.as_ptr()),
        )
    };
    if let Some(password) = &mut password_wide {
        password.fill(0);
    }
    if handle.is_null() {
        return Err(os_error("create ssmcp Windows service"));
    }
    let service = ServiceHandle(handle);
    let mut description = wide(DESCRIPTION)?;
    let description = SERVICE_DESCRIPTIONW {
        lpDescription: description.as_mut_ptr(),
    };
    let mut actions = [SC_ACTION {
        Type: SC_ACTION_RESTART,
        Delay: 5000,
    }];
    let recovery = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 86400,
        lpRebootMsg: null_mut(),
        lpCommand: null_mut(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    let failure_flag = SERVICE_FAILURE_ACTIONS_FLAG {
        fFailureActionsOnNonCrashFailures: 1,
    };
    // SAFETY: SCM copies these configuration buffers before returning.
    if unsafe {
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_DESCRIPTION,
            (&description as *const SERVICE_DESCRIPTIONW).cast(),
        )
    } == 0
        || unsafe {
            ChangeServiceConfig2W(
                service.0,
                SERVICE_CONFIG_FAILURE_ACTIONS,
                (&recovery as *const SERVICE_FAILURE_ACTIONSW).cast(),
            )
        } == 0
        || unsafe {
            ChangeServiceConfig2W(
                service.0,
                SERVICE_CONFIG_FAILURE_ACTIONS_FLAG,
                (&failure_flag as *const SERVICE_FAILURE_ACTIONS_FLAG).cast(),
            )
        } == 0
    {
        let error = os_error("configure ssmcp Windows service");
        // No worker has started; remove only the service created by this call.
        unsafe {
            DeleteService(service.0);
        }
        return Err(error);
    }
    start(&service)?;
    let status = wait_state(&service, SERVICE_RUNNING)?;
    Ok(
        json!({"manager":"windows-scm","scope":"system","state":state_name(status.dwCurrentState),
        "service":NAME,"owner":account,"pid":status.dwProcessId,"installed":true}),
    )
}

fn query(service: &ServiceHandle) -> Result<SERVICE_STATUS_PROCESS> {
    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut required = 0;
    // SAFETY: status has exactly the type and size requested with SC_STATUS_PROCESS_INFO.
    if unsafe {
        QueryServiceStatusEx(
            service.0,
            SC_STATUS_PROCESS_INFO,
            (&mut status as *mut SERVICE_STATUS_PROCESS).cast(),
            std::mem::size_of::<SERVICE_STATUS_PROCESS>() as u32,
            &mut required,
        )
    } == 0
    {
        return Err(os_error("query ssmcp Windows service status"));
    }
    Ok(status)
}

fn verify_owned(service: &ServiceHandle) -> Result<()> {
    let mut required = 0;
    // SAFETY: query only the necessary buffer length.
    unsafe {
        QueryServiceConfig2W(
            service.0,
            SERVICE_CONFIG_DESCRIPTION,
            null_mut(),
            0,
            &mut required,
        );
    }
    if required == 0 || required > 65536 {
        return Err("cannot verify ssmcp service ownership".into());
    }
    let mut bytes = vec![0usize; (required as usize).div_ceil(std::mem::size_of::<usize>())];
    // SAFETY: aligned buffer is at least the returned required size.
    if unsafe {
        QueryServiceConfig2W(
            service.0,
            SERVICE_CONFIG_DESCRIPTION,
            bytes.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(os_error("read ssmcp service ownership"));
    }
    // SAFETY: successful query populated a SERVICE_DESCRIPTIONW in aligned storage.
    let description = unsafe { &*bytes.as_ptr().cast::<SERVICE_DESCRIPTIONW>() }.lpDescription;
    if description.is_null() {
        return Err("service is not owned by ssmcp".into());
    }
    let start = bytes.as_ptr() as usize;
    let end = start + bytes.len() * std::mem::size_of::<usize>();
    let position = description as usize;
    if position < start || position >= end || position % 2 != 0 {
        return Err("invalid service ownership description".into());
    }
    let capacity = (end - position) / 2;
    // SAFETY: the description pointer has been bounded to the live output buffer.
    let text = unsafe { std::slice::from_raw_parts(description, capacity) };
    let length = text
        .iter()
        .position(|c| *c == 0)
        .ok_or("unterminated service description")?;
    if String::from_utf16(&text[..length])? != DESCRIPTION {
        return Err(
            "the existing Windows service is not owned by ssmcp; it was not changed".into(),
        );
    }
    Ok(())
}

fn start(service: &ServiceHandle) -> Result<()> {
    // SAFETY: service is live; this service takes no SCM start-time arguments.
    if unsafe { StartServiceW(service.0, 0, null()) } == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_SERVICE_ALREADY_RUNNING as i32) {
            return Err(format!("start ssmcp Windows service: {error}").into());
        }
    }
    Ok(())
}

fn stop(service: &ServiceHandle) -> Result<()> {
    if query(service)?.dwCurrentState == SERVICE_STOPPED {
        return Ok(());
    }
    let mut status = SERVICE_STATUS::default();
    // SAFETY: the initialized status buffer is valid, and SCM owns the handle.
    if unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) } == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_SERVICE_NOT_ACTIVE as i32) {
            return Err(format!("stop ssmcp Windows service: {error}").into());
        }
    }
    wait_state(service, SERVICE_STOPPED)?;
    Ok(())
}

fn wait_state(service: &ServiceHandle, expected: u32) -> Result<SERVICE_STATUS_PROCESS> {
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        let status = query(service)?;
        if status.dwCurrentState == expected {
            return Ok(status);
        }
        if expected == SERVICE_RUNNING && status.dwCurrentState == SERVICE_STOPPED {
            return Err(format!(
                "ssmcp service stopped during startup (Windows code {}, service code {})",
                status.dwWin32ExitCode, status.dwServiceSpecificExitCode
            )
            .into());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "ssmcp service transition timed out; current state {}",
                state_name(status.dwCurrentState)
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn state_name(state: u32) -> &'static str {
    match state {
        SERVICE_STOPPED => "stopped",
        SERVICE_START_PENDING => "starting",
        SERVICE_STOP_PENDING => "stopping",
        SERVICE_RUNNING => "running",
        SERVICE_CONTINUE_PENDING => "continuing",
        SERVICE_PAUSE_PENDING => "pausing",
        SERVICE_PAUSED => "paused",
        _ => "unknown",
    }
}

pub fn control(operation: &str) -> Result<Value> {
    let manager = manager(SC_MANAGER_CONNECT)?;
    let mut access = SERVICE_QUERY_STATUS | SERVICE_QUERY_CONFIG;
    match operation {
        "status" => {}
        "start" => access |= SERVICE_START,
        "stop" => access |= SERVICE_STOP,
        "restart" => access |= SERVICE_START | SERVICE_STOP,
        "uninstall" => access |= SERVICE_STOP | DELETE,
        _ => return Err("unknown Windows service operation".into()),
    }
    let Some(service) = service(&manager, access)? else {
        if matches!(operation, "status" | "stop" | "uninstall") {
            return Ok(
                json!({"manager":"windows-scm","scope":"system","state":"not-installed","service":NAME}),
            );
        }
        return Err("ssmcp system service is not installed".into());
    };
    if operation != "status" {
        verify_owned(&service)?;
    }
    match operation {
        "start" => {
            start(&service)?;
            wait_state(&service, SERVICE_RUNNING)?;
        }
        "stop" => stop(&service)?,
        "restart" => {
            stop(&service)?;
            start(&service)?;
            wait_state(&service, SERVICE_RUNNING)?;
        }
        "uninstall" => {
            stop(&service)?;
            // SAFETY: only the verified ssmcp service is marked for deletion.
            if unsafe { DeleteService(service.0) } == 0 {
                return Err(os_error("delete ssmcp Windows service"));
            }
            return Ok(
                json!({"manager":"windows-scm","scope":"system","state":"uninstalled","service":NAME}),
            );
        }
        _ => {}
    }
    let status = query(&service)?;
    Ok(
        json!({"manager":"windows-scm","scope":"system","state":state_name(status.dwCurrentState),
        "service":NAME,"pid":status.dwProcessId,"exit_code":status.dwWin32ExitCode,
        "service_exit_code":status.dwServiceSpecificExitCode}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn account_policy_rejects_system_sid_even_with_an_alias() {
        assert!(validate_account("AliasForSystem", "S-1-5-18").is_err());
        assert!(validate_account("NT AUTHORITY\\LocalService", "S-1-5-19").is_ok());
        assert!(validate_account("", "S-1-5-21-1").is_err());
    }
    #[test]
    fn service_binary_arguments_have_windows_escaping() {
        assert_eq!(
            quote_argument("C:\\Program Files\\ssmcp.exe"),
            "\"C:\\Program Files\\ssmcp.exe\""
        );
        assert_eq!(quote_argument("x\\\"y"), "\"x\\\\\\\"y\"");
        assert_eq!(quote_argument("C:\\tail\\"), "\"C:\\tail\\\\\"");
    }
}
