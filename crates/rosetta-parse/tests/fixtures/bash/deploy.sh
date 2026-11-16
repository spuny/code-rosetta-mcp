#!/usr/bin/env bash
# Synthetic deployment helper.
set -euo pipefail

export APP_NAME=demo
export REGION
declare -x STAGE=prod
declare LOCAL_ONLY=1
readonly VERSION=1.2.3
COUNT=0

source ./lib/common.sh
. "config/env.sh"
source "$HERE/dynamic.sh"
source "${ROOT}/other.sh"

log() {
  echo "$@"
}

function build {
  log "building"
  if [ -f Makefile ]; then
    make_it | tee out.txt
  fi
}

make_it() { log "make"; }

deploy() (
  build && log done || log failed
  for host in a b; do
    push "$host"
  done
  while read -r line; do
    result=$(summarize "$line")
  done < input
  case "$1" in
    x) cleanup ;;
  esac
)

push() { log "push $1"; }
summarize() { log "sum"; }
cleanup() { log "bye"; cleanup_later; }
cleanup_later() { :; }

main() {
  setup
  deploy
}

setup() { log "setup"; }

main "$@"
if [ -n "${FORCE:-}" ]; then
  deploy
fi
