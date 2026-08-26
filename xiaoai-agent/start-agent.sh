#!/bin/sh

set -u

AGENT_HOME=${XIAOAI_AGENT_HOME:-/data/open-xiaoai}
AGENT_BIN=${XIAOAI_AGENT_BIN:-$AGENT_HOME/xiaoai-agent}
AGENT_CONFIG=${XIAOAI_AGENT_CONFIG:-$AGENT_HOME/agent.yaml}
AGENT_LOG=${XIAOAI_AGENT_LOG:-$AGENT_HOME/xiaoai-agent.log}
RUNTIME_DIR=${XIAOAI_RUNTIME_DIR:-/tmp/xiaoai-agent}
PID_FILE=$RUNTIME_DIR/xiaoai-agent.pid
WATCHDOG_PID_FILE=$RUNTIME_DIR/watchdog.pid
LOCK_DIR=$RUNTIME_DIR/launcher.lock
PATCHED_PNS=$RUNTIME_DIR/pns
PNS_INIT=${XIAOAI_PNS_INIT:-/etc/init.d/pns}
MICO_INIT=${XIAOAI_MICO_INIT:-/etc/init.d/mico_aivs_lab}
PROC_MOUNTS=${XIAOAI_PROC_MOUNTS:-/proc/mounts}
SPEECH_DIR=${XIAOAI_SPEECH_DIR:-/tmp/mico_aivs_lab/usock}
SPEECH_SOCKET=$SPEECH_DIR/speech.usock
NATIVE_SPEECH_SOCKET=$SPEECH_DIR/speech.native.usock
COMMON_SOCKET=$SPEECH_DIR/common.usock
FULL_DUPLEX=${XIAOAI_FULL_DUPLEX:-/data/mipns/dialog_continuous}
DISABLED_FULL_DUPLEX=${XIAOAI_DISABLED_FULL_DUPLEX:-/data/mipns/dialog_continuous.xiaoai-agent-disabled}
START_TIMEOUT=${XIAOAI_START_TIMEOUT:-30}
WATCHDOG_INTERVAL=${XIAOAI_WATCHDOG_INTERVAL:-5}
PNS_PROCESS_NAMES="mipns-xiaomi mipns-sai mipns-gmems mipns-siot mipns-aispeech mipns-horizon mipns-nuance"
RUST_LOG=${RUST_LOG:-info}
case "$0" in
    /*) LAUNCHER=$0 ;;
    *) LAUNCHER=$(pwd)/$0 ;;
esac

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

path_inode() {
    path=$1
    # BusyBox on the tested firmware does not provide stat(1).
    # shellcheck disable=SC2012
    ls -id "$path" 2>/dev/null | awk '{ print $1 }'
}

socket_inode() {
    path_inode "$SPEECH_SOCKET"
}

path_is_mounted() {
    target=$1
    [ -r "$PROC_MOUNTS" ] || return 1
    awk -v target="$target" '$2 == target { found = 1 } END { exit !found }' "$PROC_MOUNTS"
}

pns_is_overlaid() {
    path_is_mounted "$PNS_INIT"
}

speech_is_protected() {
    path_is_mounted "$SPEECH_SOCKET"
}

pns_pids() {
    for name in $PNS_PROCESS_NAMES; do
        pidof "$name" 2>/dev/null || true
    done
}

stop_pns() {
    "$PNS_INIT" stop >/dev/null 2>&1 || true
    for name in $PNS_PROCESS_NAMES; do
        killall "$name" 2>/dev/null || true
    done
    i=0
    while [ "$i" -lt 5 ]; do
        [ -z "$(pns_pids)" ] && return 0
        sleep 1
        i=$((i + 1))
    done
    for name in $PNS_PROCESS_NAMES; do
        killall -9 "$name" 2>/dev/null || true
    done
    [ -z "$(pns_pids)" ]
}

mico_pids() {
    pidof mico_aivs_lab 2>/dev/null || true
}

stop_mico() {
    "$MICO_INIT" stop >/dev/null 2>&1 || true
    killall mico_aivs_lab 2>/dev/null || true
    i=0
    while [ "$i" -lt 10 ]; do
        [ -z "$(mico_pids)" ] && return 0
        sleep 1
        i=$((i + 1))
    done
    killall -9 mico_aivs_lab 2>/dev/null || true
    [ -z "$(mico_pids)" ]
}

wait_for_pcm_frontend() {
    i=0
    while [ "$i" -lt "$START_TIMEOUT" ]; do
        pids=$(pns_pids)
        if [ -n "$pids" ]; then
            for pid in $pids; do
                if tr '\000' ' ' <"/proc/$pid/cmdline" | grep -q 'opus32'; then
                    fail "pns started an Opus frontend instead of PCM"
                    return 1
                fi
            done
            return 0
        fi
        sleep 1
        i=$((i + 1))
    done
    fail "pns did not start a speech frontend after ${START_TIMEOUT}s"
}

wait_for_native_services() {
    old_common_inode=${1:-}
    old_speech_inode=${2:-}
    i=0
    while [ "$i" -lt "$START_TIMEOUT" ]; do
        common_inode=$(path_inode "$COMMON_SOCKET" || true)
        speech_inode=$(socket_inode || true)
        if [ -n "$(mico_pids)" ] &&
            [ -S "$COMMON_SOCKET" ] && [ -S "$SPEECH_SOCKET" ] &&
            [ -n "$common_inode" ] && [ "$common_inode" != "$old_common_inode" ] &&
            [ -n "$speech_inode" ] && [ "$speech_inode" != "$old_speech_inode" ]; then
            return 0
        fi
        sleep 1
        i=$((i + 1))
    done
    fail "native mico services were not recreated after ${START_TIMEOUT}s"
}

start_native_mico() {
    old_common_inode=$(path_inode "$COMMON_SOCKET" || true)
    old_speech_inode=$(socket_inode || true)
    "$MICO_INIT" start >/dev/null 2>&1 || return 1
    wait_for_native_services "$old_common_inode" "$old_speech_inode"
}

start_protected_mico() {
    expected_speech_inode=$1
    old_common_inode=$(path_inode "$COMMON_SOCKET" || true)
    "$MICO_INIT" start >/dev/null 2>&1 || return 1
    i=0
    while [ "$i" -lt "$START_TIMEOUT" ]; do
        common_inode=$(path_inode "$COMMON_SOCKET" || true)
        speech_inode=$(socket_inode || true)
        if [ -n "$(mico_pids)" ] && [ -S "$COMMON_SOCKET" ] &&
            [ -n "$common_inode" ] && [ "$common_inode" != "$old_common_inode" ] &&
            speech_is_protected && [ "$speech_inode" = "$expected_speech_inode" ]; then
            return 0
        fi
        sleep 1
        i=$((i + 1))
    done
    fail "mico common.usock did not start without replacing protected speech.usock"
}

wait_for_agent_socket() {
    pid=$1
    i=0
    while [ "$i" -lt "$START_TIMEOUT" ]; do
        kill -0 "$pid" 2>/dev/null || return 1
        [ -S "$SPEECH_SOCKET" ] && return 0
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

stop_watchdog() {
    watchdog_pid=$(cat "$WATCHDOG_PID_FILE" 2>/dev/null || true)
    if [ -n "$watchdog_pid" ] && [ "$watchdog_pid" != "$$" ]; then
        kill "$watchdog_pid" 2>/dev/null || true
    fi
    rm -f "$WATCHDOG_PID_FILE"
}

start_watchdog() {
    agent_pid=$1
    expected_inode=$(socket_inode) || return 1
    [ -n "$expected_inode" ] || return 1
    (
        trap '' HUP
        trap 'exit 0' INT TERM
        while sleep "$WATCHDOG_INTERVAL"; do
            reason=
            if ! kill -0 "$agent_pid" 2>/dev/null; then
                reason="agent process exited"
            elif ! speech_is_protected; then
                reason="speech.usock protection mount disappeared"
            else
                current_inode=$(socket_inode || true)
                if [ "$current_inode" != "$expected_inode" ]; then
                    reason="speech.usock was replaced"
                fi
            fi
            [ -n "$reason" ] || continue
            log "$reason; restarting the runtime takeover"
            rm -f "$WATCHDOG_PID_FILE"
            "$LAUNCHER" restart >>"$AGENT_HOME/xiaoai-agent-launcher.log" 2>&1
            exit
        done
    ) </dev/null >>"$AGENT_HOME/xiaoai-agent-launcher.log" 2>&1 &
    printf '%s\n' "$!" >"$WATCHDOG_PID_FILE"
}

protect_speech_socket() {
    [ -S "$SPEECH_SOCKET" ] || return 1
    speech_is_protected && return 0
    mount -o bind "$SPEECH_SOCKET" "$SPEECH_SOCKET" || return 1
    speech_is_protected
}

unprotect_speech_socket() {
    i=0
    while speech_is_protected; do
        umount "$SPEECH_SOCKET" || return 1
        i=$((i + 1))
        [ "$i" -lt 8 ] || return 1
    done
    return 0
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
    if ! stop_pns; then
        fail "failed to stop pns before restoring native services"
        return 1
    fi
    stop_watchdog
    if ! stop_mico; then
        fail "failed to stop mico before removing speech protection"
        return 1
    fi
    stop_agent_process
    if ! unprotect_speech_socket; then
        fail "failed to remove speech.usock protection"
        return 1
    fi
    if ! restore_native_files; then
        fail "failed to restore native speech files"
        return 1
    fi
    if ! start_native_mico; then
        fail "failed to restart native mico services"
        return 1
    fi
    if ! "$PNS_INIT" start >/dev/null 2>&1; then
        fail "failed to restart native pns"
        return 1
    fi
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
    stop_pns || true
    stop_watchdog
    stop_mico || true
    stop_agent_process
    unprotect_speech_socket || true
    restore_native_files || true
    start_native_mico || true
    "$PNS_INIT" start >/dev/null 2>&1 || true
}

stop_takeover() {
    stop_pns || return 1
    stop_watchdog
    stop_mico || return 1
    stop_agent_process
    unprotect_speech_socket || return 1
    restore_native_files || return 1
    [ -z "$(pns_pids)" ] || return 1
    [ -z "$(mico_pids)" ] || return 1
    ! pid_is_running || return 1
    ! speech_is_protected
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
    if pid_is_running && pns_is_overlaid && speech_is_protected; then
        log "already running with PID $(cat "$PID_FILE")"
        return 0
    fi
    rm -f "$PID_FILE"

    [ -x "$AGENT_BIN" ] || fail "agent binary is not executable: $AGENT_BIN" || return 1
    [ -r "$AGENT_CONFIG" ] || fail "agent config is not readable: $AGENT_CONFIG" || return 1
    [ -x "$PNS_INIT" ] || fail "pns init script is not executable: $PNS_INIT" || return 1
    [ -x "$MICO_INIT" ] || fail "mico init script is not executable: $MICO_INIT" || return 1

    if pid_is_running || pns_is_overlaid || speech_is_protected || [ -e "$NATIVE_SPEECH_SOCKET" ]; then
        log "cleaning up a previous runtime takeover"
    fi
    if ! stop_takeover; then
        fail "failed to clean up the previous runtime takeover"
        return 1
    fi

    patch_pns_runtime || {
        rollback_start
        fail "failed to install the runtime pns override"
        return 1
    }
    if ! rm -f "$SPEECH_SOCKET"; then
        rollback_start
        fail "failed to remove the stopped native speech socket"
        return 1
    fi

    log "starting $AGENT_BIN"
    RUST_LOG="$RUST_LOG" "$AGENT_BIN" -c "$AGENT_CONFIG" >>"$AGENT_LOG" 2>&1 &
    agent_pid=$!
    printf '%s\n' "$agent_pid" >"$PID_FILE"

    if ! wait_for_agent_socket "$agent_pid"; then
        rollback_start
        fail "agent did not take over speech.usock within ${START_TIMEOUT}s; see $AGENT_LOG"
        return 1
    fi
    expected_inode=$(socket_inode || true)
    if [ -z "$expected_inode" ] || ! protect_speech_socket; then
        rollback_start
        fail "failed to protect speech.usock with a bind mount"
        return 1
    fi
    if [ "$(socket_inode || true)" != "$expected_inode" ]; then
        rollback_start
        fail "speech.usock changed while installing its protection mount"
        return 1
    fi
    if ! start_protected_mico "$expected_inode"; then
        rollback_start
        return 1
    fi
    if ! "$PNS_INIT" start >/dev/null 2>&1; then
        rollback_start
        fail "failed to start pns with the PCM runtime override"
        return 1
    fi
    if ! wait_for_pcm_frontend; then
        rollback_start
        return 1
    fi
    sleep 2
    if ! kill -0 "$agent_pid" 2>/dev/null; then
        rollback_start
        fail "agent exited after the PCM frontend started; see $AGENT_LOG"
        return 1
    fi
    if ! start_watchdog "$agent_pid"; then
        rollback_start
        fail "failed to start the speech socket watchdog"
        return 1
    fi

    log "started with PID $agent_pid"
}

stop_agent() {
    acquire_lock || return 1
    if ! pid_is_running && ! pns_is_overlaid && ! speech_is_protected && [ ! -e "$NATIVE_SPEECH_SOCKET" ]; then
        rm -f "$PID_FILE"
        log "already stopped"
        return 0
    fi
    restore_native_frontend || return 1
    log "stopped and restored the native speech frontend"
}

status_agent() {
    if pid_is_running && pns_is_overlaid && speech_is_protected; then
        log "running with PID $(cat "$PID_FILE")"
        return 0
    fi
    if pid_is_running; then
        log "agent process is running without a complete runtime takeover"
        return 1
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
        acquire_lock && stop_takeover && release_lock && start_agent
        ;;
    status)
        status_agent
        ;;
    *)
        echo "usage: $0 {start|stop|restart|status}" >&2
        exit 2
        ;;
esac
