//! Dual-UID privilege separation between `flycod` and agent-controlled processes.
//!
//! On a provisioned VM or session container, `flycod` runs under a separate
//! privileged daemon principal that exclusively owns `0600 /etc/flycod/config.toml`
//! and writes managed policy directories non-writable by the agent (`0755`/`0644`).
//! Every child process the agent can influence — coding harnesses, `!` shells,
//! interactive PTYs, and the virtual desktop — drops privileges to an unprivileged
//! [`AgentUser`] before `exec`, clearing supplementary groups, stripping daemon
//! secrets from the environment, and disallowing privilege re-escalation.
//!
//! On a developer machine (where `flycod` runs unprivileged as the developer
//! themselves), [`AgentUser::resolve`] returns `None` when no explicit switch is
//! requested, keeping local development workflows unchanged.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};

/// The default unprivileged agent user on provisioned VMs and session containers.
pub const DEFAULT_AGENT_USER: &str = "flyco";

/// Environment variables inherited from the daemon when building a sanitized
/// environment for an agent-controlled child process.
///
/// Everything not listed here — including `FLYCO_*`, `GITHUB_TOKEN`,
/// `GH_TOKEN`, `CODESPACE_*`, cloud credentials, and harness secrets — is
/// stripped before the child starts. Callers may layer explicit per-process
/// environment variables (such as `CODEX_HOME` or `CLAUDE_CONFIG_DIR`) on top.
const SAFE_INHERITED_ENV: &[&str] = &[
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "TERM",
    "COLORTERM",
    "NO_COLOR",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "DISPLAY",
];

/// Standard system `PATH` fallback when the daemon's own `PATH` is unset.
const DEFAULT_SYSTEM_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// Why the unprivileged agent principal could not be resolved or applied.
#[derive(Debug, thiserror::Error)]
pub enum PrivilegeError {
    /// The requested username contains an interior NUL byte.
    #[error("invalid agent username `{0}`")]
    InvalidName(String),
    /// The target user does not exist in the system user database.
    #[error("agent user `{name}` does not exist in /etc/passwd")]
    UnknownUser {
        /// Username that could not be resolved.
        name: String,
    },
    /// Looking up the user in `/etc/passwd` failed at the OS level.
    #[error("failed to look up agent user `{name}`")]
    Lookup {
        /// Username being looked up.
        name: String,
        /// Underlying OS error.
        #[source]
        source: io::Error,
    },
    /// Changing file or directory ownership failed.
    #[error("could not set ownership on {path}")]
    Chown {
        /// Target path.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: io::Error,
    },
}

/// The unprivileged OS principal that agent-controlled processes run as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentUser {
    /// Login name (e.g. `flyco`).
    pub name: String,
    /// Numeric user id.
    pub uid: u32,
    /// Numeric primary group id.
    pub gid: u32,
    /// User's home directory (e.g. `/home/flyco`).
    pub home: PathBuf,
    /// User's login shell (e.g. `/usr/bin/fish`).
    pub shell: PathBuf,
}

impl AgentUser {
    /// Resolves the agent principal for this `flycod run` invocation.
    ///
    /// - If `requested` names a user explicitly (e.g. `--agent-user flyco`),
    ///   that user is looked up and required.
    /// - If `requested` is `None` and `flycod` is running as root (`euid == 0`),
    ///   [`DEFAULT_AGENT_USER`] is looked up and required so a privileged daemon
    ///   never runs an agent as root.
    /// - If `requested` is `None` and `flycod` is unprivileged (`euid != 0`),
    ///   returns `Ok(None)` (developer-machine mode).
    ///
    /// # Errors
    ///
    /// Returns [`PrivilegeError`] if a required user cannot be resolved.
    pub fn resolve(requested: Option<&str>) -> Result<Option<Self>, PrivilegeError> {
        if let Some(name) = requested {
            return Self::lookup(name).map(Some);
        }
        if current_euid() == 0 {
            return Self::lookup(DEFAULT_AGENT_USER).map(Some);
        }
        Ok(None)
    }

