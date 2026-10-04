#!/usr/bin/env bash
# Stop and remove the dev container (named volumes with build caches are kept).
export MSYS_NO_PATHCONV=1
docker rm -f "${CINDER_DEV_CONTAINER:-cinder-dev}" >/dev/null 2>&1 && echo removed || echo "not running"
