#!/bin/sh
set -eu

# Two machines, one installer.
#
#   flycod.sh                     the session image cloud-init builds: the
#                                 daemon, its harnesses, and flycod.service.
#   flycod.sh host enroll <token> a machine the user owns: rootless Podman,
#                                 the flyco user, `flycod host enroll` against
#                                 the control plane, and flycod-host.service.
#
# The second is the one line the enrollment wizard hands out:
#   curl -fsSL https://dev.flyco.dev/install/flycod.sh | sudo sh -s -- host enroll fh_…

if [ "$(id -u)" -ne 0 ]; then
  echo "flycod installer must run as root" >&2
  exit 1
fi

usage() {
  echo "usage: flycod.sh [--asset-base URL] [--control-plane URL]                     set this machine up as a flyco session VM" >&2
  echo "       flycod.sh [--asset-base URL] [--control-plane URL] image <flycod>      set up a session container image, flycod from a file" >&2
  echo "       flycod.sh [--asset-base URL] [--control-plane URL] host enroll <token> enrol this machine as a flyco host" >&2
  exit 64
}

asset_base_arg=
control_plane_arg=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --asset-base)
      [ "$#" -ge 2 ] || usage
      asset_base_arg=${2%/}
      shift 2
      ;;
    --asset-base=*)
      asset_base_arg=${1#--asset-base=}
      asset_base_arg=${asset_base_arg%/}
      shift
      ;;
    --control-plane)
      [ "$#" -ge 2 ] || usage
      control_plane_arg=${2%/}
      shift 2
      ;;
    --control-plane=*)
      control_plane_arg=${1#--control-plane=}
      control_plane_arg=${control_plane_arg%/}
      shift
      ;;
    --)
      shift
      break
      ;;
    *)
      break
      ;;
  esac
done

if [ -n "$asset_base_arg" ]; then
  asset_base=$asset_base_arg
elif [ -n "$control_plane_arg" ] && [ -z "${FLYCO_ASSET_BASE:-}" ]; then
  asset_base="$control_plane_arg/install"
else
  asset_base=${FLYCO_ASSET_BASE:-https://dev.flyco.dev/install}
fi
asset_base=${asset_base%/}
binary_base=${FLYCO_BINARY_BASE:-$asset_base}
binary_base=${binary_base%/}
# The control plane a host enrols with is the origin serving this installer:
# a machine is enrolled into the deployment whose wizard printed the command,
# and there is nothing else on the machine that could know which that is.
control_plane=${control_plane_arg:-${FLYCO_CONTROL_PLANE:-${asset_base%/install}}}
control_plane=${control_plane%/}
install_root=/usr/local/bin
service_root=/etc/systemd/system
runtime_user=flyco
runtime_group=flyco
runtime_home=/home/flyco
host_config_dir=/etc/flyco
host_config=$host_config_dir/host.toml
# Where rootless Podman keeps this machine's containers and volumes. Its free
# space is what the control plane schedules sessions against.
volume_root=$runtime_home/.local/share/containers
# One subordinate id range per user, 65536 wide, as useradd itself allocates
# them. Rootless Podman maps the container's users into it.
subid_count=65536
subid_base=100000

case "$(uname -m)" in
  x86_64) artifact=flycod-linux-x86_64 ;;
  aarch64|arm64) artifact=flycod-linux-aarch64 ;;
  *)
    echo "flycod does not publish a binary for architecture $(uname -m)" >&2
    exit 1
    ;;
esac

download() {
  curl --fail --silent --show-error --location --retry 3 --output "$2" "$1"
}

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
chmod 0755 "$scratch"

# The flyco user, with the login shell the caller's machine actually has.
create_runtime_user() {
  login_shell=$1
  if ! getent group "$runtime_group" >/dev/null; then
    groupadd --system "$runtime_group"
  fi
  if ! id "$runtime_user" >/dev/null 2>&1; then
    useradd --system --gid "$runtime_group" --create-home --home-dir "$runtime_home" --shell "$login_shell" "$runtime_user"
  fi
}