    /// Looks up `name` in the OS user database via `getpwnam_r`.
    ///
    /// # Errors
    ///
    /// Returns [`PrivilegeError::UnknownUser`] if no entry exists or
    /// [`PrivilegeError::Lookup`] if the OS call fails.
    pub fn lookup(name: &str) -> Result<Self, PrivilegeError> {
        let c_name =
            CString::new(name).map_err(|_| PrivilegeError::InvalidName(name.to_owned()))?;
        let mut buf_len = 16 * 1024;
        loop {
            let mut pwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
            let mut buf = vec![0u8; buf_len];
            let mut result: *mut libc::passwd = std::ptr::null_mut();
            // SAFETY: `c_name` is a valid NUL-terminated string, `pwd` and `buf`
            // are valid writable buffers of size `buf_len`, and `result` receives
            // either null or a pointer into `pwd`.
            let rc = unsafe {
                libc::getpwnam_r(
                    c_name.as_ptr(),
                    pwd.as_mut_ptr(),
                    buf.as_mut_ptr().cast::<libc::c_char>(),
                    buf.len(),
                    &raw mut result,
                )
            };
            if rc == libc::ERANGE && buf_len < 1024 * 1024 {
                buf_len *= 2;
                continue;
            }
            if rc != 0 {
                return Err(PrivilegeError::Lookup {
                    name: name.to_owned(),
                    source: io::Error::from_raw_os_error(rc),
                });
            }
            if result.is_null() {
                return Err(PrivilegeError::UnknownUser {
                    name: name.to_owned(),
                });
            }
            // SAFETY: `result` is non-null, so `getpwnam_r` initialized `pwd`
            // with valid NUL-terminated string pointers into `buf`.
            let pwd = unsafe { pwd.assume_init() };
            let home = if pwd.pw_dir.is_null() {
                PathBuf::from("/")
            } else {
                // SAFETY: `pw_dir` is a valid C string in `buf`.
                PathBuf::from(OsStr::from_bytes(
                    unsafe { CStr::from_ptr(pwd.pw_dir) }.to_bytes(),
                ))
            };
            let shell = if pwd.pw_shell.is_null() {
                PathBuf::from("/bin/sh")
            } else {
                // SAFETY: `pw_shell` is a valid C string in `buf`.
                PathBuf::from(OsStr::from_bytes(
                    unsafe { CStr::from_ptr(pwd.pw_shell) }.to_bytes(),
                ))
            };
            return Ok(Self {
                name: name.to_owned(),
                uid: pwd.pw_uid,
                gid: pwd.pw_gid,
                home,
                shell,
            });
        }
    }

    /// Whether spawning a child as this user requires a `setgid`/`setuid`
    /// transition from the current process credentials.
    #[must_use]
    pub fn needs_uid_switch(&self) -> bool {
        self.uid != current_euid() || self.gid != current_egid()
    }

    /// Builds a clean environment for a child running as this user.
    ///
    /// Starts from an empty map (stripping every daemon secret), copies only
    /// safe locale/terminal/proxy variables from `source_env`, sets `HOME`,
    /// `USER`, `LOGNAME`, `SHELL`, and `PATH` for this user, and then applies
    /// `overrides`.
    #[must_use]
    pub fn sanitized_env<I, K, V, O, OK, OV>(
        &self,
        source_env: I,
        overrides: O,
    ) -> BTreeMap<OsString, OsString>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
        O: IntoIterator<Item = (OK, OV)>,
        OK: Into<OsString>,
        OV: Into<OsString>,
    {
        let mut env = BTreeMap::new();
        let mut source_path: Option<OsString> = None;

        for (key, value) in source_env {
            let key = key.into();
            let value = value.into();
            if key == OsStr::new("PATH") {
                source_path = Some(value);
                continue;
            }
            if key
                .to_str()
                .is_some_and(|name| SAFE_INHERITED_ENV.contains(&name))
            {
                env.insert(key, value);
            }
        }

        let bun_bin = self.home.join(".bun/bin");
        let base_path = source_path.unwrap_or_else(|| OsString::from(DEFAULT_SYSTEM_PATH));
        let path = prepend_path_dir(&bun_bin, &base_path);
        env.insert(OsString::from("PATH"), path);
        env.insert(OsString::from("HOME"), self.home.as_os_str().to_owned());
        env.insert(OsString::from("USER"), OsString::from(&self.name));
        env.insert(OsString::from("LOGNAME"), OsString::from(&self.name));
        env.insert(OsString::from("SHELL"), self.shell.as_os_str().to_owned());

        for (key, value) in overrides {
            env.insert(key.into(), value.into());
        }
        env
    }

