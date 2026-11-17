//! Own a process tree so inherited pipes cannot outlive their transport.
//! The optional `process` feature keeps OS-specific dependencies out of the
//! SDK's default ABI-only build.
use std::process::{Child, Command};

#[cfg(windows)]
#[derive(Debug)]
pub struct ProcessTree(std::os::windows::io::OwnedHandle);

#[cfg(windows)]
impl ProcessTree {
    pub fn prepare(command: &mut Command) {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};
        // Assign the job before server code can spawn descendants or inherit pipes.
        command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    }
    pub fn attach(child: &Child) -> Result<Self, String> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::JobObjects::*;
        // SAFETY: Windows creates an owned job handle; the initialized limits
        // and Child's live process handle remain valid throughout these calls.
        // OwnedHandle closes the job on every success or error path.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let tree = Self(OwnedHandle::from_raw_handle(job));
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of_val(&limits) as u32,
            ) == 0
                || AssignProcessToJobObject(job, child.as_raw_handle()) == 0
            {
                return Err(format!(
                    "protect MCP process tree: {}",
                    std::io::Error::last_os_error()
                ));
            }
            resume_child(child.id())?;
            Ok(tree)
        }
    }
    pub fn kill(&self) {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: self owns the job handle for the duration of this call.
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0.as_raw_handle(), 1);
        }
    }
}

#[cfg(unix)]
#[derive(Debug)]
pub struct ProcessTree(i32);
#[cfg(unix)]
impl ProcessTree {
    pub fn prepare(command: &mut Command) {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    pub fn attach(child: &Child) -> Result<Self, String> {
        Ok(Self(child.id() as i32))
    }
    pub fn kill(&self) {
        // SAFETY: kill accepts a scalar process-group ID and signal; no Rust
        // memory is exposed. prepare creates the child's dedicated group.
        unsafe {
            libc::kill(-self.0, libc::SIGKILL);
        }
    }
}

/// Resume a process created with `CREATE_SUSPENDED`, once it is in its job.
#[cfg(windows)]
pub fn resume_child(pid: u32) -> Result<(), String> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};
    // SAFETY: successful OS calls return owned handles, and enumeration writes
    // only to the initialized local entry. RAII closes every snapshot/thread.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(format!(
                "snapshot child thread: {}",
                std::io::Error::last_os_error()
            ));
        }
        let snapshot = OwnedHandle::from_raw_handle(snapshot);
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of_val(&entry) as u32;
        let mut found = Thread32First(snapshot.as_raw_handle(), &mut entry);
        let mut result = Err("child primary thread not found".into());
        while found != 0 {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if thread.is_null() {
                    result = Err(format!(
                        "open child thread: {}",
                        std::io::Error::last_os_error()
                    ));
                } else {
                    let thread = OwnedHandle::from_raw_handle(thread);
                    let resumed = ResumeThread(thread.as_raw_handle());
                    result = if resumed == u32::MAX {
                        Err(format!(
                            "resume MCP thread: {}",
                            std::io::Error::last_os_error()
                        ))
                    } else {
                        Ok(())
                    };
                }
                break;
            }
            found = Thread32Next(snapshot.as_raw_handle(), &mut entry);
        }
        result
    }
}

/// Query whether a PID still names a running process. Access failures are
/// reported rather than treating an unqueryable process as dead.
#[cfg(windows)]
pub fn process_is_running(pid: u32) -> std::io::Result<bool> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: these APIs take scalar IDs and valid owned handles; the exit
    // code pointer is a writable local, and RAII closes the process handle.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            let error = std::io::Error::last_os_error();
            return if error.raw_os_error() == Some(87) {
                Ok(false)
            } else {
                Err(error)
            };
        }
        let process = OwnedHandle::from_raw_handle(process);
        let mut code = 0;
        if GetExitCodeProcess(process.as_raw_handle(), &mut code) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(code == 259) // STILL_ACTIVE
    }
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn queries_live_and_missing_processes() {
        assert!(super::process_is_running(std::process::id()).unwrap());
        assert!(!super::process_is_running(0).unwrap());
    }
}
