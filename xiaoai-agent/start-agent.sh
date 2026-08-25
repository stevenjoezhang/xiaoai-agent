#!/bin/sh

set -u

AGENT_HOME=${XIAOAI_AGENT_HOME:-/data/open-xiaoai}
AGENT_BIN=${XIAOAI_AGENT_BIN:-$AGENT_HOME/xiaoai-agent}
AGENT_CONFIG=${XIAOAI_AGENT_CONFIG:-$AGENT_HOME/agent.yaml}
AGENT_LOG=${XIAOAI_AGENT_LOG:-$AGENT_HOME/xiaoai-agent.log}
RUNTIME_DIR=${XIAOAI_RUNTIME_DIR:-/tmp/xiaoai-agent}
PID_FILE=$RUNTIME_DIR/xiaoai-agent.pid
LOCK_DIR=$RUNTIME_DIR/launcher.lock
PATCHED_PNS=$RUNTIME_DIR/pns
PNS_INIT=${XIAOAI_PNS_INIT:-/etc/init.d/pns}
PROC_MOUNTS=${XIAOAI_PROC_MOUNTS:-/proc/mounts}
SPEECH_DIR=${XIAOAI_SPEECH_DIR:-/tmp/mico_aivs_lab/usock}
SPEECH_SOCKET=$SPEECH_DIR/speech.usock
NATIVE_SPEECH_SOCKET=$SPEECH_DIR/speech.native.usock
COMMON_SOCKET=$SPEECH_DIR/common.usock
FULL_DUPLEX=${XIAOAI_FULL_DUPLEX:-/data/mipns/dialog_continuous}
DISABLED_FULL_DUPLEX=${XIAOAI_DISABLED_FULL_DUPLEX:-/data/mipns/dialog_continuous.xiaoai-agent-disabled}
START_TIMEOUT=${XIAOAI_START_TIMEOUT:-30}
RUST_LOG=${RUST_LOG:-info}

log() {
    printf '%s\n' "xiaoai-agent launcher: $*"
}

fail() {
    log "error: $*" >&2
    return 1
}