    /// Applies environment sanitization and `pre_exec` privilege drop to a
    /// [`std::process::Command`].
    ///
    /// Any `overrides` are applied on top of the sanitized environment.
    pub fn apply_std_command<O, OK, OV>(&self, command: &mut std::process::Command, overrides: O)
    where
        O: IntoIterator<Item = (OK, OV)>,
        OK: Into<OsString>,
        OV: Into<OsString>,
    {
        let env = self.sanitized_env(std::env::vars_os(), overrides);
        command.env_clear().envs(env);
        let spec = DropSpec::from(self);
        // SAFETY: `drop_in_pre_exec` performs only async-signal-safe syscalls
        // (`setgroups`, `setgid`, `setuid`, `prctl`) with no heap allocation.
        unsafe {
            command.pre_exec(move || drop_in_pre_exec(spec));
        }
    }

    /// Applies environment sanitization and `pre_exec` privilege drop to a
    /// [`tokio::process::Command`].
    ///
    /// Any `overrides` are applied on top of the sanitized environment.
    pub fn apply_tokio_command<O, OK, OV>(
        &self,
        command: &mut tokio::process::Command,
        overrides: O,
    ) where
        O: IntoIterator<Item = (OK, OV)>,
        OK: Into<OsString>,
        OV: Into<OsString>,
    {
        self.apply_std_command(command.as_std_mut(), overrides);
    }

    /// Applies privilege drop inside an existing `pre_exec` hook (such as a PTY
    /// child after `setsid`/`TIOCSCTTY`).
    #[must_use]
    pub fn drop_spec(&self) -> DropSpec {
        DropSpec::from(self)
    }
}

/// Async-signal-safe privilege-drop parameters copied into a `pre_exec` closure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropSpec {
    uid: u32,
    gid: u32,
    switch_ids: bool,
}

impl From<&AgentUser> for DropSpec {
    fn from(user: &AgentUser) -> Self {
        Self {
            uid: user.uid,
            gid: user.gid,
            switch_ids: user.needs_uid_switch(),
        }
    }
}

