//! The shell one [`ContainerJob`] becomes on the machine.
//!
//! One script per job, rendered from a template under `templates/host/`
//! with every interpolated value shell-quoted first. Podman is invoked,
//! nothing else; flyco never writes to the host's filesystem, and the
//! session's `flycod` configuration — which carries its daemon token —
//! reaches the container through an env-file on the shell's stdin rather
//! than as a command-line argument a `ps` would show.
//!
//! Rendered here rather than on the machine, and tested here, because the
//! templates *are* the contract with Podman: `flycod host` runs what this
//! produces, and a quoting mistake in it is a container name that becomes
//! another command on somebody's real hardware.

use core::fmt;

use askama::Template;
use base64::Engine as _;

use super::{CONFIG_ENV, ContainerJob};
use crate::{ProviderError, flycod};

/// One rendered container job: the non-secret shell script and the optional
/// env-file payload piped to the child process's standard input.
///
/// Keeping the two apart ensures `FLYCO_DAEMON_CONFIG` never enters
/// `/bin/sh -c` argv (visible in `/proc/<pid>/cmdline`).
#[derive(Clone, PartialEq, Eq)]
pub struct RenderedScript {
    /// The POSIX shell commands to run. Contains no credentials.
    pub script: String,
    /// Env-file contents (`KEY=VALUE\n`) piped to stdin for
    /// `podman run --env-file /dev/stdin`.
    pub stdin: Option<String>,
}

impl fmt::Debug for RenderedScript {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RenderedScript")
            .field("script", &self.script)
            .field("has_stdin", &self.stdin.is_some())
            .finish()
    }
}

/// Renders the script and optional stdin env-file for one job.
///
/// Every interpolated value is shell-quoted before it reaches the template,
/// so a container name or an image reference cannot become another command.
/// Credentials for [`ContainerJob::Create`] are returned in
/// [`RenderedScript::stdin`] rather than interpolated into the script text.
///
/// # Errors
///
/// Returns [`ProviderError::Malformed`] if the daemon configuration or the
/// template itself would not render, both of which are flyco's own bug
/// rather than anything the host did.
pub fn render(job: &ContainerJob) -> Result<RenderedScript, ProviderError> {
    let (rendered, stdin) = match job {
        ContainerJob::Create {
            container,
            volume,
            image,
            machine,
            bootstrap,
        } => {
            let config = flycod::render(bootstrap)
                .map_err(|_| ProviderError::Malformed("the flycod configuration did not render"))?;
            let encoded = base64::engine::general_purpose::STANDARD.encode(config);
            let script = CreateScript {
                container: quote(container),
                volume: quote(volume),
                workdir: quote(flycod::WORKDIR),
                image: quote(image),
                machine: quote(&machine.to_string()),
                session: quote(&bootstrap.session.to_string()),
            }
            .render();
            (script, Some(format!("{CONFIG_ENV}={encoded}\n")))
        }
        ContainerJob::Stop { container } => (
            StopScript {
                container: quote(container),
            }
            .render(),
            None,
        ),
        ContainerJob::Start { container } => (
            StartScript {
                container: quote(container),
            }
            .render(),
            None,
        ),
        ContainerJob::Remove {
            container,
            volume,
            keep_volume,
        } => (
            RemoveScript {
                container: quote(container),
                volume: quote(volume),
                keep_volume: *keep_volume,
            }
            .render(),
            None,
        ),
    };

    let script = rendered
        .map_err(|_| ProviderError::Malformed("a podman script template did not render"))?;
    Ok(RenderedScript { script, stdin })
}

/// POSIX-quotes one value for the host's shell.
fn quote(value: &str) -> String {
    shell_escape::unix::escape(value.into()).into_owned()
}

#[derive(Template)]
#[template(path = "host/create.sh", escape = "none")]
struct CreateScript {
    container: String,
    volume: String,
    workdir: String,
    image: String,
    machine: String,
    session: String,
}

#[derive(Template)]
#[template(path = "host/stop.sh", escape = "none")]
struct StopScript {
    container: String,
}

#[derive(Template)]
#[template(path = "host/start.sh", escape = "none")]
struct StartScript {
    container: String,
}

