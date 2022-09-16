#!/bin/bash
# Starts the Aleph.im P2P service.
# Checks that RabbitMQ is launched before launching the service, using wait-for-it.sh.
# This script expects ry to be installed (`cargo install ry`) to retrieve YAML configuration options.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"

P2P_SERVICE_ARGS=("$@")

while test $# -gt 0; do
  case "$1" in
  --help)
    help
    ;;
  --config)
    CONFIG_FILE="$2"
    shift
    ;;
  esac
  shift
done

function wait_for_it() {
  "${SCRIPT_DIR}"/wait-for-it.sh "$@"
}

function get_config() {
  config_key="$1"
  config_value=$(ry "${CONFIG_FILE}" "${config_key}")
  echo "${config_value}"
}

RABBITMQ_HOST=$(get_config rabbitmq.host)
RABBITMQ_PORT=$(get_config rabbitmq.port)

wait_for_it -h "${RABBITMQ_HOST}" -p "${RABBITMQ_PORT}"

exec aleph-p2p-service "${P2P_SERVICE_ARGS[@]}"