# The daemon binary, checked against the checksum published beside it.
# These names do not pick a version: the control plane serving this URL
# resolves each of them to the daemon build that speaks its own wire
# protocol, so a machine can never install a flycod its control plane
# refuses at attach — and a protocol nothing was ever published for answers
# 404, which fails this script the same way a missing release always did.
install_flycod() {
  download "$binary_base/$artifact.sha256" "$scratch/$artifact.sha256"
  expected_sha256=$(awk '{ print $1; exit }' "$scratch/$artifact.sha256")
  if [ -z "$expected_sha256" ]; then
    echo "checksum file $artifact.sha256 is empty" >&2
    exit 1
  fi
  download "$binary_base/$artifact?sha256=$expected_sha256" "$scratch/$artifact"
  (cd "$scratch" && sha256sum --check "$artifact.sha256")
  install -m 0755 "$scratch/$artifact" "$install_root/flycod"
}

# The image build has the binary it just cross-compiled on hand, and no
# control plane to fetch one from: an image is published *with* the daemon
# it carries, by the same publish, so downloading here would be downloading
# the previous release.
install_flycod_from() {
  install -m 0755 "$1" "$install_root/flycod"
}

# One systemd unit, downloaded and installed under its published name.
install_unit() {
  download "$asset_base/$1" "$scratch/$1"
  install -m 0644 "$scratch/$1" "$service_root/$1"
}

# The first free subordinate id range in `$1`, so two users on one machine
# never share a range.
next_subid() {
  [ -f "$1" ] || : >"$1"
  awk -F: -v base="$subid_base" '
    { end = $2 + $3; if (end > base) base = end }
    END { print base }
  ' "$1"
}

# ── The session image ──

# Pinned session-runtime tool versions and SHA-256 digests.
# Pinning versions and verifying hardcoded digests avoids per-boot unauthenticated
# GitHub API lookups and guarantees reproducible VM and container builds.
bun_version=1.3.4
bun_sha256_x64=33c6996049e8d37e8b815959b14b05e5b6f496121352bf11bae7d047193c28bf
bun_sha256_aarch64=c46e841fed85347521915b1b3904d6d175d8e2fd915e18e01c111318219115a4

claude_version=2.1.331
claude_dist_base=https://storage.googleapis.com/claude-code-dist-86c565f3-f756-42ad-8dfa-d59b1c096819/claude-code-releases
claude_sha256_x64=4c00839cf46beb2b38ec310105f2d538f7f9321fe46c636299b74bcab94626b1
claude_sha256_arm64=3e5afea76cf6ed53f42076d8b175c4ee3d9ab34cd9b90452da115d21ad0d5d31

codex_version=rust-v0.172.0
codex_sha256_x64=a2342a8f7f68f088be30620ccfe9cb84162c93a6b56177855961cda3ed6be392
codex_sha256_aarch64=0b99526c185a4425fe96a2214fb7b6a6a7f7c1471883b7afbebe97e15e83e79e

