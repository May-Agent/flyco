set -eu
podman rm --force --ignore {{ container }}
{%- if !keep_volume %}
if podman volume exists {{ volume }}; then
  podman volume rm --force {{ volume }}
fi
{%- endif %}
