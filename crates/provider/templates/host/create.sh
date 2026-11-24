set -eu
podman rm --force --ignore {{ container }} </dev/null
podman volume exists {{ volume }} </dev/null || podman volume create {{ volume }} </dev/null
podman run --detach \
  --name {{ container }} \
  --volume {{ volume }}:{{ workdir }} \
  --label flyco.machine={{ machine }} \
  --label flyco.session={{ session }} \
  --restart on-failure \
  --env-file /dev/stdin \
  {{ image }}