pid_is_running() {
    [ -f "$PID_FILE" ] || return 1
    pid=$(cat "$PID_FILE" 2>/dev/null) || return 1
    [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null
}

pns_is_overlaid() {
    [ -r "$PROC_MOUNTS" ] || return 1
    awk -v target="$PNS_INIT" '$2 == target { found = 1 } END { exit !found }' "$PROC_MOUNTS"
}

wait_for_native_services() {
    i=0
    while [ "$i" -lt "$START_TIMEOUT" ]; do
        [ -S "$COMMON_SOCKET" ] && [ -S "$SPEECH_SOCKET" ] && return 0
        sleep 1
        i=$((i + 1))
    done
    fail "native common.usock and speech.usock were not ready after ${START_TIMEOUT}s"
}

wait_for_agent_socket() {
    pid=$1
    i=0
    while [ "$i" -lt "$START_TIMEOUT" ]; do
        kill -0 "$pid" 2>/dev/null || return 1
        [ -S "$NATIVE_SPEECH_SOCKET" ] && [ -S "$SPEECH_SOCKET" ] && return 0
        sleep 1
        i=$((i + 1))
    done
    return 1
}

stop_agent_process() {
    pids=$(cat "$PID_FILE" 2>/dev/null || true)
    if [ -z "$pids" ] && command -v pidof >/dev/null 2>&1; then
        pids=$(pidof "$(basename "$AGENT_BIN")" 2>/dev/null || true)
    fi
    for pid in $pids; do
        kill -0 "$pid" 2>/dev/null || continue
        kill "$pid" 2>/dev/null || true
        i=0
        while kill -0 "$pid" 2>/dev/null && [ "$i" -lt 10 ]; do
            sleep 1
            i=$((i + 1))
        done
        kill -9 "$pid" 2>/dev/null || true
    done
    rm -f "$PID_FILE"
}

restore_native_files() {
    if pns_is_overlaid; then
        umount "$PNS_INIT" || return 1
    fi
    if [ -e "$NATIVE_SPEECH_SOCKET" ]; then
        rm -f "$SPEECH_SOCKET"
        mv "$NATIVE_SPEECH_SOCKET" "$SPEECH_SOCKET" || return 1
    fi
    if [ -e "$DISABLED_FULL_DUPLEX" ] && [ ! -e "$FULL_DUPLEX" ]; then
        mv "$DISABLED_FULL_DUPLEX" "$FULL_DUPLEX" || return 1
    fi
    return 0
}

restore_native_frontend() {
    "$PNS_INIT" stop >/dev/null 2>&1 || true
    stop_agent_process
    restore_native_files || fail "failed to restore native speech files"
    "$PNS_INIT" start >/dev/null 2>&1 || fail "failed to restart native pns"
}

patch_pns_runtime() {
    sed 's/[[:space:]]-r[[:space:]]opus32//g' "$PNS_INIT" >"$PATCHED_PNS" || return 1
    if grep -q -e '-r[[:space:]]*opus32' "$PATCHED_PNS"; then
        fail "failed to remove the Opus codec argument from pns"
        return 1
    fi
    chmod +x "$PATCHED_PNS" || return 1
    mount -o bind "$PATCHED_PNS" "$PNS_INIT" || return 1
}

rollback_start() {
    log "startup failed; restoring the native speech frontend"
    "$PNS_INIT" stop >/dev/null 2>&1 || true
    stop_agent_process
    restore_native_files || true
    "$PNS_INIT" start >/dev/null 2>&1 || true
}

acquire_lock() {
    mkdir -p "$RUNTIME_DIR"
    if mkdir "$LOCK_DIR" 2>/dev/null; then
        trap 'rmdir "$LOCK_DIR" 2>/dev/null || true' EXIT INT TERM
        return 0
    fi
    fail "another launcher operation is in progress"
}

release_lock() {
    trap - EXIT INT TERM
    rmdir "$LOCK_DIR" 2>/dev/null || true
}

start_agent() {
    acquire_lock || return 1
    if pid_is_running; then
        log "already running with PID $(cat "$PID_FILE")"
        return 0
    fi
    rm -f "$PID_FILE"

    [ -x "$AGENT_BIN" ] || fail "agent binary is not executable: $AGENT_BIN" || return 1
    [ -r "$AGENT_CONFIG" ] || fail "agent config is not readable: $AGENT_CONFIG" || return 1
    [ -x "$PNS_INIT" ] || fail "pns init script is not executable: $PNS_INIT" || return 1

    if pns_is_overlaid || [ -e "$NATIVE_SPEECH_SOCKET" ]; then
        log "cleaning up a previous runtime takeover"
        restore_native_frontend || return 1
    fi

    wait_for_native_services || return 1
    "$PNS_INIT" stop >/dev/null 2>&1 || {
        fail "failed to stop pns"
        return 1
    }
    patch_pns_runtime || {
        rollback_start
        fail "failed to install the runtime pns override"
        return 1
    }

    log "starting $AGENT_BIN"
    RUST_LOG="$RUST_LOG" "$AGENT_BIN" -c "$AGENT_CONFIG" >>"$AGENT_LOG" 2>&1 &
    agent_pid=$!
    printf '%s\n' "$agent_pid" >"$PID_FILE"

    if ! wait_for_agent_socket "$agent_pid"; then
        rollback_start
        fail "agent did not take over speech.usock within ${START_TIMEOUT}s; see $AGENT_LOG"
        return 1
    fi
    if ! "$PNS_INIT" start >/dev/null 2>&1; then
        rollback_start
        fail "failed to start pns with the PCM runtime override"
        return 1
    fi

    log "started with PID $agent_pid"
}

stop_agent() {
    acquire_lock || return 1
    if ! pid_is_running && ! pns_is_overlaid && [ ! -e "$NATIVE_SPEECH_SOCKET" ]; then
        rm -f "$PID_FILE"
        log "already stopped"
        return 0
    fi
    restore_native_frontend || return 1
    log "stopped and restored the native speech frontend"
}

status_agent() {
    if pid_is_running; then
        log "running with PID $(cat "$PID_FILE")"
        return 0
    fi
    log "not running"
    return 1
}

command=${1:-start}
case "$command" in
    start)
        start_agent
        ;;
    stop)
        stop_agent
        ;;
    restart)
        stop_agent && release_lock && start_agent
        ;;
    status)
        status_agent
        ;;
    *)
        echo "usage: $0 {start|stop|restart|status}" >&2
        exit 2
        ;;
esac
