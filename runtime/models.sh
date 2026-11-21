#!/usr/bin/env bash
# Installs and runs findery's embedding runtime on Linux: llama.cpp behind
# llama-swap, every model on one local port.
#
# Everything lands in the runtime directory
# (${XDG_DATA_HOME:-~/.local/share}/findery/runtime, or FINDERY_RUNTIME_DIR):
# the two pinned binaries, llama-swap.yaml, the logs, and a copy of this script
# as findery-models, so the runtime keeps working without the repository. The
# models go to the LM Studio library when there is one (~/.lmstudio/models),
# else to <runtime>/models.
#
# Versions and sha256 come from pins.txt; every download is checked before it
# is unpacked, and a step already done is skipped, so `install` can be run
# again at any time.
#
#   models.sh install [--backend vulkan|cpu] [--models DIR]
#   models.sh pull | start | stop | restart | status | ui | logs | devices
#   models.sh autostart on|off      a systemd user service, started at login
#   models.sh uninstall             the binaries go; the config and models stay

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUNTIME="${FINDERY_RUNTIME_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/findery/runtime}"
LISTEN="${FINDERY_MODELS_LISTEN:-127.0.0.1:8090}"
BASE_URL="http://$LISTEN"
CONFIG="$RUNTIME/llama-swap.yaml"
STATE="$RUNTIME/runtime.env"
LOG="$RUNTIME/logs/llama-swap.log"
PIDFILE="$RUNTIME/llama-swap.pid"
UNIT_NAME="findery-models.service"
UNIT="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/$UNIT_NAME"

say() { printf '%s\n' "$*"; }
die() { printf 'models: %s\n' "$*" >&2; exit 1; }

# --- pins ----------------------------------------------------------------------

pin() { # pin <kind> <name> <column>: one field of pins.txt
  awk -v k="$1" -v n="$2" -v c="$3" '$1 == k && $2 == n { print $c; exit }' "$HERE/pins.txt"
}
models_pinned() { awk '$1 == "model" { print $2, $3, $4 }' "$HERE/pins.txt"; }

LLAMA_CPP="$(pin version llama.cpp 3)"
SWAP_VER="$(pin version llama-swap 3)"
SERVER="$RUNTIME/llama.cpp/$LLAMA_CPP/llama-server"
SWAP="$RUNTIME/llama-swap/v$SWAP_VER/llama-swap"

# --- state: what `install` chose, read back by every other verb ----------------

BACKEND=""
MODELS_DIR=""
state() { [ -f "$STATE" ] && grep -E "^$1=" "$STATE" | head -1 | cut -d= -f2- || true; }

backend() {
  [ -n "$BACKEND" ] && { say "$BACKEND"; return; }
  local saved; saved="$(state BACKEND)"
  say "${saved:-vulkan}"
}

models_dir() {
  [ -n "$MODELS_DIR" ] && { say "$MODELS_DIR"; return; }
  local saved; saved="$(state MODELS_DIR)"
  if [ -n "$saved" ]; then say "$saved"
  elif [ -d "$HOME/.lmstudio/models" ]; then say "$HOME/.lmstudio/models"
  else say "$RUNTIME/models"
  fi
}

# --- downloads -----------------------------------------------------------------

need() { command -v "$1" >/dev/null 2>&1 || die "$1 is required (install it with your package manager)"; }

verified() { # verified <url> <sha256> <out>
  local url="$1" sha="$2" out="$3" partial="$3.partial"
  if [ -f "$out" ] && [ "$(sha256sum "$out" | cut -d' ' -f1)" = "$sha" ]; then return; fi
  mkdir -p "$(dirname "$out")"
  say "  downloading $url"
  curl -L --fail --retry 3 -C - --progress-bar -o "$partial" "$url"
  local got; got="$(sha256sum "$partial" | cut -d' ' -f1)"
  if [ "$got" != "$sha" ]; then
    rm -f "$partial"
    die "sha256 mismatch for $url
  expected $sha
  got      $got"
  fi
  mv -f "$partial" "$out"
}

unpack_into() { # unpack_into <archive> <binary> <dest>: the directory holding <binary> becomes <dest>
  local archive="$1" binary="$2" dest="$3" tmp
  tmp="$(mktemp -d "$RUNTIME/.unpack-XXXXXX")"
  tar -xzf "$archive" -C "$tmp"
  local found; found="$(find "$tmp" -type f -name "$binary" | head -1)"
  [ -n "$found" ] || { rm -rf "$tmp"; die "$binary not found in $archive"; }
  rm -rf "$dest"
  mkdir -p "$(dirname "$dest")"
  mv "$(dirname "$found")" "$dest"
  rm -rf "$tmp"
}

