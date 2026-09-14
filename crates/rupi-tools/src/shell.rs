//! BashTool 的宿主 shell：Unix `sh -c`，Windows PowerShell。
//!
//! `cfg(windows)` 决定运行时选哪条；[`ShellSpec`] / [`IsolationKind`] 在任意
//! 目标上都能构造，方便非 Windows CI 覆盖 Windows 分支，而不假装有 Windows runner。

/// 对标 Pi `POWERSHELL_ARGS`：`pwsh` / `powershell` 共用。
pub(crate) const POWERSHELL_ARGS: &[&str] = &[
    "-NoProfile",
    "-NonInteractive",
    "-ExecutionPolicy",
    "Bypass",
    "-Command",
];

/// 对标 Pi `UTF8_OUTPUT_PREFIX`。OEM 代码页下中文会乱码。
pub(crate) const POWERSHELL_UTF8_PREFIX: &str =
    "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}\n";

/// `CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW`。
#[cfg(any(windows, test))]
pub(crate) const WINDOWS_CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
#[cfg(any(windows, test))]
pub(crate) const WINDOWS_CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(any(windows, test))]
pub(crate) const WINDOWS_CREATION_FLAGS: u32 =
    WINDOWS_CREATE_NEW_PROCESS_GROUP | WINDOWS_CREATE_NO_WINDOW;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PowerShellExe {
    Pwsh,
    WindowsPowerShell,
}

impl PowerShellExe {
    /// 有 PowerShell 7 用 `pwsh`，否则 Windows PowerShell。
    pub(crate) fn select(has_pwsh: bool) -> Self {
        if has_pwsh {
            Self::Pwsh
        } else {
            Self::WindowsPowerShell
        }
    }

    pub(crate) fn program(self) -> &'static str {
        match self {
            Self::Pwsh => "pwsh.exe",
            Self::WindowsPowerShell => "powershell.exe",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostShell {
    UnixSh,
    PowerShell(PowerShellExe),
}

impl HostShell {
    pub(crate) fn current() -> Self {
        #[cfg(windows)]
        {
            Self::PowerShell(PowerShellExe::select(pwsh_on_path()))
        }
        #[cfg(not(windows))]
        {
            Self::UnixSh
        }
    }

    pub(crate) fn program(self) -> &'static str {
        match self {
            Self::UnixSh => "sh",
            Self::PowerShell(exe) => exe.program(),
        }
    }
}

/// 即将 spawn 的 program + argv。Windows 规格可在 Linux 上断言。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShellSpec {
    pub program: String,
    pub args: Vec<String>,
}

impl ShellSpec {
    pub(crate) fn for_host(host: HostShell, command: &str) -> Self {
        match host {
            HostShell::UnixSh => Self {
                program: host.program().into(),
                args: vec!["-c".into(), command.into()],
            },
            HostShell::PowerShell(exe) => {
                let mut args: Vec<String> =
                    POWERSHELL_ARGS.iter().map(|s| (*s).to_string()).collect();
                args.push(wrap_powershell_command(command));
                Self {
                    program: exe.program().into(),
                    args,
                }
            }
        }
    }

    pub(crate) fn current(command: &str) -> Self {
        Self::for_host(HostShell::current(), command)
    }
}

pub(crate) fn wrap_powershell_command(command: &str) -> String {
    format!("{POWERSHELL_UTF8_PREFIX}{command}")
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IsolationKind {
    UnixProcessGroup,
    WindowsCreationFlags(u32),
}

/// 隔离策略按 OS，不按 shell 名：Linux 上测 PowerShell 规格仍是 Unix 进程组。
#[cfg(test)]
pub(crate) fn isolation_for_os(windows: bool) -> IsolationKind {
    if windows {
        IsolationKind::WindowsCreationFlags(WINDOWS_CREATION_FLAGS)
    } else {
        IsolationKind::UnixProcessGroup
    }
}

#[cfg(test)]
pub(crate) fn isolation_kind() -> IsolationKind {
    isolation_for_os(cfg!(windows))
}

pub(crate) fn configure_isolation(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(WINDOWS_CREATION_FLAGS);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = cmd;
    }
}