/// Drops process credentials inside a `pre_exec` closure before `exec`.
///
/// Clears supplementary groups, sets primary GID, sets UID, clears any Linux
/// ambient capabilities, and enables `PR_SET_NO_NEW_PRIVS` on Linux so setuid
/// binaries cannot re-escalate.
///
/// # Safety / Async-signal-safety
///
/// Calls only async-signal-safe POSIX/Linux syscalls with no allocations.
///
/// # Errors
///
/// Returns the OS error if any privilege-drop syscall fails, aborting the
/// child spawn rather than running with elevated privileges.
pub fn drop_in_pre_exec(spec: DropSpec) -> io::Result<()> {
    if spec.switch_ids {
        // Clear supplementary groups first while still holding CAP_SETGID.
        // SAFETY: `setgroups(0, null)` is valid on Linux and macOS to drop all
        // supplementary groups.
        if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: setting primary group id to target `gid`.
        if unsafe { libc::setgid(spec.gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: setting real/effective/saved user id to target `uid`.
        if unsafe { libc::setuid(spec.uid) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }

    #[cfg(target_os = "linux")]
    {
        // Clear any ambient capabilities (defense in depth when invoked with
        // ambient caps rather than UID 0) and lock out privilege escalation
        // through setuid binaries.
        // SAFETY: `PR_CAP_AMBIENT` / `PR_CAP_AMBIENT_CLEAR_ALL` and
        // `PR_SET_NO_NEW_PRIVS` take scalar arguments only.
        let _ = unsafe {
            libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_CLEAR_ALL,
                0,
                0,
                0,
            )
        };
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }

    Ok(())
}

/// Current effective user id.
#[must_use]
pub fn current_euid() -> u32 {
    // SAFETY: `geteuid` always succeeds and has no preconditions.
    unsafe { libc::geteuid() }
}

/// Current effective group id.
#[must_use]
pub fn current_egid() -> u32 {
    // SAFETY: `getegid` always succeeds and has no preconditions.
    unsafe { libc::getegid() }
}

/// Changes the owner and/or group of `path` without following a trailing symlink.
///
/// `None` leaves that component unchanged (`-1` in POSIX `lchown`).
///
/// # Errors
///
/// Returns [`PrivilegeError::Chown`] if the path is invalid or `lchown` fails.
pub fn chown_path(path: &Path, uid: Option<u32>, gid: Option<u32>) -> Result<(), PrivilegeError> {
    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| PrivilegeError::Chown {
        path: path.to_owned(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL byte"),
    })?;
    let owner_uid: libc::uid_t = uid.unwrap_or(libc::uid_t::MAX);
    let group_gid: libc::gid_t = gid.unwrap_or(libc::gid_t::MAX);
    // SAFETY: `c_path` is a valid NUL-terminated path; `lchown` does not
    // follow symlinks.
    if unsafe { libc::lchown(c_path.as_ptr(), owner_uid, group_gid) } != 0 {
        return Err(PrivilegeError::Chown {
            path: path.to_owned(),
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

/// Recursively changes ownership of `root` and every entry underneath it to
/// `agent.uid:agent.gid` without following symlinks.
///
/// # Errors
///
/// Returns [`PrivilegeError::Chown`] if reading a directory or changing
/// ownership on any entry fails.
pub fn chown_tree(root: &Path, agent: &AgentUser) -> Result<(), PrivilegeError> {
    if !agent.needs_uid_switch() {
        return Ok(());
    }
    let mut stack = vec![root.to_owned()];
    while let Some(path) = stack.pop() {
        let meta = std::fs::symlink_metadata(&path).map_err(|source| PrivilegeError::Chown {
            path: path.clone(),
            source,
        })?;
        chown_path(&path, Some(agent.uid), Some(agent.gid))?;
        if meta.is_dir() {
            let entries = std::fs::read_dir(&path).map_err(|source| PrivilegeError::Chown {
                path: path.clone(),
                source,
            })?;
            for entry in entries {
                let entry = entry.map_err(|source| PrivilegeError::Chown {
                    path: path.clone(),
                    source,
                })?;
                stack.push(entry.path());
            }
        }
    }
    Ok(())
}

/// Ensures `dir` exists at `mode`, owned by the daemon (not writable by the
/// agent when `mode` is `0o755`), or group-accessible to `agent` when
/// `group_agent` is `true` (e.g. `0o750` socket directories).
///
/// # Errors
///
/// Returns an `io::Error` if creating, setting permissions on, or chowning the
/// directory fails.
pub fn ensure_dir(
    dir: &Path,
    mode: u32,
    agent: Option<&AgentUser>,
    group_agent: bool,
) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode))?;
    if group_agent
        && let Some(user) = agent
        && user.needs_uid_switch()
    {
        chown_path(dir, None, Some(user.gid)).map_err(|err| match err {
            PrivilegeError::Chown { source, .. } => source,
            other => io::Error::other(other),
        })?;
    }
    Ok(())
}

/// Prepends `dir` to a colon-separated `PATH` if not already present.
fn prepend_path_dir(dir: &Path, current: &OsStr) -> OsString {
    let dir_bytes = dir.as_os_str().as_bytes();
    let already_present = current
        .as_bytes()
        .split(|&byte| byte == b':')
        .any(|segment| segment == dir_bytes);
    if already_present {
        return current.to_owned();
    }
    let mut combined = OsString::with_capacity(dir.as_os_str().len() + 1 + current.len());
    combined.push(dir.as_os_str());
    if !current.is_empty() {
        combined.push(":");
        combined.push(current);
    }
    combined
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::{AgentUser, PrivilegeError, current_egid, current_euid};

    fn fake_agent() -> AgentUser {
        AgentUser {
            name: "flyco".to_owned(),
            uid: 1001,
            gid: 1001,
            home: PathBuf::from("/home/flyco"),
            shell: PathBuf::from("/usr/bin/fish"),
        }
    }

    #[test]
    fn sanitized_env_strips_daemon_secrets_and_sets_agent_identity() {
        let agent = fake_agent();
        let daemon_env = vec![
            ("HOME", "/var/lib/flyco/daemon-home"),
            ("USER", "root"),
            ("LOGNAME", "root"),
            ("SHELL", "/bin/sh"),
            ("PATH", "/usr/local/bin:/usr/bin:/bin"),
            ("LANG", "C.UTF-8"),
            ("TZ", "UTC"),
            ("FLYCO_DAEMON_CONFIG", "c2VjcmV0"),
            ("FLYCO_GIT_TOKEN", "gho_secret"),
            ("GITHUB_TOKEN", "ghu_codespace_secret"),
            ("GH_TOKEN", "gho_cli_secret"),
            ("ANTHROPIC_API_KEY", "sk-ant-leaked"),
            ("OPENAI_API_KEY", "sk-openai-leaked"),
            ("AWS_SECRET_ACCESS_KEY", "aws-secret"),
        ];
        let overrides = vec![("CODEX_HOME", "/var/lib/flyco/codex")];

        let env = agent.sanitized_env(daemon_env, overrides);
        let get = |k: &str| env.get(&OsString::from(k)).and_then(|v| v.to_str());

        assert_eq!(get("HOME"), Some("/home/flyco"));
        assert_eq!(get("USER"), Some("flyco"));
        assert_eq!(get("LOGNAME"), Some("flyco"));
        assert_eq!(get("SHELL"), Some("/usr/bin/fish"));
        assert_eq!(
            get("PATH"),
            Some("/home/flyco/.bun/bin:/usr/local/bin:/usr/bin:/bin")
        );
        assert_eq!(get("LANG"), Some("C.UTF-8"));
        assert_eq!(get("TZ"), Some("UTC"));
        assert_eq!(get("CODEX_HOME"), Some("/var/lib/flyco/codex"));

        for secret in [
            "FLYCO_DAEMON_CONFIG",
            "FLYCO_GIT_TOKEN",
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            assert_eq!(
                get(secret),
                None,
                "{secret} must be stripped from child env"
            );
        }
    }

    #[test]
    fn lookup_rejects_missing_and_invalid_usernames() {
        let missing = AgentUser::lookup("flyco-definitely-no-such-user-999999");
        assert!(matches!(missing, Err(PrivilegeError::UnknownUser { .. })));

        let invalid = AgentUser::lookup("bad\0name");
        assert!(matches!(invalid, Err(PrivilegeError::InvalidName(_))));
    }

    #[test]
    fn apply_std_command_sanitizes_real_subprocess_environment() {
        let agent = AgentUser {
            name: "test-agent".to_owned(),
            uid: current_euid(),
            gid: current_egid(),
            home: PathBuf::from("/tmp/flyco-test-agent-home"),
            shell: PathBuf::from("/bin/sh"),
        };
        assert!(!agent.needs_uid_switch());

        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(
            "printf '%s|%s|%s|%s' \"$HOME\" \"$USER\" \"$FLYCO_DAEMON_CONFIG\" \"$CUSTOM_VAR\"",
        );
        agent.apply_std_command(
            &mut cmd,
            BTreeMap::from([("CUSTOM_VAR", "expected-override")]),
        );

        let out = cmd.output().expect("spawn sanitized child");
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "/tmp/flyco-test-agent-home|test-agent||expected-override"
        );
    }

    #[test]
    fn needs_uid_switch_detects_different_uid_or_gid() {
        let same = AgentUser {
            name: "self".to_owned(),
            uid: current_euid(),
            gid: current_egid(),
            home: PathBuf::from("/tmp"),
            shell: PathBuf::from("/bin/sh"),
        };
        assert!(!same.needs_uid_switch());

        let other_user = AgentUser {
            uid: current_euid().wrapping_add(1),
            ..same.clone()
        };
        assert!(other_user.needs_uid_switch());

        let other_group = AgentUser {
            gid: current_egid().wrapping_add(1),
            ..same
        };
        assert!(other_group.needs_uid_switch());
    }
}
