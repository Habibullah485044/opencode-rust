//! Windows: every shell Habi starts runs in a Job Object. Its descendants
//! join the job by themselves, so killing the job ends all of them, where
//! `taskkill /T` (which follows parent ids) missed Git Bash's forked
//! children on a busy machine and a dev server outlived Habi. The jobs close
//! when Habi exits for any reason (window closed, crash), and the system
//! then ends what is left in them (`KILL_ON_JOB_CLOSE`).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

struct Job(HANDLE);
// SAFETY: a job handle is a kernel object handle; it may be used and
// closed from any thread.
unsafe impl Send for Job {}

impl Job {
    fn active(&self) -> u32 {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: `info` is the struct this information class fills.
        let ok = unsafe {
            QueryInformationJobObject(
                Some(self.0),
                JobObjectBasicAccountingInformation,
                &mut info as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of_val(&info) as u32,
                None,
            )
        };
        if ok.is_ok() { info.ActiveProcesses } else { 0 }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateJobObjectW and is closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn jobs() -> &'static Mutex<HashMap<u32, Job>> {
    static J: OnceLock<Mutex<HashMap<u32, Job>>> = OnceLock::new();
    J.get_or_init(Default::default)
}

/// Put the process `pid` (just started, handle `process`) in a new job.
pub fn adopt(pid: u32, process: std::os::windows::io::RawHandle) {
    // SAFETY: plain Win32 calls with valid arguments; the info struct is
    // the one the information class expects.
    let job = unsafe {
        let Ok(h) = CreateJobObjectW(None, windows::core::PCWSTR::null()) else { return };
        let job = Job(h);
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set = SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const std::ffi::c_void,
            std::mem::size_of_val(&info) as u32,
        );
        if set.is_err() || AssignProcessToJobObject(job.0, HANDLE(process)).is_err() {
            return;
        }
        job
    };
    let mut map = jobs().lock().unwrap_or_else(|e| e.into_inner());
    // Jobs whose processes have all ended are not needed any more.
    map.retain(|_, j| j.active() > 0);
    map.insert(pid, job);
}

/// End everything in the job of `pid`. False when it has none.
pub fn kill(pid: u32) -> bool {
    let map = jobs().lock().unwrap_or_else(|e| e.into_inner());
    match map.get(&pid) {
        // SAFETY: a live job handle from `adopt`.
        Some(job) => unsafe { TerminateJobObject(job.0, 1).is_ok() },
        None => false,
    }
}
