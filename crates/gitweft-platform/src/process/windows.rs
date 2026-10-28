use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};
use std::time::Duration;

use windows_sys::Win32::Foundation::{FALSE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_CONSOLE, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED, OpenThread,
    ResumeThread, THREAD_SUSPEND_RESUME,
};

/// What a command started: a job object holding the child and everything it goes on to
/// start.
///
/// The job doesn't kill on close: a finished git may leave behind a daemon it started on
/// purpose (git-credential-cache), which must live on. The job only matters for a cancel.
#[derive(Debug)]
pub struct ProcessTree {
    job: OwnedHandle,
}

/// Starts `command` in a job object of its own, so [`ProcessTree::kill`] can end everything
/// it goes on to start.
pub fn spawn_tree(command: &mut Command) -> io::Result<(Child, ProcessTree)> {
    // SAFETY: no attributes and no name; the call only creates a handle, checked below.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `job` is a fresh handle that nothing else owns.
    let tree = ProcessTree {
        job: unsafe { OwnedHandle::from_raw_handle(job) },
    };
    // Suspended until it is in the job, so nothing it starts can escape it. Without
    // CREATE_NO_WINDOW every console program we run would flash a console window.
    let mut child = command
        .creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW)
        .spawn()?;
    if let Err(err) = tree.adopt(&child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }
    Ok((child, tree))
}

impl ProcessTree {
    /// Ends every process in the job, then reaps the child. Windows has no polite signal
    /// for a console program without a console, so `grace` goes unused.
    pub fn kill(&self, child: &mut Child, _grace: Duration) {
        // SAFETY: the job handle is ours and open for as long as `self` lives.
        unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) };
        let _ = child.wait();
    }

    /// Puts the suspended `child` into the job and lets it run.
    fn adopt(&self, child: &Child) -> io::Result<()> {
        // SAFETY: both handles are open: the job's is ours, the process's belongs to
        // `child`, which outlives the call.
        let assigned =
            unsafe { AssignProcessToJobObject(self.job.as_raw_handle(), child.as_raw_handle()) };
        if assigned == FALSE {
            return Err(io::Error::last_os_error());
        }
        resume_threads(child.id())
    }
}

/// Starts one of the user's own programs, outside our job and with a console of its own if
/// it is a console program (GUI programs ignore that), so that it outlives us.
pub fn spawn_detached(command: &mut Command) -> io::Result<Child> {
    command
        .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NEW_CONSOLE)
        .spawn()
}

/// Resumes the threads of the suspended process `pid`. std hands out no thread handle, so
/// they are looked up in a snapshot of the system's threads.
fn resume_threads(pid: u32) -> io::Result<()> {
    // SAFETY: plain call; the process id is ignored for a thread snapshot.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `snapshot` is a fresh handle that nothing else owns.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry = THREADENTRY32 {
        dwSize: size_of::<THREADENTRY32>() as u32,
        cntUsage: 0,
        th32ThreadID: 0,
        th32OwnerProcessID: 0,
        tpBasePri: 0,
        tpDeltaPri: 0,
        dwFlags: 0,
    };
    let mut resumed = 0;
    // SAFETY: the snapshot is open and `entry` is a writable THREADENTRY32 whose dwSize
    // is set, as Thread32First and Thread32Next require.
    let mut more = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) } != FALSE;
    while more {
        if entry.th32OwnerProcessID == pid {
            resume_thread(entry.th32ThreadID)?;
            resumed += 1;
        }
        // SAFETY: as above.
        more = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) } != FALSE;
    }
    if resumed == 0 {
        return Err(io::Error::other(
            "the started process has no thread to resume",
        ));
    }
    Ok(())
}

fn resume_thread(id: u32) -> io::Result<()> {
    // SAFETY: plain call; the handle it returns is checked below.
    let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, FALSE, id) };
    if thread.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `thread` is a fresh handle that nothing else owns.
    let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
    // SAFETY: the handle is open and has THREAD_SUSPEND_RESUME access.
    if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::process::Stdio;
    use std::thread;
    use std::time::Instant;

    use windows_sys::Win32::System::JobObjects::{
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
        QueryInformationJobObject,
    };

    /// How many processes in the tree's job are still running.
    fn active_processes(tree: &ProcessTree) -> u32 {
        // SAFETY: the struct is plain integers, for which zero is a valid value.
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the job handle is open and `info` is as large as the size we pass.
        let found = unsafe {
            QueryInformationJobObject(
                tree.job.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                (&raw mut info).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(found, FALSE, "{}", io::Error::last_os_error());
        info.ActiveProcesses
    }

    /// A shell that starts a long-running child of its own, says so, and then runs another:
    /// only ending the job gets rid of all of them.
    #[allow(clippy::disallowed_methods)] // A test's own shell, not git.
    fn stubborn() -> Command {
        let mut command = Command::new("cmd");
        command
            .arg("/c")
            .raw_arg("start /b ping -n 30 127.0.0.1 & echo started & ping -n 30 127.0.0.1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    #[test]
    fn killing_the_tree_ends_the_whole_job() {
        let (mut child, tree) = spawn_tree(&mut stubborn()).unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        while !line.contains("started") {
            line.clear();
            assert_ne!(stdout.read_line(&mut line).unwrap(), 0, "ended early");
        }
        assert!(active_processes(&tree) >= 2);

        let started = Instant::now();
        tree.kill(&mut child, Duration::from_millis(100));
        // The read finishes only once nothing in the job holds the pipe open.
        let mut rest = Vec::new();
        stdout.read_to_end(&mut rest).unwrap();
        while active_processes(&tree) > 0 && started.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(active_processes(&tree), 0);
        assert!(started.elapsed() < Duration::from_secs(5), "took too long");
    }

    #[test]
    fn killing_a_finished_tree_is_harmless() {
        #[allow(clippy::disallowed_methods)] // A test's own shell, not git.
        let mut command = Command::new("cmd");
        command.args(["/c", "exit 3"]).stdin(Stdio::null());
        let (mut child, tree) = spawn_tree(&mut command).unwrap();
        let status = child.wait().unwrap();
        assert_eq!(crate::process::exit_code(status), Some(3));
        tree.kill(&mut child, Duration::from_millis(100));
        assert_eq!(crate::process::exit_code(child.wait().unwrap()), Some(3));
    }

    #[test]
    fn a_detached_program_runs_on_its_own() {
        #[allow(clippy::disallowed_methods)] // A test's own shell, not git.
        let mut command = Command::new("cmd");
        command.args(["/c", "exit 7"]).stdin(Stdio::null());
        let mut child = spawn_detached(&mut command).unwrap();
        let status = child.wait().unwrap();
        assert_eq!(crate::process::exit_code(status), Some(7));
    }

    #[test]
    fn a_tree_can_be_killed_from_another_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<ProcessTree>();
    }
}
