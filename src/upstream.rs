//! Terminate the upstream pipeline producer on quit.
//!
//! `docker compose logs -f | logtui` — when logtui quits, the producer keeps
//! running until its next write hits the broken pipe (which may be never for
//! a quiet stream), and the shell waits for it, so the prompt doesn't come
//! back without a Ctrl+C. Best-effort fix: on quit, terminate the pipeline
//! siblings ourselves. Every failure mode here is "the old behavior", so all
//! errors are ignored.

/// Unix: an interactive shell puts the whole pipeline into one process group
/// of its own, so signalling our group reaches exactly the siblings. Guard:
/// if we share the group with our parent (no job control — e.g. inside a
/// plain script), do nothing rather than kill the script.
#[cfg(unix)]
pub fn terminate() {
    unsafe {
        let own_group = libc::getpgrp();
        if libc::getpgid(libc::getppid()) == own_group {
            return;
        }
        // Shield ourselves, then signal the group (0 = caller's group).
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
        libc::kill(0, libc::SIGTERM);
    }
}

/// Windows: there are no pipeline process groups, so approximate "pipeline
/// sibling" as: attached to our console AND spawned by the same parent (the
/// shell). That matches `producer | logtui` under cmd, PowerShell, and MSYS
/// bash, and excludes the shell itself and unrelated console processes.
#[cfg(windows)]
pub fn terminate() {
    use std::collections::HashMap;

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::GetConsoleProcessList;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, TerminateProcess, PROCESS_TERMINATE,
    };

    unsafe {
        let mut pids = vec![0u32; 64];
        let mut n = GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) as usize;
        if n > pids.len() {
            pids.resize(n, 0);
            n = GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) as usize;
        }
        if n == 0 || n > pids.len() {
            return;
        }
        pids.truncate(n);

        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return;
        }
        let mut parent_of: HashMap<u32, u32> = HashMap::new();
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                parent_of.insert(entry.th32ProcessID, entry.th32ParentProcessID);
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);

        let me = std::process::id();
        let Some(&shell) = parent_of.get(&me) else {
            return;
        };
        for &pid in &pids {
            if pid == me || pid == shell || parent_of.get(&pid) != Some(&shell) {
                continue;
            }
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !handle.is_null() {
                TerminateProcess(handle, 1);
                CloseHandle(handle);
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
pub fn terminate() {}
