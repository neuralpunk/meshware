#!/usr/bin/env bash
# Exercise Director's SSH client against a real sshd, without root.
#
# Starts a throwaway OpenSSH server as *this* user on a high port, with a
# fresh host key and a fresh client key, then runs the ignored
# `ssh::tests::a_session_runs_commands_feeds_stdin_and_streams_lines` test
# against it. Nothing touches /etc, ~/.ssh, or a running sshd.
#
#   ./scripts/test-ssh-local.sh            # port 2222 by default
#   PORT=2299 ./scripts/test-ssh-local.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${PORT:-2222}"
SSHD="$(command -v sshd || echo /usr/sbin/sshd)"
[ -x "$SSHD" ] || { echo "no sshd binary found (install openssh-server)"; exit 1; }

DIR="$(mktemp -d)"
trap 'kill "$PID" 2>/dev/null || true; rm -rf "$DIR"' EXIT
ssh-keygen -q -t ed25519 -N "" -f "$DIR/host_key"
ssh-keygen -q -t ed25519 -N "" -f "$DIR/client_key"
cp "$DIR/client_key.pub" "$DIR/authorized_keys"
chmod 600 "$DIR/authorized_keys"
cat > "$DIR/sshd_config" <<CFG
Port $PORT
ListenAddress 127.0.0.1
HostKey $DIR/host_key
PidFile $DIR/sshd.pid
AuthorizedKeysFile $DIR/authorized_keys
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
UsePAM no
StrictModes no
LogLevel ERROR
CFG
"$SSHD" -f "$DIR/sshd_config" -D -e &
PID=$!
for _ in $(seq 1 30); do
  if (echo > /dev/tcp/127.0.0.1/"$PORT") 2>/dev/null; then break; fi
  sleep 0.2
done

# Director's known_hosts store lives under its data dir; point that at
# the temp dir so the test records the throwaway host key there.
export MESHWARE_HOME="$DIR/home"
export MESHWARE_TEST_SSH="$(id -un)@127.0.0.1:$PORT"
export MESHWARE_TEST_SSH_KEY="$DIR/client_key"
cd "$ROOT"
cargo test -p meshware-director --lib a_session_runs_commands_feeds_stdin_and_streams_lines -- --ignored --nocapture
echo
echo "host key recorded at: $MESHWARE_HOME/director/known_hosts"
cat "$MESHWARE_HOME/director/known_hosts" 2>/dev/null || true
