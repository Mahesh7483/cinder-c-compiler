#!/usr/bin/env bash
# Run a command inside the dev container, from the repo root (/work).
set -euo pipefail
export MSYS_NO_PATHCONV=1
exec docker exec -w /work "${CINDER_DEV_CONTAINER:-cinder-dev}" bash -c "$*"
