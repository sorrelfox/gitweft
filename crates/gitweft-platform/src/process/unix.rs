use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

/// How often [`ProcessTree::kill`] looks whether the child has gone.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// What a command started: the process group, whose id is the child's own pid.
#[derive(Debug)]
pub struct ProcessTree {
    pid: u32,
}

/// Starts `command` in a group of its own, so [`ProcessTree::kill`] can end everything it
/// goes on to start.
pub fn spawn_tree(command: &mut Command) -> io::Result<(Child, ProcessTree)> {
    // Own process group, so cancellation also reaches ssh, hooks and helpers.
    let child = command.process_group(0).spawn()?;
    let tree = ProcessTree { pid: child.id() };
    Ok((child, tree))
}

impl ProcessTree {
    /// SIGTERM the whole process group, then SIGKILL it after `grace`, or as soon as the
    /// child itself has gone: a process it started that ignores SIGTERM (a hook, a
    /// credential helper) would keep stderr open, and its reader waiting, forever (an
    /// earlier version sent the SIGKILL only to a child that outlived the grace period).
    /// The child isn't reaped until then, so its pid, the group's id, can't be anybody
    /// else's meanwhile.
    pub fn kill(&self, child: &mut Child, grace: Duration) {
        let Ok(pid) = i32::try_from(self.pid) else {
            return;
        };
        // SAFETY: plain syscall on the process group we created with process_group(0).
        unsafe { libc::kill(-pid, libc::SIGTERM) };
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline && !exited_unreaped(pid) {
            thread::sleep(POLL_INTERVAL);
        }
        // SAFETY: as above; the leader is at worst a zombie, so the group is still ours.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
        let _ = child.wait();
    }
}

/// Starts one of the user's own programs, in a process group of its own so that it neither
/// takes our terminal input nor dies with us.
pub fn spawn_detached(command: &mut Command) -> io::Result<Child> {
    command.process_group(0).spawn()
}

/// Whether our child `pid` has exited, leaving it unreaped (`WNOWAIT`).
fn exited_unreaped(pid: i32) -> bool {
    let Ok(id) = libc::id_t::try_from(pid) else {
        return true;
    };
    // SAFETY: a zeroed siginfo_t is valid; waitid only reads our own child's state here.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    // SAFETY: as above; `info` outlives the call.
    let found = unsafe { libc::waitid(libc::P_PID, id, &mut info, flags) };
    found == 0 && reported_pid(&info) != 0
}

/// The pid waitid reported, which stays the zero we gave it when no child had anything to
/// report. Linux hides si_pid behind an accessor; the BSDs have it as a field.
#[cfg(target_os = "linux")]
fn reported_pid(info: &libc::siginfo_t) -> libc::pid_t {
    // SAFETY: si_pid is set by waitid (0 when the child is still running).
    unsafe { info.si_pid() }
}

#[cfg(not(target_os = "linux"))]
fn reported_pid(info: &libc::siginfo_t) -> libc::pid_t {
    info.si_pid
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// A shell that starts a child of its own and ignores SIGTERM: only ending the group
    /// gets rid of both.
    #[allow(clippy::disallowed_methods)] // A test's own shell, not git.
    fn stubborn() -> Command {
        let mut command = Command::new("sh");
        command
            .args(["-c", "trap '' TERM; sleep 30 & sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    #[test]
    fn killing_the_tree_ends_the_whole_group() {
        let (mut child, tree) = spawn_tree(&mut stubborn()).unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let started = Instant::now();
        tree.kill(&mut child, Duration::from_millis(100));
        // Both ends of the pipe are shut, so the read finishes: nothing in the group is
        // left holding it open.
        let mut rest = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut rest).unwrap();
        assert!(rest.is_empty());
        assert!(started.elapsed() < Duration::from_secs(5), "took too long");
    }

    #[test]
    fn a_detached_program_runs_on_its_own() {
        #[allow(clippy::disallowed_methods)] // A test's own shell, not git.
        let mut command = Command::new("sh");
        command.args(["-c", "exit 7"]).stdin(Stdio::null());
        let mut child = spawn_detached(&mut command).unwrap();
        let status = child.wait().unwrap();
        assert_eq!(crate::process::exit_code(status), Some(7));
    }

    #[test]
    fn a_signal_counts_as_a_code() {
        #[allow(clippy::disallowed_methods)] // A test's own shell, not git.
        let mut command = Command::new("sh");
        command.args(["-c", "kill -9 $$"]).stdin(Stdio::null());
        let mut child = spawn_detached(&mut command).unwrap();
        let status = child.wait().unwrap();
        assert_eq!(crate::process::exit_code(status), Some(128 + libc::SIGKILL));
    }
}
