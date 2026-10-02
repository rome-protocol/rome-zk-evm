#!/usr/bin/env bash
# deploy/rollup/tests/node_image_data_dirs.sh — every named volume a node service (the sequencer, derive, batcher)
# mounts must land on a directory the node image creates and gives to its own user. A fresh named volume takes the
# ownership of the image directory it is mounted on; on a path the image does not have, Docker creates it as root, and
# the node, which runs as the image's user, cannot write there (the sequencer then restarts in a loop, unable to open
# its database).
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
COMPOSE="$ROLLUP_DIR/docker-compose.yml"
DOCKERFILE="$REPO_ROOT/Dockerfile"
[[ -f "$COMPOSE" && -f "$DOCKERFILE" ]] || { fail "compose file and Dockerfile exist" "missing"; finish node_image_data_dirs; }

# The directories the image creates, and the ones it hands to its user (from the runtime stage's RUN line).
made="$(grep -oE 'mkdir -p [^&;]+' "$DOCKERFILE" | sed 's/^mkdir -p //' | tr ' ' '\n' | grep '^/')"
owned="$(grep -oE 'chown [a-z]+:[a-z]+ [^&;]+' "$DOCKERFILE" | sed -E 's/^chown [a-z]+:[a-z]+ //' | tr ' ' '\n' | grep '^/')"

# Named-volume mounts (`- name:/path[:ro]`, name not a file path) of the services that run the node image.
targets="$(awk '
  /^  [a-z-]+:$/ { svc = $1; sub(":", "", svc) }
  svc ~ /^(sequencer|derive|batcher)$/ && $1 == "-" && $2 ~ /^[a-z][a-z0-9-]*:\// {
    split($2, a, ":"); print svc " " a[2]
  }' "$COMPOSE")"
[[ -n "$targets" ]] || fail "the node services mount at least one named volume" "none found in $COMPOSE"

while read -r svc path; do
  [[ -n "$svc" ]] || continue
  if grep -qxF "$path" <<<"$made" && grep -qxF "$path" <<<"$owned"; then
    pass "$svc mounts a named volume at $path, which the image creates and gives to its user"
  else
    fail "$svc's named volume at $path is created and owned by the image" "add $path to the Dockerfile's mkdir -p and chown"
  fi
done <<<"$targets"
finish node_image_data_dirs
