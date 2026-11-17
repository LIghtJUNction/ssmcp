//! Windows process ownership and protected, current-user filesystem access.
use std::{
    ffi::c_void,
    fs::{File, OpenOptions},
    io,
    os::windows::{
        ffi::OsStrExt,
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
};
use windows_sys::Win32::{
    Foundation::{ERROR_ALREADY_EXISTS, ERROR_LOCK_VIOLATION, LocalFree},
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
        },
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetSecurityDescriptorControl,
        GetTokenInformation, IsWellKnownSid, OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED,
        SECURITY_ATTRIBUTES, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER, TokenOwner, TokenUser,
        WinLocalSystemSid,
    },
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        GetFileInformationByHandle, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    },
    System::{
        IO::{OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0},
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

struct LocalMemory(*mut c_void);
impl Drop for LocalMemory {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

struct Identity {
    user: Vec<usize>,
    owner: Vec<usize>,
}
impl Identity {
    fn current() -> io::Result<Self> {
        let mut raw = std::ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = unsafe { OwnedHandle::from_raw_handle(raw) };
        fn query(token: &OwnedHandle, class: i32) -> io::Result<Vec<usize>> {
            let mut needed = 0;
            unsafe {
                GetTokenInformation(
                    token.as_raw_handle(),
                    class,
                    std::ptr::null_mut(),
                    0,
                    &mut needed,
                );
            }
            if needed == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut buffer = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
            if unsafe {
                GetTokenInformation(
                    token.as_raw_handle(),
                    class,
                    buffer.as_mut_ptr().cast(),
                    needed,
                    &mut needed,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(buffer)
        }
        Ok(Self {
            user: query(&token, TokenUser)?,
            owner: query(&token, TokenOwner)?,
        })
    }

    fn user_sid(&self) -> *mut c_void {
        unsafe { (*(self.user.as_ptr().cast::<TOKEN_USER>())).User.Sid }
    }

    fn owner_sid(&self) -> *mut c_void {
        unsafe { (*(self.owner.as_ptr().cast::<TOKEN_OWNER>())).Owner }
    }

    fn sid_string(&self) -> io::Result<String> {
        let mut sid = std::ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(self.user_sid(), &mut sid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let _guard = LocalMemory(sid.cast());
        let mut length = 0;
        while unsafe { *sid.add(length) } != 0 {
            length += 1;
        }
        String::from_utf16(unsafe { std::slice::from_raw_parts(sid, length) })
            .map_err(io::Error::other)
    }
}

fn verify_private(file: &File, identity: &Identity) -> io::Result<()> {
    let mut owner = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let code = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    let _guard = LocalMemory(descriptor);
    if file.metadata()?.is_dir() {
        let mut control = 0;
        let mut revision = 0;
        if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if control & SE_DACL_PROTECTED == 0 {
            return Err(io::Error::other(
                "output directory must have a protected private access list",
            ));
        }
    }
    if owner.is_null()
        || dacl.is_null()
        || unsafe {
            EqualSid(owner, identity.user_sid()) == 0 && EqualSid(owner, identity.owner_sid()) == 0
        }
    {
        return Err(io::Error::other(
            "output storage is not owned by the current user",
        ));
    }
    if unsafe { (*dacl).AceCount } == 0 {
        return Err(io::Error::other(
            "output storage has no private access entries",
        ));
    }
    for index in 0..unsafe { (*dacl).AceCount } as u32 {
        let mut ace = std::ptr::null_mut();
        if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let header = unsafe { &*(ace.cast::<ACE_HEADER>()) };
        // Only ordinary allow entries for this user and SYSTEM are accepted.
        // Reject broad inherited ACLs, object/callback ACEs, and unknown layouts.
        if header.AceType != 0
            || (header.AceSize as usize) < std::mem::size_of::<ACCESS_ALLOWED_ACE>()
        {
            return Err(io::Error::other(
                "output storage has an unsupported access rule",
            ));
        }
        let allowed = unsafe { &*(ace.cast::<ACCESS_ALLOWED_ACE>()) };
        let sid = (&allowed.SidStart as *const u32).cast_mut().cast();
        if unsafe {
            EqualSid(sid, identity.user_sid()) == 0 && IsWellKnownSid(sid, WinLocalSystemSid) == 0
        } {
            return Err(io::Error::other(
                "output storage grants access to another identity",
            ));
        }
    }
    Ok(())
}

pub(crate) fn create_private_directory(path: &Path) -> io::Result<()> {
    let identity = Identity::current()?;
    let sid = identity.sid_string()?;
    // A protected DACL does not inherit broad permissions from a shared temp
    // directory. Child files inherit only the current user and SYSTEM grants.
    let descriptor: Vec<u16> = format!("O:{sid}D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;{sid})\0")
        .encode_utf16()
        .collect();
    let mut security = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor.as_ptr(),
            SDDL_REVISION_1,
            &mut security,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let _security = LocalMemory(security);
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security,
        bInheritHandle: 0,
    };
    let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { CreateDirectoryW(path_wide.as_ptr(), &attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn private_directory(path: &Path) -> io::Result<cap_std::fs::Dir> {
    match create_private_directory(path) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32) => {}
        Err(error) => return Err(error),
    }
    let identity = Identity::current()?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let info = information(&file)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(io::Error::other(
            "output root must be a directory, never a link or reparse point",
        ));
    }
    verify_private(&file, &identity)?;
    Ok(cap_std::fs::Dir::from_std_file(file))
}

fn information(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
}

pub(crate) fn verify_file(file: &File) -> io::Result<()> {
    let info = information(file)?;
    if info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
        || info.nNumberOfLinks != 1
    {
        return Err(io::Error::other(
            "output artifact must be a singly linked regular file",
        ));
    }
    verify_private(file, &Identity::current()?)
}

pub(crate) fn lock(file: &File, nonblocking: bool) -> io::Result<()> {
    // Windows byte-range locks are mandatory. Lock one byte beyond the maximum
    // artifact size so active captures remain readable, like Unix flock.
    let mut overlap = OVERLAPPED {
        Anonymous: OVERLAPPED_0 {
            Anonymous: OVERLAPPED_0_0 {
                Offset: u32::MAX,
                OffsetHigh: 0,
            },
        },
        ..Default::default()
    };
    let flags = LOCKFILE_EXCLUSIVE_LOCK
        | if nonblocking {
            LOCKFILE_FAIL_IMMEDIATELY
        } else {
            0
        };
    if unsafe { LockFileEx(file.as_raw_handle(), flags, 0, 1, 0, &mut overlap) } == 0 {
        let error = io::Error::last_os_error();
        // Match flock's nonblocking contention contract. Other errors must
        // remain errors: a permissions/I/O failure is not proof of a held lease.
        if nonblocking && error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
            Err(io::Error::new(io::ErrorKind::WouldBlock, error))
        } else {
            Err(error)
        }
    } else {
        Ok(())
    }
}

pub(crate) struct WindowsJob(std::os::windows::io::OwnedHandle);

pub(crate) fn resume_process(pid: u32) -> io::Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        System::{Diagnostics::ToolHelp::*, Threading::*},
    };
    // The child starts suspended so even a fast batch wrapper cannot launch
    // descendants before it belongs to the kill-on-close job.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) } != 0;
    while found {
        if entry.th32OwnerProcessID == pid {
            let raw = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if raw.is_null() {
                return Err(io::Error::last_os_error());
            }
            let thread = unsafe { OwnedHandle::from_raw_handle(raw) };
            if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            return Ok(());
        }
        found = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) } != 0;
    }
    Err(io::Error::other(
        "suspended child primary thread was not found",
    ))
}

impl WindowsJob {
    pub fn new() -> io::Result<Self> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle};
        use windows_sys::Win32::System::JobObjects::*;
        // SAFETY: null parameters create an unnamed job with the caller's security descriptor.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Self(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle) });
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the job and correctly sized limit structure remain live for this call.
        if unsafe {
            SetInformationJobObject(
                job.0.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }
    pub fn assign(&self, process: std::os::windows::io::RawHandle) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: both handles are borrowed live kernel objects.
        if unsafe {
            windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(
                self.0.as_raw_handle(),
                process,
            )
        } == 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    pub(crate) fn attach(process: std::os::windows::io::RawHandle) -> io::Result<Self> {
        let job = Self::new()?;
        job.assign(process)?;
        Ok(job)
    }
    pub fn terminate(&self) {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: this unnamed job only owns children started by this client.
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0.as_raw_handle(), 1);
        }
    }
}
