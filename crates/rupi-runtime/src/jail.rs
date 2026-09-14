//! 把用户 `sh -c` 箍在句柄根。
//!
//! - Linux：优先 bwrap + user ns + 每槽独立根；否则 landlock。sandboxd 尽量断网。
//! - macOS：`sandbox-exec`，`(deny default)` + 必要 allow。
//! - 其他 Unix：chdir，并尽力 `chroot`。
//!
//! jail 必须能执行 `/bin/sh`（只读挂上 `/usr` `/bin` `/lib` 等），但不能读邻居卷。
//! 这不是微 VM，也不是本机 Docker。指定了不支持的镜像必须失败，不能静默丢掉。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use tokio::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolation {
    /// execd：文件系统 jail。
    Jail,
    /// sandboxd：每槽独立根 + 尽量 user ns / 断网。
    Sandbox,
}

/// sandboxd `CreateIn.image` 解析结果。Docker / OCI 引用不是后端。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxImage {
    /// 内建隔离 jail（bwrap / user ns / landlock / macOS deny-default）。
    Default,
    /// 本地 rootfs 目录，经 bwrap 挂成独立根。不是 Docker。
    Rootfs(PathBuf),
}

impl SandboxImage {
    pub fn as_label(&self) -> String {
        match self {
            Self::Default => "default".into(),
            Self::Rootfs(p) => format!("rootfs:{}", p.display()),
        }
    }

    pub fn rootfs(&self) -> Option<&Path> {
        match self {
            Self::Default => None,
            Self::Rootfs(p) => Some(p),
        }
    }

    /// 镜像对应的隔离后端是否可用。不可用必须让 create 失败，不能忽略镜像。
    pub fn require_backend(&self) -> Result<(), String> {
        match self {
            Self::Default => Ok(()),
            Self::Rootfs(p) => {
                if !which("bwrap") {
                    return Err(format!(
                        "unsupported sandbox image {}: isolated rootfs requires bwrap (not Docker)",
                        p.display()
                    ));
                }
                Ok(())
            }
        }
    }
}

/// 解析 create 的 `image`。空 / `default` / `jail` 走内建隔离；其余要么是本地 rootfs，要么失败。
pub fn parse_sandbox_image(image: Option<&str>) -> Result<SandboxImage, String> {
    let raw = image.map(str::trim).unwrap_or("");
    let key = raw.to_ascii_lowercase();
    if key.is_empty() || key == "default" || key == "jail" {
        return Ok(SandboxImage::Default);
    }
    if key.starts_with("docker:")
        || key.starts_with("docker://")
        || key.starts_with("oci:")
        || key.starts_with("http://")
        || key.starts_with("https://")
    {
        return Err(format!(
            "unsupported sandbox image {raw:?}: docker/oci is not a sandboxd backend"
        ));
    }
    if let Some(path) = raw.strip_prefix("rootfs:") {
        return rootfs_image(PathBuf::from(path));
    }
    let p = Path::new(raw);
    if p.is_absolute() {
        return rootfs_image(p.to_path_buf());
    }
    if let Ok(dir) = std::env::var("RUPI_SANDBOX_IMAGES") {
        let cand = PathBuf::from(dir).join(raw);
        if cand.is_dir() {
            return rootfs_image(cand);
        }
    }
    Err(format!(
        "unsupported sandbox image {raw:?}: not a default jail or local rootfs"
    ))
}

fn rootfs_image(p: PathBuf) -> Result<SandboxImage, String> {
    if !p.is_dir() {
        return Err(format!(
            "unsupported sandbox image: rootfs {} is not a directory",
            p.display()
        ));
    }
    let canon = p.canonicalize().unwrap_or(p);
    if canon == Path::new("/") {
        return Err("unsupported sandbox image: host / is not an isolated rootfs".into());
    }
    if !canon.join("bin/sh").is_file() && !canon.join("bin/bash").is_file() {
        return Err(format!(
            "unsupported sandbox image: rootfs {} missing /bin/sh",
            canon.display()
        ));
    }
    Ok(SandboxImage::Rootfs(canon))
}

pub struct JailedChild {
    pub child: tokio::process::Child,
}

/// 构造已 jail 的 `sh -c`。
pub fn command(root: &Path, script: &str, isolation: Isolation) -> anyhow::Result<Command> {
    command_with(
        root,
        script,
        JailOpts {
            isolation,
            rootfs: None,
        },
    )
}

pub struct JailOpts<'a> {
    pub isolation: Isolation,
    pub rootfs: Option<&'a Path>,
}

pub fn command_with(root: &Path, script: &str, opts: JailOpts<'_>) -> anyhow::Result<Command> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    if let Some(rfs) = opts.rootfs {
        if !which("bwrap") {
            anyhow::bail!(
                "unsupported sandbox image: isolated rootfs {} requires bwrap",
                rfs.display()
            );
        }
        return Ok(bwrap_command(&root, script, opts.isolation, Some(rfs)));
    }
    if which("bwrap") {
        return Ok(bwrap_command(&root, script, opts.isolation, None));
    }
    #[cfg(target_os = "macos")]
    if which("sandbox-exec") {
        return Ok(macos_sandbox_command(&root, script));
    }
    if opts.isolation == Isolation::Sandbox {
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        anyhow::bail!("sandbox isolation backend unavailable on this OS");
    }
    Ok(plain_jailed_sh(&root, script, opts.isolation))
}

