#!/bin/bash
# Starts the Aleph.im P2P service.

set -euo pipefail

exec aleph-p2p-service "$@"