install_binaries() {
  local b="$1" cache="$RUNTIME/downloads" asset
  [ "$(uname -m)" = "x86_64" ] || die "only x86_64 builds are pinned; see pins.txt"
  if [ -x "$SERVER" ]; then say "llama.cpp $LLAMA_CPP: present"; else
    case "$b" in
      vulkan) asset="llama-$LLAMA_CPP-bin-ubuntu-vulkan-x64.tar.gz" ;;
      cpu)    asset="llama-$LLAMA_CPP-bin-ubuntu-x64.tar.gz" ;;
      *)      die "unknown backend '$b' (vulkan or cpu)" ;;
    esac
    say "llama.cpp $LLAMA_CPP ($b)"
    verified "https://github.com/ggml-org/llama.cpp/releases/download/$LLAMA_CPP/$asset" \
      "$(pin asset "$asset" 4)" "$cache/$asset"
    unpack_into "$cache/$asset" llama-server "$(dirname "$SERVER")"
  fi
  if [ -x "$SWAP" ]; then say "llama-swap v$SWAP_VER: present"; else
    asset="llama-swap_${SWAP_VER}_linux_amd64.tar.gz"
    say "llama-swap v$SWAP_VER"
    verified "https://github.com/mostlygeek/llama-swap/releases/download/v$SWAP_VER/$asset" \
      "$(pin asset "$asset" 4)" "$cache/$asset"
    unpack_into "$cache/$asset" llama-swap "$(dirname "$SWAP")"
  fi
  rm -rf "$cache"
}

install_models() {
  local dir; dir="$(models_dir)"
  while read -r name path sha; do
    if [ -f "$dir/$path" ]; then say "model $name: present"; continue; fi
    say "model $name"
    local owner="${path%%/*}" rest="${path#*/}"
    local repo="${rest%%/*}" file="${rest#*/}"
    verified "https://huggingface.co/$owner/$repo/resolve/main/$file" "$sha" "$dir/$path"
  done < <(models_pinned)
}

render() { # render <template>: the @...@ placeholders filled in
  local device=""
  [ "$(backend)" = "cpu" ] || device="-ngl all"
  sed -e "s|@SERVER@|$SERVER|g" -e "s|@MODELS@|$(models_dir)|g" -e "s|@DEVICE@|$device|g" \
      -e "s|@SWAP@|$SWAP|g" -e "s|@CONFIG@|$CONFIG|g" -e "s|@LISTEN@|$LISTEN|g" "$1"
}

template() { # run from the runtime copy, the templates are the ones `install` left beside it
  if [ "$HERE" = "$RUNTIME" ]; then say "$RUNTIME/templates/$1"; else say "$HERE/$1"; fi
}

write_config() {
  if [ -f "$CONFIG" ]; then say "config: kept $CONFIG"; return; fi
  render "$(template llama-swap.yaml)" > "$CONFIG"
  say "config: wrote $CONFIG"
}

# A copy of the tooling beside the runtime, so the service and `findery-models`
# keep working whatever happens to the repository checkout.
copy_tooling() {
  [ "$HERE" = "$RUNTIME" ] && return
  cp -f "$HERE/models.sh" "$RUNTIME/findery-models"
  chmod +x "$RUNTIME/findery-models"
  cp -f "$HERE/pins.txt" "$RUNTIME/pins.txt"
  mkdir -p "$RUNTIME/templates"
  cp -f "$HERE/llama-swap.yaml" "$HERE/findery-models.service" "$RUNTIME/templates/"
}

# --- running -------------------------------------------------------------------

