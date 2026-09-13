//! 把用户 `sh -c` 箍在句柄根。
//!
//! - Linux：bwrap（若在 PATH）或 landlock；sandboxd 尽量 `unshare(CLONE_NEWNET)`。
//! - macOS：`sandbox-exec` 约束到句柄根。
//! - 其他 Unix：chdir，并尽力 `chroot`。
//!
//! jail 必须能执行 `/bin/sh`（只读挂上 `/usr` `/bin` `/lib` 等），但不能读邻居卷。
//! 这不是微 VM / 容器集群。

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

/// 构造已 jail 的 `sh -c`。
pub fn command(root: &Path, script: &str, isolation: Isolation) -> anyhow::Result<Command> {
    let root = root
        .canonicalize()
        .unwrap_or_else(|_| root.to_path_buf());
    if which("bwrap") {
        return Ok(bwrap_command(&root, script, isolation));
    }
    #[cfg(target_os = "macos")]
    if which("sandbox-exec") {
        return Ok(macos_sandbox_command(&root, script));
    }
    Ok(plain_jailed_sh(&root, script, isolation))
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

fn apply_stdio(cmd: &mut Command, root: &Path) {
    cmd.current_dir(root)
        .env("HOME", root)
        .env("PWD", root)
        .env("TMPDIR", root)
        .env("TMP", root)
        .env("TEMP", root)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
}

fn plain_jailed_sh(root: &Path, script: &str, isolation: Isolation) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script);
    apply_stdio(&mut cmd, root);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.as_std_mut().process_group(0);
        let jail = root.to_path_buf();
        unsafe {
            cmd.as_std_mut()
                .pre_exec(move || apply_restrictions(&jail, isolation));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = isolation;
    }
    cmd
}