/// Unix：`killpg`；Windows：`System32\taskkill.exe /F /T /PID`（对标 Pi）。
pub(crate) async fn kill_tree(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        kill_pid_tree(pid);
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

pub(crate) fn kill_pid_tree(pid: u32) {
    #[cfg(unix)]
    {
        // SAFETY: 只向 process_group(0) 建出的组发 SIGKILL，不碰内存。
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        let _ = spawn_taskkill(pid);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
    }
}

/// 任意 OS 可断言的 taskkill argv（不含检测 SystemRoot）。
#[cfg(any(windows, test))]
pub(crate) fn windows_taskkill_args(pid: u32) -> Vec<String> {
    vec!["/F".into(), "/T".into(), "/PID".into(), pid.to_string()]
}

#[cfg(any(windows, test))]
pub(crate) fn windows_taskkill_program(system_root: &str) -> String {
    format!("{system_root}\\System32\\taskkill.exe")
}

#[cfg(windows)]
fn spawn_taskkill(pid: u32) -> std::io::Result<std::process::ExitStatus> {
    use std::os::windows::process::CommandExt as _;
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    std::process::Command::new(windows_taskkill_program(&system_root))
        .args(windows_taskkill_args(pid))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(WINDOWS_CREATE_NO_WINDOW)
        .status()
}

#[cfg(windows)]
fn pwsh_on_path() -> bool {
    use std::os::windows::process::CommandExt as _;
    std::process::Command::new("where.exe")
        .arg("pwsh.exe")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(WINDOWS_CREATE_NO_WINDOW)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_spec_is_sh_c() {
        let spec = ShellSpec::for_host(HostShell::UnixSh, "echo hi");
        assert_eq!(spec.program, "sh");
        assert_eq!(spec.args, vec!["-c", "echo hi"]);
    }

    #[test]
    fn powershell_spec_uses_pi_flags_and_utf8_prefix() {
        for (exe, program) in [
            (PowerShellExe::Pwsh, "pwsh.exe"),
            (PowerShellExe::WindowsPowerShell, "powershell.exe"),
        ] {
            let spec = ShellSpec::for_host(HostShell::PowerShell(exe), "Write-Output hi");
            assert_eq!(spec.program, program);
            assert_eq!(
                &spec.args[..5],
                [
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command"
                ]
            );
            assert_eq!(spec.args[5], wrap_powershell_command("Write-Output hi"));
            assert!(spec.args[5].starts_with(POWERSHELL_UTF8_PREFIX));
            assert!(spec.args[5].contains("Write-Output hi"));
        }
    }

    #[test]
    fn powershell_select_prefers_pwsh() {
        assert_eq!(PowerShellExe::select(true), PowerShellExe::Pwsh);
        assert_eq!(
            PowerShellExe::select(false),
            PowerShellExe::WindowsPowerShell
        );
        assert_eq!(PowerShellExe::Pwsh.program(), "pwsh.exe");
        assert_eq!(PowerShellExe::WindowsPowerShell.program(), "powershell.exe");
    }

    #[test]
    fn current_host_matches_compile_target() {
        if cfg!(windows) {
            assert!(matches!(HostShell::current(), HostShell::PowerShell(_)));
        } else {
            assert_eq!(HostShell::current(), HostShell::UnixSh);
            assert_eq!(ShellSpec::current("echo hi").program, "sh");
        }
    }

    #[test]
    fn isolation_is_os_not_shell_name() {
        assert_eq!(isolation_for_os(false), IsolationKind::UnixProcessGroup);
        assert_eq!(
            isolation_for_os(true),
            IsolationKind::WindowsCreationFlags(WINDOWS_CREATION_FLAGS)
        );
        assert_eq!(
            WINDOWS_CREATION_FLAGS,
            WINDOWS_CREATE_NEW_PROCESS_GROUP | WINDOWS_CREATE_NO_WINDOW
        );
        if cfg!(windows) {
            assert_eq!(
                isolation_kind(),
                IsolationKind::WindowsCreationFlags(WINDOWS_CREATION_FLAGS)
            );
        } else {
            assert_eq!(isolation_kind(), IsolationKind::UnixProcessGroup);
        }
    }

    #[test]
    fn taskkill_spec_is_force_tree() {
        assert_eq!(
            windows_taskkill_program(r"C:\Windows"),
            r"C:\Windows\System32\taskkill.exe"
        );
        assert_eq!(
            windows_taskkill_args(4242),
            vec!["/F", "/T", "/PID", "4242"]
        );
    }
}
