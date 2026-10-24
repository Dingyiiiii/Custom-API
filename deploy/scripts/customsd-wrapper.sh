#!/bin/sh
#
# Launcher for customsd, used by /usr/local/etc/rc.d/customsd.
#
# It assembles the gateway's argument list so that the rc.d script can hand
# daemon(8) a single, complete child command: daemon(8) accepts its own options
# and then the child with that child's arguments, so the two option groups
# cannot be interleaved on one line. Keeping the assembly here also keeps the
# rc.d script readable and gives one place to fail loudly on a missing file.
#
# Privilege dropping is daemon(8)'s job (`-u`), not this script's: the service
# user deliberately has a nologin shell, so su(1) would refuse to run it.
#
# Usage:
#   customsd-wrapper -c CONFIG [-e ENVFILE] [extra gateway flags...]

set -eu

BIN=/usr/local/sbin/customsd
CONFIG=
ENVFILE=
EXTRA=""

while [ $# -gt 0 ]; do
	case "$1" in
	-b) BIN="$2"; shift 2 ;;
	-c) CONFIG="$2"; shift 2 ;;
	-e) ENVFILE="$2"; shift 2 ;;
	*) EXTRA="${EXTRA} $1"; shift ;;
	esac
done

if [ -z "${CONFIG}" ]; then
	echo "customsd-wrapper: -c CONFIG is required" >&2
	exit 1
fi
if [ ! -f "${CONFIG}" ]; then
	echo "customsd-wrapper: configuration not found: ${CONFIG}" >&2
	exit 1
fi
if [ ! -x "${BIN}" ]; then
	echo "customsd-wrapper: binary not executable: ${BIN}" >&2
	exit 1
fi

set -- "${BIN}" --config "${CONFIG}"
if [ -n "${ENVFILE}" ]; then
	set -- "$@" --env-file "${ENVFILE}"
fi

# Extra gateway flags are passed through unquoted on purpose: they are a
# pre-split list, and quoting them would deliver them as a single argument.
# shellcheck disable=SC2086
exec "$@" ${EXTRA}
