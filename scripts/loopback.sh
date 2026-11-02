#!/usr/bin/env bash
# Give this machine the loopback addresses the transport's tests put their
# machines at, 127.0.0.2 to 127.0.0.9 (DESIGN.md §6.1, §14.2).
#
#   scripts/loopback.sh
#
# The fake Tailscale gives every machine on one host its own loopback
# address, as Tailscale gives every machine its own IP, so that the address
# a connection comes from says which machine dialed. Linux answers all of
# 127.0.0.0/8 already, so there the script does nothing. macOS answers only
# 127.0.0.1 until each address is aliased on lo0, which needs sudo; the
# aliases last until the Mac restarts.

set -euo pipefail

case "$(uname -s)" in
Darwin)
  for n in 2 3 4 5 6 7 8 9; do
    sudo ifconfig lo0 alias "127.0.0.$n" up
  done
  ;;
Linux) ;;
*)
  echo "loopback.sh: delocal runs on Linux and macOS only" >&2
  exit 1
  ;;
esac