#[derive(Template)]
#[template(path = "host/remove.sh", escape = "none")]
struct RemoveScript {
    container: String,
    volume: String,
    keep_volume: bool,
}

#[cfg(test)]
mod tests {
    use flyco_core::MachineId;

    use super::render;
    use crate::MachineOperation;
    use crate::host::tests::{HOSTNAME, bootstrap, host};
    use crate::host::{CONFIG_ENV, ContainerJob, container_name, volume_name};
    use crate::{ProvisionRequest, flycod};
    use flyco_core::machine::{CloudProviderKind, MachineSpec, Runtime};

    const TOKEN: &str = "fd_a-live-daemon-token";

    fn create() -> ContainerJob {
        let mut bootstrap = bootstrap();
        bootstrap.daemon_token = TOKEN.to_owned();
        host()
            .plan(&MachineOperation::Provision(Box::new(ProvisionRequest {
                machine: MachineId::generate(),
                spec: MachineSpec {
                    provider: CloudProviderKind::Host,
                    machine_type: HOSTNAME.to_owned(),
                    runtime: Runtime::Container,
                    region: HOSTNAME.to_owned(),
                    spot: true,
                    disk_gib: 0,
                },
                bootstrap,
            })))
            .expect("plan a create")
    }

    #[test]
    fn creating_mounts_the_sessions_volume_at_the_checkout() {
        let rendered = render(&create()).expect("render");
        assert!(rendered.script.contains("podman run"));
        assert!(
            rendered.script.contains(&format!(":{}", flycod::WORKDIR)),
            "the volume must be mounted where the checkout lives: {}",
            rendered.script
        );
        assert!(
            rendered.script.contains("podman volume"),
            "the volume has to exist before the container mounts it: {}",
            rendered.script
        );
    }

    #[test]
    fn the_daemon_token_never_reaches_the_command_line() {
        let rendered = render(&create()).expect("render");

        assert!(
            !rendered.script.contains(TOKEN),
            "the token must not appear in the script"
        );
        assert!(
            !rendered.script.contains(CONFIG_ENV),
            "the encoded daemon config must ride stdin, never the shell script argv"
        );
        assert!(
            !format!("{rendered:?}").contains(CONFIG_ENV),
            "Debug must not expose the env-file payload"
        );
        let stdin = rendered
            .stdin
            .as_deref()
            .expect("create pipes env on stdin");
        let lines: Vec<&str> = stdin.lines().collect();
        assert_eq!(lines.len(), 1, "the config rides one env-file line");
        assert!(lines[0].starts_with(&format!("{CONFIG_ENV}=")));
        assert!(
            !lines[0].contains(TOKEN),
            "the config is base64, so even the env-file line is not the token verbatim"
        );
    }

    #[test]
    fn a_hostile_container_name_cannot_become_another_command() {
        let rendered = render(&ContainerJob::Stop {
            container: "flyco-x; rm -rf /".to_owned(),
        })
        .expect("render");

        assert!(rendered.script.contains("'flyco-x; rm -rf /'"));
        assert!(!rendered.script.contains("podman stop flyco-x; rm"));
        assert!(rendered.stdin.is_none());
    }

    #[test]
    fn a_removal_keeps_the_volume_exactly_when_it_was_told_to() {
        let machine = MachineId::generate();
        let volume = volume_name(machine);
        let removal = |keep_volume| ContainerJob::Remove {
            container: container_name(machine),
            volume: volume.clone(),
            keep_volume,
        };

        let kept = render(&removal(true)).expect("render");
        assert!(kept.script.contains("podman rm"));
        assert!(
            !kept.script.contains("podman volume rm"),
            "a machine destroyed with its disk kept must not lose the work on it: {}",
            kept.script
        );

        let released = render(&removal(false)).expect("render");
        assert!(
            released
                .script
                .contains(&format!("if podman volume exists {volume}; then")),
            "volume removal must guard against an already-missing volume under set -eu: {}",
            released.script
        );
        assert!(released.script.contains("podman volume rm --force"));
        assert!(
            released.script.contains(&volume),
            "the volume being released has to be named: {}",
            released.script
        );
    }
}