fn bwrap_command(root: &Path, script: &str, isolation: Isolation) -> Command {
    let root_s = root.display().to_string();
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
        &root_s,
        "/workspace",
        "--chdir",
        "/workspace",
        "--setenv",
        "HOME",
        "/workspace",
        "--setenv",
        "TMPDIR",
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

#[cfg(target_os = "macos")]
fn macos_sandbox_command(root: &Path, script: &str) -> Command {
    let root_s = root.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
    // 允许跑 sh/cat，并写管道 / /dev/fd（否则 echo 成功但 stdout 是空的）。
    // 禁止读句柄根以外的用户文件。不要放行整个 /tmp、/private/var/folders、/Users。
    let profile = format!(
        r#"(version 1)
(deny default)
(allow process-exec)
(allow process-fork)
(allow signal)
(allow sysctl-read)
(allow mach-lookup)
(allow file-ioctl)
(allow file-read-metadata)
(allow file-read*
  (subpath "/usr")
  (subpath "/bin")
  (subpath "/sbin")
  (subpath "/opt")
  (subpath "/System")
  (subpath "/Library")
  (subpath "/dev")
  (subpath "/etc")
  (subpath "/private/etc")
  (subpath "/private/var/db")
)
(allow file-write* file-ioctl file-read*
  (literal "/dev/null")
  (literal "/dev/zero")
  (literal "/dev/stdout")
  (literal "/dev/stderr")
  (literal "/dev/stdin")
  (literal "/dev/tty")
  (subpath "/dev/fd")
)
(allow file-write-data file-ioctl
  (vnode-type PIPE)
  (vnode-type SOCKET)
)
(allow file-read* file-write*
  (subpath "{root_s}")
)
"#
    );
    let mut cmd = Command::new("sandbox-exec");
    cmd.arg("-p").arg(profile).arg("sh").arg("-c").arg(script);
    apply_stdio(&mut cmd, root);
    use std::os::unix::process::CommandExt as _;
    cmd.as_std_mut().process_group(0);
    cmd
}

#[cfg(target_os = "linux")]
fn apply_restrictions(root: &Path, isolation: Isolation) -> std::io::Result<()> {
    if isolation == Isolation::Sandbox {
        let _ = unshare_net();
    }
    apply_landlock(root)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn apply_restrictions(root: &Path, _isolation: Isolation) -> std::io::Result<()> {
    chroot_jail(root)
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
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn unshare_net() -> std::io::Result<()> {
    let rc = unsafe { libc::unshare(libc::CLONE_NEWNET) };
    let _ = rc;
    Ok(())
}

#[cfg(target_os = "linux")]
fn apply_landlock(root: &Path) -> std::io::Result<()> {
    match landlock_restrict(root) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => chroot_jail(root),
        Err(e) => Err(e),
    }
}

#[cfg(target_os = "linux")]
fn landlock_restrict(root: &Path) -> std::io::Result<()> {
    // landlock_create_ruleset=444, add_rule=445, restrict_self=446（x86_64 / aarch64）。
    const SYS_CREATE: libc::c_long = 444;
    const SYS_ADD: libc::c_long = 445;
    const SYS_RESTRICT: libc::c_long = 446;
    const LANDLOCK_RULE_PATH_BENEATH: u64 = 1;
    const ACCESS_EXECUTE: u64 = 1 << 0;
    const ACCESS_WRITE_FILE: u64 = 1 << 1;
    const ACCESS_READ_FILE: u64 = 1 << 2;
    const ACCESS_READ_DIR: u64 = 1 << 3;
    const ACCESS_REMOVE_DIR: u64 = 1 << 4;
    const ACCESS_REMOVE_FILE: u64 = 1 << 5;
    const ACCESS_MAKE_CHAR: u64 = 1 << 6;
    const ACCESS_MAKE_DIR: u64 = 1 << 7;
    const ACCESS_MAKE_REG: u64 = 1 << 8;
    const ACCESS_MAKE_SOCK: u64 = 1 << 9;
    const ACCESS_MAKE_FIFO: u64 = 1 << 10;
    const ACCESS_MAKE_BLOCK: u64 = 1 << 11;
    const ACCESS_MAKE_SYM: u64 = 1 << 12;
    const ACCESS_FS_RW: u64 = ACCESS_EXECUTE
        | ACCESS_WRITE_FILE
        | ACCESS_READ_FILE
        | ACCESS_READ_DIR
        | ACCESS_REMOVE_DIR
        | ACCESS_REMOVE_FILE
        | ACCESS_MAKE_CHAR
        | ACCESS_MAKE_DIR
        | ACCESS_MAKE_REG
        | ACCESS_MAKE_SOCK
        | ACCESS_MAKE_FIFO
        | ACCESS_MAKE_BLOCK
        | ACCESS_MAKE_SYM;
    const ACCESS_FS_RO: u64 = ACCESS_EXECUTE | ACCESS_READ_FILE | ACCESS_READ_DIR;

    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
    }
    #[repr(C)]
    struct PathBeneath {
        allowed_access: u64,
        parent_fd: i32,
    }

    let attr = RulesetAttr {
        handled_access_fs: ACCESS_FS_RW,
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
        if matches!(
            err.raw_os_error(),
            Some(libc::ENOSYS | libc::EOPNOTSUPP | libc::EPERM | libc::EINVAL)
        ) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "landlock unavailable",
            ));
        }
        return Err(err);
    }
    let ruleset_fd = ruleset as i32;
    let add_rule = |path: &Path, access: u64| -> std::io::Result<()> {
        if !path.exists() {
            return Ok(());
        }
        use std::os::unix::ffi::OsStrExt;
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let fd =
            unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC | libc::O_DIRECTORY) };
        if fd < 0 {
            return Ok(());
        }
        let beneath = PathBeneath {
            allowed_access: access,
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
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    };
    let cleanup_err = |e: std::io::Error| {
        unsafe { libc::close(ruleset_fd) };
        e
    };
    // 必须挂上 bash 依赖的只读路径。不要挂整个 /tmp，否则邻居卷可读。
    for p in ["/usr", "/bin", "/lib", "/lib64", "/etc", "/dev", "/proc"] {
        add_rule(Path::new(p), ACCESS_FS_RO).map_err(cleanup_err)?;
    }
    add_rule(root, ACCESS_FS_RW).map_err(cleanup_err)?;
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        unsafe { libc::close(ruleset_fd) };
        return Err(std::io::Error::last_os_error());
    }
    let rest = unsafe { libc::syscall(SYS_RESTRICT, ruleset, 0u32) };
    unsafe { libc::close(ruleset_fd) };
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
