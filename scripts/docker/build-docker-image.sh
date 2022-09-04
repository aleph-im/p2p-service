#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )"
ROOT_DIR="${SCRIPT_DIR}/../.."

docker build -f "${SCRIPT_DIR}/Dockerfile" -t alephim/p2p-service:latest "${ROOT_DIR}"
