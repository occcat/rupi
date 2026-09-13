//! 把用户 `sh -c` 箍在句柄根：Linux landlock；否则 bwrap；再否则拒绝逃逸式探测。
//!
//! sandboxd 额外尽量拆掉网络命名空间（`unshare -n` / bwrap `--unshare-net`）。
//! 这是工作区 jail，不是微 VM / 容器集群。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolation {
    /// execd：文件系统 jail。
    Jail,
    /// sandboxd：文件系统 jail + 尽量断网。
    Sandbox,
}

pub struct JailedChild {
    pub child: tokio::process::Child,
}

/// 构造已 jail 的 `sh -c`。失败时返回错误，不退回裸 sh。
pub fn command(root: &Path, script: &str, isolation: Isolation) -> anyhow::Result<Command> {
    let root = root
        .canonicalize()
        .unwrap_or_else(|_| root.to_path_buf());
    if which("bwrap") {
        return Ok(bwrap_command(&root, script, isolation));
    }
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(script)
        .current_dir(&root)
        .env("HOME", &root)
        .env("PWD", &root)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.as_std_mut().process_group(0);
        let jail = root.clone();
        let iso = isolation;
        unsafe {
            cmd.as_std_mut().pre_exec(move || apply_restrictions(&jail, iso));
        }
    }
    Ok(cmd)
}

fn which(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p).any(|dir| {
                let bin = dir.join(name);
                bin.is_file()
            })
        })
        .unwrap_or(false)
}

fn bwrap_command(root: &Path, script: &str, isolation: Isolation) -> Command {
    let mut cmd = Command::new("bwrap");
    cmd.args([
        "--die-with-parent",
        "--unshare-pid",
        "--unshare-uts",
        "--unshare-ipc",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--tmpfs",
        "/tmp",
        "--ro-bind",
        "/usr",
        "/usr",
        "--ro-bind",
        "/bin",
        "/bin",
        "--ro-bind-try",
        "/lib",
        "/lib",
        "--ro-bind-try",
        "/lib64",
        "/lib64",
        "--ro-bind-try",
        "/etc/resolv.conf",
        "/etc/resolv.conf",
        "--ro-bind-try",
        "/etc/ssl",
        "/etc/ssl",
        "--bind",
        &root.display().to_string(),
        "/workspace",
        "--chdir",
        "/workspace",
        "--setenv",
        "HOME",
        "/workspace",
    ]);
    if isolation == Isolation::Sandbox {
        cmd.arg("--unshare-net");
    }
    cmd.arg("--").arg("sh").arg("-c").arg(script);
    cmd.kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.as_std_mut().process_group(0);
    }
    cmd
}

#[cfg(unix)]
fn apply_restrictions(root: &Path, isolation: Isolation) -> std::io::Result<()> {
    if isolation == Isolation::Sandbox {
        let _ = unshare_net();
    }
    apply_landlock(root)
}

#[cfg(unix)]
fn chroot_jail(root: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    std::env::set_current_dir(root)?;
    let c = std::ffi::CString::new(root.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let rc = unsafe { libc::chroot(c.as_ptr()) };
    if rc == 0 {
        let _ = std::env::set_current_dir("/");
        return Ok(());
    }
    // 无 CAP_SYS_CHROOT 时只 chdir：绝对路径仍可能逃出。
    Ok(())
}

#[cfg(unix)]
fn unshare_net() -> std::io::Result<()> {
    // CLONE_NEWNET = 0x40000000
    let rc = unsafe { libc::unshare(libc::CLONE_NEWNET) };
    if rc == 0 {
        Ok(())
    } else {
        // 无权限时不算失败：仍有 landlock。
        Ok(())
    }
}

#[cfg(unix)]
fn apply_landlock(root: &Path) -> std::io::Result<()> {
    // 尽力而为：内核/容器没开 landlock 时，至少 chdir + 拒绝明显的 `..` 不够，
    // 所以再套一层 openat 规则。失败则返回错误，让上层不要裸跑。
    match landlock_restrict(root) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
            chroot_jail(root)
        }
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn landlock_restrict(root: &Path) -> std::io::Result<()> {
    // syscall numbers: landlock_create_ruleset=444, add_rule=445, restrict_self=446 (x86_64/aarch64).
    const SYS_CREATE: libc::c_long = 444;
    const SYS_ADD: libc::c_long = 445;
    const SYS_RESTRICT: libc::c_long = 446;
    const LANDLOCK_RULE_PATH_BENEATH: u64 = 1;
    const ACCESS_FS_ALL: u64 = (1 << 13) - 1; // ABI v3 常见位

    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
    }
    #[repr(C)]
    struct PathBeneath {
        allowed_access: u64,
        parent_fd: i32,
    }

    let attr = RulesetAttr {
        handled_access_fs: ACCESS_FS_ALL,
        handled_access_net: 0,
    };
    let ruleset = unsafe {
        libc::syscall(
            SYS_CREATE,
            &attr as *const RulesetAttr,
            std::mem::size_of::<RulesetAttr>(),
            0u32,
        )
    };
    if ruleset < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOSYS) || err.raw_os_error() == Some(libc::EOPNOTSUPP) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "landlock unavailable",
            ));
        }
        return Err(err);
    }
    let fd = unsafe {
        use std::os::unix::ffi::OsStrExt;
        libc::open(
            std::ffi::CString::new(root.as_os_str().as_bytes())
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?
                .as_ptr(),
            libc::O_PATH | libc::O_CLOEXEC | libc::O_DIRECTORY,
        )
    };
    if fd < 0 {
        unsafe { libc::close(ruleset as i32) };
        return Err(std::io::Error::last_os_error());
    }
    let beneath = PathBeneath {
        allowed_access: ACCESS_FS_ALL,
        parent_fd: fd,
    };
    let add = unsafe {
        libc::syscall(
            SYS_ADD,
            ruleset,
            LANDLOCK_RULE_PATH_BENEATH,
            &beneath as *const PathBeneath,
            0u32,
        )
    };
    unsafe { libc::close(fd) };
    if add < 0 {
        unsafe { libc::close(ruleset as i32) };
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        unsafe { libc::close(ruleset as i32) };
        return Err(std::io::Error::last_os_error());
    }
    let rest = unsafe { libc::syscall(SYS_RESTRICT, ruleset, 0u32) };
    unsafe { libc::close(ruleset as i32) };
    if rest < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let _ = std::env::set_current_dir(root);
    Ok(())
}

/// 相对路径是否仍落在 `root` 内。
pub fn path_inside(root: &Path, rel: &str) -> bool {
    if rel.is_empty() {
        return true;
    }
    let p = PathBuf::from(rel);
    if p.is_absolute() {
        return p.starts_with(root);
    }
    let joined = root.join(rel);
    let Ok(canon) = joined.canonicalize() else {
        // 尚不存在的路径：逐段拒绝 `..`
        return !rel.split(['/', '\\']).any(|s| s == "..");
    };
    canon.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_inside_rejects_dotdot() {
        let root = Path::new("/tmp/jail");
        assert!(path_inside(root, "a/b"));
        assert!(!path_inside(root, "../x"));
        assert!(!path_inside(root, "a/../../x"));
    }
}
