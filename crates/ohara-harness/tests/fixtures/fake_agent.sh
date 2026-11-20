#!/bin/sh
# Test fixture standing in for an agent CLI. Usage: fake_agent.sh <mode> <out-file> [args...]
mode="$1"
out="$2"
case "$mode" in
  argv)
    shift 2
    printf '%s\n' "$@" > "$out"
    echo "argv recorded"
    ;;
  stdin)
    cat > "$out"
    echo "stdin recorded"
    ;;
  say)
    shift 2
    echo "$@"
    ;;
  fail)
    shift 2
    echo "$@" >&2
    exit 3
    ;;
  sleep)
    exec sleep 60
    ;;
  *)
    echo "unknown mode: $mode" >&2
    exit 64
    ;;
esac
