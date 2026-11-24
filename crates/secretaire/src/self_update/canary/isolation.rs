//! Apply a write allowlist before executing the candidate or any of its children.

use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

use anyhow::{ensure, Context, Result};
use tokio::process::Command;

#[repr(C)]
struct Ruleset {
    handled_access_fs: u64,
}

#[repr(C, packed)]
struct PathRule {
    allowed_access: u64,
    parent_fd: i32,
}

pub(super) fn configure(cmd: &mut Command, dir: &Path, project: &Path) -> Result<()> {
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<Ruleset>(),
            0,
            1u32,
        )
    };
    ensure!(
        abi >= 3,
        "試運転のファイル隔離には Landlock ABI 3 以降が必要だよ"
    );

    // WRITE_FILE, REMOVE_*, MAKE_*, REFER, and TRUNCATE.
    const WRITES: u64 = (1 << 1) | (0x7ff << 4);
    let attrs = Ruleset {
        handled_access_fs: WRITES,
    };
    let fd = syscall_result(unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attrs,
            size_of::<Ruleset>(),
            0u32,
        )
    })
    .context("試運転のファイル隔離を用意できない")?;
    let ruleset = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    // Keep the staged executable and PID marker outside the candidate's write scope.
    for name in ["home", "state", "config", "tmp"] {
        allow(&ruleset, &dir.join(name), WRITES)?;
    }
    allow(&ruleset, project, WRITES)?;
    // Child programs may open the null device directly.
    allow(&ruleset, Path::new("/dev/null"), 1 << 1)?;

    let parent = std::process::id() as libc::pid_t;
    // Only async-signal-safe system calls run between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            syscall_result(libc::setsid().into())?;
            syscall_result(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0).into())?;
            if libc::getppid() != parent {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            syscall_result(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0).into())?;
            syscall_result(libc::syscall(
                libc::SYS_landlock_restrict_self,
                ruleset.as_raw_fd(),
                0u32,
            ))?;
            Ok(())
        });
    }
    Ok(())
}

fn allow(ruleset: &OwnedFd, path: &Path, rights: u64) -> Result<()> {
    let file = File::open(path)?;
    let rule = PathRule {
        allowed_access: rights,
        parent_fd: file.as_raw_fd(),
    };
    syscall_result(unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset.as_raw_fd(),
            1u32,
            &rule,
            0u32,
        )
    })
    .with_context(|| format!("試運転で {} への書き込みを設定できない", path.display()))?;
    Ok(())
}

fn syscall_result(result: libc::c_long) -> io::Result<libc::c_long> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

// The server owns a session, and some of its commands use additional process
// groups for their own cancellation. Stop those groups before removing state.
pub(super) fn stop_session(leader: rustix::process::Pid) -> io::Result<()> {
    use rustix::process::{kill_process_group, Pid, Signal};
    let kill = |group| match kill_process_group(group, Signal::KILL) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(error) => Err(io::Error::from(error)),
    };
    kill(leader)?;
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        if !entry
            .file_name()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some((_, ids)) = stat.rsplit_once(')') else {
            continue;
        };
        // The first two fields after comm are state and parent PID.
        let mut fields = ids.split_whitespace().skip(2);
        let group = fields
            .next()
            .and_then(|value| value.parse().ok())
            .and_then(Pid::from_raw);
        let session = fields.next().and_then(|value| value.parse::<i32>().ok());
        if session == Some(leader.as_raw_pid()) {
            if let Some(group) = group.filter(|group| *group != leader) {
                kill(group)?;
            }
        }
    }
    Ok(())
}