# Everything a session runs on, whether the machine is a VM or a container:
# the packages, the runtime user, the directories the daemon writes, bun for
# MCP server aliases (`bunx`/`npx`/`node`), native `claude`, `codex`, `devin`,
# and the shell the terminal opens. The two callers differ only in where
# `flycod` comes from and whether there is a systemd to hand it to, so this is
# the one list and they add their own line.
install_session_runtime() {
  export DEBIAN_FRONTEND=noninteractive
  apt-get update
  # xvfb + openbox + xfonts-base are the desktop a `computer_use` session
  # drives: Xvfb is the screen the daemon captures, openbox the window
  # manager its tools open windows under, and the fonts are what X clients
  # draw text with. They ship in every image because the daemon runs
  # unprivileged and cannot install them per-session.
  apt-get install --yes --no-install-recommends ca-certificates curl fish git unzip xvfb openbox xfonts-base
  rm -rf /var/lib/apt/lists/*

  create_runtime_user /usr/bin/fish

  # Dual-UID directory layout:
  # - Daemon-only secrets and state (`0700 root:root`): unreachable by the
  #   unprivileged agent user (`flyco`).
  # - Managed policy trees (`0755 root:root`): readable by the harnesses, never
  #   writable or replaceable by the agent user.
  # - Local Unix-socket runtime directory (`0750 root:flyco`): daemon binds the
  #   MCP broker and desktop IPC sockets; agent connects.
  # - Agent workspace and writable harness state (`flyco:flyco`).
  install -d -o root -g root -m 0700 /etc/flycod /var/lib/flyco/transcripts /var/lib/flyco/daemon-home
  install -d -o root -g root -m 0755 /var/lib/flyco /var/lib/flyco/codex /var/lib/flyco/devin-config /etc/claude-code
  install -d -o root -g "$runtime_group" -m 0750 /var/lib/flyco/run
  install -d -o "$runtime_user" -g "$runtime_group" -m 0755 /srv/flyco/work
  install -d -o "$runtime_user" -g "$runtime_group" -m 0750 /var/lib/flyco/claude /var/lib/flyco/devin

  # Bun, kept for `bunx`/`npx`/`node` MCP server registrations.
  case "$artifact" in
    flycod-linux-x86_64)
      bun_archive=bun-linux-x64.zip
      expected_bun_hash=$bun_sha256_x64
      ;;
    flycod-linux-aarch64)
      bun_archive=bun-linux-aarch64.zip
      expected_bun_hash=$bun_sha256_aarch64
      ;;
  esac
  download "https://github.com/oven-sh/bun/releases/download/bun-v$bun_version/$bun_archive" "$scratch/$bun_archive"
  printf '%s  %s\n' "$expected_bun_hash" "$scratch/$bun_archive" | sha256sum --check -
  unzip -q "$scratch/$bun_archive" -d "$scratch/bun"
  install -d -o "$runtime_user" -g "$runtime_group" /home/flyco/.bun/bin
  install -m 0755 "$scratch/bun"/*/bun /home/flyco/.bun/bin/bun
  chown -R "$runtime_user:$runtime_group" /home/flyco/.bun

  # Native Claude Code CLI, driven directly in-process by `flycod` via `claude-acp`.
  case "$artifact" in
    flycod-linux-x86_64)
      claude_platform=linux-x64
      expected_claude_hash=$claude_sha256_x64
      ;;
    flycod-linux-aarch64)
      claude_platform=linux-arm64
      expected_claude_hash=$claude_sha256_arm64
      ;;
  esac
  download "$claude_dist_base/$claude_version/$claude_platform/claude" "$scratch/claude"
  printf '%s  %s\n' "$expected_claude_hash" "$scratch/claude" | sha256sum --check -
  install -m 0755 "$scratch/claude" "$install_root/claude"

  # Native Codex CLI, driven directly in-process by `flycod` via `codex-acp`
  # over `codex app-server` (no external Node/Bun ACP adapter package).
  case "$artifact" in
    flycod-linux-x86_64)
      codex_target=x86_64-unknown-linux-musl
      expected_codex_hash=$codex_sha256_x64
      ;;
    flycod-linux-aarch64)
      codex_target=aarch64-unknown-linux-musl
      expected_codex_hash=$codex_sha256_aarch64
      ;;
  esac
  download "https://github.com/openai/codex/releases/download/$codex_version/codex-$codex_target.tar.gz" "$scratch/codex.tar.gz"
  printf '%s  %s\n' "$expected_codex_hash" "$scratch/codex.tar.gz" | sha256sum --check -
  tar xzf "$scratch/codex.tar.gz" -C "$scratch"
  install -m 0755 "$scratch/codex-$codex_target" "$install_root/codex"

  # Devin, from the manifest its own installer reads: the bundle is a
  # tarball carrying bin/devin, and the manifest carries its sha256.
  case "$artifact" in
    flycod-linux-x86_64) devin_target=x86_64-unknown-linux ;;
    flycod-linux-aarch64) devin_target=aarch64-unknown-linux ;;
  esac
  download https://static.devin.ai/cli/current/manifest.json "$scratch/devin-manifest.json"
  devin_meta=$(DEVIN_MANIFEST_JSON="$scratch/devin-manifest.json" DEVIN_TARGET="$devin_target" "$runtime_home/.bun/bin/bun" -e '
const manifest = JSON.parse(require("fs").readFileSync(process.env.DEVIN_MANIFEST_JSON, "utf8"));
const platform = manifest.platforms?.[process.env.DEVIN_TARGET];
if (!platform?.url || !platform.sha256) process.exit(1);
console.log(platform.url, platform.sha256);
')
  read devin_url devin_sha256 <<EOF
$devin_meta
EOF
  download "$devin_url" "$scratch/devin.tar.gz"
  printf '%s  %s\n' "$devin_sha256" "$scratch/devin.tar.gz" | sha256sum --check -
  mkdir "$scratch/devin"
  tar xzf "$scratch/devin.tar.gz" -C "$scratch/devin" bin/devin
  install -m 0755 "$scratch/devin/bin/devin" "$install_root/devin"

  # `node` and `npx` are the commands MCP server registrations name, and
  # there is no Node on this runtime: bunx runs the same packages, so the
  # aliases keep those servers working.
  ln -sf "$runtime_home/.bun/bin/bun" "$install_root/bunx"
  printf '#!/bin/sh\ncase "${1-}" in -y|--yes) shift ;; esac\nexec %s "$@"\n' "$install_root/bunx" >"$install_root/npx"
  printf '#!/bin/sh\nexec %s "$@"\n' "$runtime_home/.bun/bin/bun" >"$install_root/node"
  chmod 0755 "$install_root/npx" "$install_root/node"
}

