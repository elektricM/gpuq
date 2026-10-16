#!/bin/sh
# Fake docker for the Immich adapter tests.
# Usage: docker.sh STATE_DIR docker-arguments...
# Every call is appended to STATE_DIR/log. The container is "running" while STATE_DIR/running
# exists; STATE_DIR/health holds the health status it reports (written as "healthy" by the
# first `up` unless the test wrote one earlier). STATE_DIR/fail_up and fail_stop make those
# compose commands fail.
d=$1
shift
echo "$*" >> "$d/log"
case "$1" in
compose)
    pwd > "$d/cwd"
    case " $* " in
    *" up "*)
        [ -f "$d/fail_up" ] && { echo "up failed" >&2; exit 1; }
        touch "$d/running"
        [ -f "$d/health" ] || echo healthy > "$d/health"
        ;;
    *" stop "*)
        [ -f "$d/fail_stop" ] && { echo "stop failed" >&2; exit 1; }
        rm -f "$d/running"
        ;;
    esac
    ;;
inspect)
    case "$3" in
    *Running*)
        if [ -f "$d/running" ]; then echo true; else echo false; fi
        ;;
    *Health*)
        [ -f "$d/running" ] && cat "$d/health"
        ;;
    esac
    ;;
esac
exit 0
