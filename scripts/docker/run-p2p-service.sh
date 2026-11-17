#!/bin/bash
# Starts the Aleph.im P2P service.

set -euo pipefail

# Raise the soft file-descriptor limit to the hard limit when the container was
# given extra headroom at creation time (docker run --ulimit nofile=...). Every
# libp2p, gRPC and HTTP connection holds a descriptor.
ulimit -n "$(ulimit -Hn)" 2>/dev/null || true

exec aleph-p2p-service "$@"