verify_session_runtime() {
  "$install_root/flycod" --version
  command -v fish >/dev/null
  runuser -u "$runtime_user" -- env HOME=/home/flyco /home/flyco/.bun/bin/bun --version
  runuser -u "$runtime_user" -- env HOME=/home/flyco "$install_root/claude" --version
  runuser -u "$runtime_user" -- env HOME=/home/flyco "$install_root/codex" --version
  runuser -u "$runtime_user" -- env HOME=/home/flyco "$install_root/devin" --version
}

install_session() {
  install_session_runtime
  # cloud-init wrote the configuration as root before this ran; keep it
  # daemon-only (`0600 root:root`) so the unprivileged agent user (`flyco`)
  # can never read `daemon_token`, `github.token`, or harness credentials.
  chown root:root /etc/flycod/config.toml
  chmod 0600 /etc/flycod/config.toml
  install_flycod
  install_unit flycod.service
  systemctl daemon-reload
  verify_session_runtime
}

# A session container image. The daemon binary is a file the image build
# has on hand, there is no systemd, and the configuration is not on disk
# yet: the entrypoint writes it at start as the daemon principal (`root`,
# `0600 /etc/flycod/config.toml`), and `flycod run` drops privileges to the
# unprivileged `flyco` user whenever spawning harnesses, shells, or PTYs.
install_image() {
  install_session_runtime
  install -d -o root -g root -m 0700 /etc/flycod
  install_flycod_from "$1"
  verify_session_runtime
}

# ── A machine the user owns ──

install_host() {
  enrollment_token=$1

  export DEBIAN_FRONTEND=noninteractive
  apt-get update
  apt-get install --yes --no-install-recommends ca-certificates curl
  # Podman, and the two packages rootless containers need beside it:
  # `uidmap` provides newuidmap/newgidmap, `dbus-user-session` is what gives
  # the flyco user a session bus for its own systemd instance.
  if ! command -v podman >/dev/null 2>&1; then
    apt-get install --yes --no-install-recommends podman uidmap dbus-user-session
  fi

  # No login shell: this account exists to own containers, not to be logged
  # into, and the host is somebody's real machine.
  create_runtime_user /usr/sbin/nologin

  if ! grep -q "^$runtime_user:" /etc/subuid 2>/dev/null; then
    start=$(next_subid /etc/subuid)
    usermod --add-subuids "$start-$((start + subid_count - 1))" "$runtime_user"
  fi
  if ! grep -q "^$runtime_user:" /etc/subgid 2>/dev/null; then
    start=$(next_subid /etc/subgid)
    usermod --add-subgids "$start-$((start + subid_count - 1))" "$runtime_user"
  fi

  # Linger is what keeps the flyco user's systemd instance and its
  # /run/user/<uid> alive with nobody logged in, which is where rootless
  # Podman keeps its runtime state.
  loginctl enable-linger "$runtime_user"
  install -d -o "$runtime_user" -g "$runtime_group" -m 0755 "$volume_root"

  install_flycod

  # Root-only: the file `flycod host enroll` writes carries this machine's
  # host token.
  install -d -m 0700 "$host_config_dir"
  "$install_root/flycod" host enroll \
    --token "$enrollment_token" \
    --control-plane "$control_plane" \
    --volume-root "$volume_root" \
    --config "$host_config"

  install_unit flycod-host.service
  systemctl daemon-reload
  systemctl enable --now flycod-host.service

  "$install_root/flycod" --version
  runuser -u "$runtime_user" -- env HOME="$runtime_home" podman --version
  systemctl is-active flycod-host.service >/dev/null
}

case "${1:-}" in
  "") install_session ;;
  image)
    shift
    [ -n "${1:-}" ] && [ -f "$1" ] || usage
    install_image "$1"
    ;;
  host)
    shift
    [ "${1:-}" = "enroll" ] || usage
    shift
    [ -n "${1:-}" ] || usage
    install_host "$1"
    ;;
  *) usage ;;
esac