pub(crate) fn which(name: &str) -> bool {
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

fn bwrap_user_ns() -> bool {
    static CELL: OnceLock<bool> = OnceLock::new();
    *CELL.get_or_init(|| {
        if !which("bwrap") {
            return false;
        }
        std::process::Command::new("bwrap")
            .args([
                "--unshare-user",
                "--die-with-parent",
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
                "--dev",
                "/dev",
                "--",
                "true",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

fn bwrap_command(
    root: &Path,
    script: &str,
    isolation: Isolation,
    rootfs: Option<&Path>,
) -> Command {
    let root_s = root.display().to_string();
    let mut cmd = Command::new("bwrap");
    cmd.arg("--die-with-parent");
    if bwrap_user_ns() {
        cmd.args(["--unshare-user", "--uid", "0", "--gid", "0"]);
    }
    cmd.args(["--unshare-pid", "--unshare-uts", "--unshare-ipc"]);
    if isolation == Isolation::Sandbox {
        cmd.arg("--unshare-net");
    }
    if let Some(rfs) = rootfs {
        let rfs_s = rfs.display().to_string();
        cmd.args(["--ro-bind", &rfs_s, "/"]);
        cmd.args(["--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp"]);
    } else {
        cmd.args(["--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp"]);
        cmd.args(["--ro-bind", "/usr", "/usr", "--ro-bind", "/bin", "/bin"]);
        cmd.args([
            "--ro-bind-try",
            "/lib",
            "/lib",
            "--ro-bind-try",
            "/lib64",
            "/lib64",
        ]);
        if isolation != Isolation::Sandbox {
            cmd.args([
                "--ro-bind-try",
                "/etc/resolv.conf",
                "/etc/resolv.conf",
                "--ro-bind-try",
                "/etc/ssl",
                "/etc/ssl",
            ]);
        }
    }
    cmd.args([
        "--bind",
        &root_s,
        "/workspace",
        "--chdir",
        "/workspace",
        "--hostname",
        "rupi",
        "--setenv",
        "HOME",
        "/workspace",
        "--setenv",
        "TMPDIR",
        "/workspace",
        "--setenv",
        "PWD",
        "/workspace",
    ]);
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

fn escape_sb(root: &Path) -> String {
    root.display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// macOS seatbelt：deny-default，只放行跑 `/bin/sh` 与句柄根所需路径。
/// 全平台可生成，供回归断言，避免再写成 `(allow default)`。
///
/// Tokio 管道是 FIFO vnode，不是 `/dev/fd/N`；缺 FIFO write 时 echo 会
/// `exit=1 stdout="" stderr=""`。SBPL 没有 `PIPE`，vnode 名是 `FIFO`。
pub fn macos_sandbox_profile(root: &Path) -> String {
    let root_s = escape_sb(root);
    format!(
        r##"(version 1)
(deny default)
(allow process-fork)
(allow process-info* (target self))
(allow signal)
(allow sysctl*)
(allow mach-lookup)
(allow mach-register)
(allow ipc-posix-shm*)
(allow ipc-posix-sem*)
(allow file-read-metadata)
(allow process-exec*
  (literal "/bin/sh")
  (literal "/bin/bash")
  (literal "/usr/bin/env")
  (literal "/usr/bin/true")
  (literal "/usr/bin/false")
  (subpath "/bin")
  (subpath "/usr/bin")
  (subpath "/usr/libexec")
  (subpath "/usr/sbin")
  (subpath "/System")
  (subpath "/Library")
  (subpath "/System/Cryptexes")
)
(allow file-map-executable
  (literal "/usr/lib/dyld")
  (literal "/usr/lib/libSystem.B.dylib")
  (subpath "/usr/lib")
  (subpath "/usr/lib/system")
  (subpath "/bin")
  (subpath "/usr/bin")
  (subpath "/System")
  (subpath "/Library")
  (subpath "/private/var/db/dyld")
  (subpath "/var/db/dyld")
  (subpath "/System/Volumes/Preboot")
  (subpath "/System/Cryptexes")
)
(allow file-read*
  (literal "/bin/sh")
  (literal "/bin/bash")
  (literal "/usr/bin/env")
  (literal "/usr/lib/dyld")
  (literal "/usr/lib/libSystem.B.dylib")
  (subpath "/usr")
  (subpath "/usr/lib")
  (subpath "/usr/lib/system")
  (subpath "/bin")
  (subpath "/sbin")
  (subpath "/System")
  (subpath "/Library")
  (subpath "/private/var/db/dyld")
  (subpath "/var/db/dyld")
  (subpath "/private/var/db/timezone")
  (subpath "/System/Volumes/Preboot")
  (subpath "/System/Cryptexes")
  (subpath "/opt/homebrew")
  (subpath "/opt/local")
  (literal "/etc")
  (subpath "/etc")
  (literal "/private/etc")
  (subpath "/private/etc")
  (literal "/dev/null")
  (literal "/dev/zero")
  (literal "/dev/random")
  (literal "/dev/urandom")
  (literal "/dev/tty")
  (literal "/dev/stdin")
  (literal "/dev/stdout")
  (literal "/dev/stderr")
  (literal "/dev/dtracehelper")
  (literal "/dev/dtrussHelper")
  (regex #"^/dev/fd/")
  (subpath "{root_s}")
)
(allow file-read* file-write*
  (subpath "{root_s}")
)
(allow file-read-data file-write-data file-ioctl
  (vnode-type FIFO)
  (vnode-type SOCKET)
  (vnode-type CHARACTER-DEVICE)
)
(allow file-write-data
  (literal "/dev/null")
  (literal "/dev/stdout")
  (literal "/dev/stderr")
  (literal "/dev/tty")
  (regex #"^/dev/fd/")
)
(allow file-ioctl
  (literal "/dev/null")
  (literal "/dev/dtracehelper")
  (literal "/dev/dtrussHelper")
  (regex #"^/dev/fd/")
)
"##
    )
}

#[cfg(target_os = "macos")]
fn macos_sandbox_command(root: &Path, script: &str) -> Command {
    let profile = macos_sandbox_profile(root);
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
        let _ = unshare_user();
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
fn write_proc(name: &str, data: &[u8]) -> std::io::Result<()> {
    let path = format!("/proc/self/{name}");
    let c = std::ffi::CString::new(path)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
    unsafe { libc::close(fd) };
    if n < 0 || n as usize != data.len() {
        return Err(std::io::Error::other("short write"));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn unshare_user() -> std::io::Result<()> {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let rc = unsafe { libc::unshare(libc::CLONE_NEWUSER) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    write_proc("setgroups", b"deny")?;
    write_proc("uid_map", format!("0 {uid} 1").as_bytes())?;
    write_proc("gid_map", format!("0 {gid} 1").as_bytes())?;
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
        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_PATH | libc::O_CLOEXEC | libc::O_DIRECTORY,
            )
        };
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

    #[test]
    fn macos_profile_is_deny_default_not_allow_default() {
        let p = macos_sandbox_profile(Path::new("/tmp/slot-root"));
        assert!(p.contains("(deny default)"), "{p}");
        assert!(
            !p.contains("(allow default)"),
            "macOS jail must not allow-default: {p}"
        );
        assert!(p.contains("/tmp/slot-root"), "{p}");
        assert!(p.contains("/dev/fd"), "{p}");
        assert!(p.contains("/usr"), "{p}");
        assert!(p.contains("/bin"), "{p}");
        assert!(p.contains("/bin/sh"), "{p}");
        assert!(p.contains("/usr/lib/dyld"), "{p}");
        assert!(p.contains("/usr/lib/libSystem.B.dylib"), "{p}");
        assert!(p.contains("(vnode-type FIFO)"), "{p}");
        assert!(p.contains("file-write-data"), "{p}");
        assert!(!p.contains("(subpath \"/Users\")"), "{p}");
        assert!(!p.contains("(subpath \"/tmp\")"), "{p}");
        assert!(!p.contains("(subpath \"/private/var/folders\")"), "{p}");
        assert!(!p.contains("(vnode-type PIPE)"), "{p}");
    }

    #[test]
    fn parse_image_default_ok() {
        assert_eq!(parse_sandbox_image(None).unwrap(), SandboxImage::Default);
        assert_eq!(
            parse_sandbox_image(Some("")).unwrap(),
            SandboxImage::Default
        );
        assert_eq!(
            parse_sandbox_image(Some("default")).unwrap(),
            SandboxImage::Default
        );
        assert_eq!(
            parse_sandbox_image(Some("jail")).unwrap(),
            SandboxImage::Default
        );
        parse_sandbox_image(Some("default"))
            .unwrap()
            .require_backend()
            .unwrap();
    }

    #[test]
    fn parse_image_docker_and_unknown_rejected() {
        for img in [
            "ubuntu:22.04",
            "docker://foo",
            "docker:latest",
            "oci:alpine",
            "https://example/img",
            "alpine",
        ] {
            let err = parse_sandbox_image(Some(img)).unwrap_err();
            assert!(
                err.contains("unsupported"),
                "image {img:?} must fail, got {err}"
            );
        }
    }

    #[test]
    fn parse_image_host_root_rejected() {
        let err = parse_sandbox_image(Some("/")).unwrap_err();
        assert!(err.contains("unsupported"), "{err}");
    }

    #[test]
    fn parse_image_missing_rootfs_rejected() {
        let err = parse_sandbox_image(Some("/no/such/rupi-rootfs-dir")).unwrap_err();
        assert!(err.contains("unsupported"), "{err}");
    }
}