up() { curl -fs -m 2 "$BASE_URL/health" >/dev/null 2>&1; }
installed() { [ -x "$SWAP" ] && [ -f "$CONFIG" ] || die "not installed in $RUNTIME - run: models.sh install"; }
has_systemd() { command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; }
by_service() { has_systemd && systemctl --user is-enabled --quiet "$UNIT_NAME" 2>/dev/null; }
running_pid() { [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null && cat "$PIDFILE"; }

start() {
  installed
  if up; then say "already running on $BASE_URL"; return; fi
  if by_service; then
    systemctl --user start "$UNIT_NAME"
  else
    mkdir -p "$(dirname "$LOG")"
    nohup "$SWAP" -config "$CONFIG" -listen "$LISTEN" -watch-config >"$LOG" 2>"$LOG.err" &
    echo $! > "$PIDFILE"
  fi
  wait_up
}

wait_up() {
  for _ in $(seq 40); do
    if up; then say "up on $BASE_URL  (dashboard: $BASE_URL/ui)"; return; fi
    sleep 0.25
  done
  die "llama-swap did not answer on $BASE_URL; see $(by_service && echo "journalctl --user -u $UNIT_NAME" || echo "$LOG.err")"
}

stop() {
  # llama-swap owns the llama-server children; unloading lets it stop them cleanly.
  curl -fs -m 15 -X POST "$BASE_URL/api/models/unload" >/dev/null 2>&1 || true
  if by_service && systemctl --user is-active --quiet "$UNIT_NAME"; then
    systemctl --user stop "$UNIT_NAME"
  elif pid="$(running_pid)"; then
    kill "$pid"
  else
    say "not running"; return
  fi
  rm -f "$PIDFILE"
  pkill -f "$RUNTIME/llama.cpp/" 2>/dev/null || true
  say "stopped"
}

status() {
  local note=""
  [ -x "$SWAP" ] && [ -x "$SERVER" ] || note="  (not installed)"
  printf 'runtime   %s%s\n' "$RUNTIME" "$note"
  printf 'backend   %s\n' "$(backend)"
  printf 'models    %s\n' "$(models_dir)"
  printf 'autostart %s\n' "$(by_service && echo on || echo off)"
  local loaded=""
  if up; then
    say "server    up on $BASE_URL  (dashboard: $BASE_URL/ui)"
    loaded="$(curl -fs -m 5 "$BASE_URL/running" || true)"
  else
    say "server    not running - start it with: models.sh start"
  fi
  local dir; dir="$(models_dir)"
  while read -r name path _; do
    local s="idle"
    if [ ! -f "$dir/$path" ]; then s="missing - models.sh pull"
    elif printf '%s' "$loaded" | grep -q "\"$name\""; then s="loaded"
    fi
    printf '  %-38s %s\n' "$name" "$s"
  done < <(models_pinned)
}

autostart() {
  case "${1:-}" in
    on)
      installed
      has_systemd || die "no systemd user session here; start it from your shell profile instead: $RUNTIME/findery-models start"
      mkdir -p "$(dirname "$UNIT")"
      render "$(template findery-models.service)" > "$UNIT"
      stop >/dev/null || true
      systemctl --user daemon-reload
      systemctl --user enable --now --quiet "$UNIT_NAME"
      say "autostart on: $UNIT_NAME starts at every login (logs: journalctl --user -u $UNIT_NAME -f)"
      wait_up
      ;;
    off)
      if has_systemd && [ -f "$UNIT" ]; then
        systemctl --user disable --now "$UNIT_NAME" 2>/dev/null || true
        rm -f "$UNIT"
        systemctl --user daemon-reload
      fi
      say "autostart off"
      ;;
    *) die "autostart takes on or off" ;;
  esac
}

next_steps() {
  say ""
  say "Point findery at it:"
  say "  findery config set url   $BASE_URL"
  say "  findery config set model harrier-oss-v1-0.6b"
  say "The model names are the ones LM Studio uses, so vectors it computed are reused."
}

usage() { sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

action="${1:-help}"
[ $# -gt 0 ] && shift
while [ $# -gt 0 ]; do
  case "$1" in
    --backend) BACKEND="${2:?--backend takes vulkan or cpu}"; shift 2 ;;
    --models)  MODELS_DIR="${2:?--models takes a directory}"; shift 2 ;;
    on|off)    mode="$1"; shift ;;
    *)         die "unknown argument '$1'" ;;
  esac
done

case "$action" in
  install)
    need curl; need sha256sum; need tar
    mkdir -p "$RUNTIME"
    b="$(backend)"
    install_binaries "$b"
    install_models
    write_config
    printf 'BACKEND=%s\nMODELS_DIR=%s\n' "$b" "$(models_dir)" > "$STATE"
    copy_tooling
    say "installed in $RUNTIME"
    next_steps
    ;;
  pull)      need curl; need sha256sum; install_models ;;
  start)     start ;;
  stop)      stop ;;
  restart)   stop; start ;;
  status)    status ;;
  ui)        up || start; xdg-open "$BASE_URL/ui" >/dev/null 2>&1 || say "$BASE_URL/ui" ;;
  logs)      if by_service; then journalctl --user -u "$UNIT_NAME" -f; else tail -n 40 -f "$LOG"; fi ;;
  devices)   installed; "$SERVER" --list-devices ;;
  autostart) autostart "${mode:-}" ;;
  uninstall)
    stop || true
    autostart off
    rm -rf "$RUNTIME/llama.cpp" "$RUNTIME/llama-swap"
    say "removed the binaries; kept $CONFIG and the models in $(models_dir)"
    ;;
  help|-h|--help) usage ;;
  *) die "unknown command '$action'; see: models.sh help" ;;
esac
