#!/usr/bin/env bash
set -u

run_directory=$1
# Konsole expands environment variables in PTY arguments. Decode the exact
# settings here so quoting and expansion happen only in the configured Bash command.
printf -v directory '%b' "${2//%/\\x}"
printf -v command_line '%b' "${3//%/\\x}"

case "$directory" in
  \~) directory=$HOME ;;
  \~/*) directory=$HOME/${directory:2} ;;
  /*) ;;
  *) exit 64 ;;
esac
cd -- "$directory" || exit 64
entry='export SSMCP_SESSION_ACTIVITY_PID=$$; exec -- '"$command_line"
bash -n -c "$entry" || exit 66

umask 077
mkdir -m 700 -- "$run_directory" || exit 67
trap 'rm -f -- "$run_directory/calls.json" "$run_directory/calls.tmp"; rmdir -- "$run_directory"' EXIT
export SSMCP_SESSION_ACTIVITY_PATH="$run_directory/calls.json"

# A locally built widget CLI can be installed without replacing a packaged ssmcp.
bin="${XDG_DATA_HOME:-$HOME/.local/share}/ssmcp-omarchy/bin"
if [[ -x $bin/ssmcp ]]; then
  PATH="$bin:$PATH"
  export PATH
fi

# Preserve PTY input for the child while the supervisor waits for either exit
# or the widget's stop signal. The CLI handles graceful session shutdown.
bash -c "$entry" <&0 &
client_pid=$!

trap 'kill -TERM "$client_pid" 2>/dev/null; wait "$client_pid" 2>/dev/null; exit 0' HUP INT TERM
wait "$client_pid"
