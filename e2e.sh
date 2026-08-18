#!/usr/bin/env bash
# E2E harness for pipewire-vircam.
# Builds redcam + C oracle, runs live PipeWire sequences, asserts pixels.
# Requires: a running PipeWire/WirePlumber session (wpctl).
# Usage: ./e2e.sh
set -euo pipefail
cd "$(dirname "$0")"

RED_BIN="${RED_BIN:-target/release/redcam}"
ORACLE=./redcam-test
FRAMES=30

mkdir -p target
TMP="$(mktemp -d "target/e2e.XXXXXX")"
REDPID=""
REDLOG=""
NODE_ID=""
TOTAL_PASS=0
TOTAL_FAIL=0

cleanup() {
    [ -n "$REDPID" ] && { kill "$REDPID" 2>/dev/null || true; wait "$REDPID" 2>/dev/null || true; }
    rm -rf "$TMP"
}
trap cleanup EXIT INT TERM

ok()   { echo "  PASS: $1"; TOTAL_PASS=$((TOTAL_PASS + 1)); }
fail() { echo "  FAIL: $1"; TOTAL_FAIL=$((TOTAL_FAIL + 1)); }

print_oracle_log() {
    grep -E '^(PASS|FAIL|frames=)' "$1" | sed 's/^/    /'
}

start_producer() {
    local seq=$1; shift
    REDLOG="$TMP/redcam-$seq.log"
    NODE_ID=""
    "$RED_BIN" "$@" >"$REDLOG" 2>&1 &
    REDPID=$!
    local i=0
    while [ "$i" -lt 100 ]; do
        if grep -q '^node id: [0-9][0-9]*$' "$REDLOG" 2>/dev/null; then
            NODE_ID=$(sed -n 's/^node id: \([0-9]*\)$/\1/p' "$REDLOG" | head -1)
            if [ -n "$NODE_ID" ]; then break; fi
        fi
        sleep 0.1; i=$((i + 1))
    done
    if [ -z "$NODE_ID" ]; then
        fail "sequence $seq: redcam did not register a node (see $REDLOG)"
        sed 's/^/    /' "$REDLOG"
        stop_producer "$seq"
        return 1
    fi
}

stop_producer() {
    kill "$REDPID" 2>/dev/null || true
    wait "$REDPID" 2>/dev/null || true
    REDPID=""
}

assert_registered() {
    local seq=$1 listing
    # Snapshot once: grepping a live pw-cli can SIGPIPE it mid-stream, and
    # both properties must come from the *same* node block anyway.
    listing=$(pw-cli ls 2>/dev/null)
    if printf '%s\n' "$listing" | grep -A 30 'node.name = "redcam"' \
        | grep -q 'media.class = "Video/Source"'; then
        ok "sequence $seq: node registered (node.name=redcam, media.class=Video/Source)"
    else
        fail "sequence $seq: node registration (pw-cli)"
    fi
}

assert_clean_teardown() {
    local seq=$1 i=0 node_gone=1
    stop_producer "$seq"
    while [ "$i" -lt 12 ]; do
        if ! pw-cli ls 2>/dev/null | grep -q 'node.name = "redcam"'; then
            node_gone=0; break
        fi
        sleep 0.5; i=$((i + 1))
    done
    if [ "$node_gone" -eq 0 ]; then
        ok "sequence $seq: node gone after redcam exit"
    else
        fail "sequence $seq: node still present after redcam exit"
    fi

    if grep -qE 'stream state: "error"' "$REDLOG" 2>/dev/null; then
        fail "sequence $seq: redcam log contains stream errors"
    else
        ok "sequence $seq: no stream errors in redcam log"
    fi
}

oracle_check() {
    local oseq=$1 okey=$2 olabel=$3; shift 3
    local olog="$TMP/oracle-$oseq-$okey.log"
    if "$ORACLE" "$NODE_ID" "$FRAMES" "$@" >"$olog" 2>&1; then
        ok "sequence $oseq: $olabel"
        print_oracle_log "$olog"
    else
        fail "sequence $oseq: $olabel (see log below)"
        print_oracle_log "$olog"
    fi
}

gst_check() {
    local seq=$1 label=$2 caps=$3 r_min=$4 g_max=$5 b_max=$6
    local png="$TMP/gst-$seq-$label.png"
    local glog="$TMP/gst-$seq-$label.log"

    if ! timeout 15 gst-launch-1.0 -q \
            pipewiresrc target-object=redcam \
            ! "$caps" ! videoconvert ! pngenc snapshot=true \
            ! filesink location="$png" 2>"$glog"; then
        fail "sequence $seq: gst $label capture (see $glog)"
        return
    fi
    if [ ! -s "$png" ] \
        || ! od -An -tx1 -N8 "$png" | tr -d ' \n' | grep -q '^89504e470d0a1a0a'; then
        fail "sequence $seq: gst $label capture produced no valid PNG"
        return
    fi
    local dims rgb r g b rest
    dims=$(identify -format '%w %h' "$png" 2>/dev/null | head -1)
    rgb=$(convert "$png" -resize '1x1!' -colorspace sRGB -alpha off \
             -format '%[pixel:p{0,0}]' info: 2>/dev/null | tr -cd '0-9,')
    r=${rgb%%,*}; rest=${rgb#*,}
    g=${rest%%,*}; b=${rest#*,}

    if [ "$dims" = "1920 1080" ] && [ -n "$r" ] \
        && [ "$r" -ge "$r_min" ] && [ "$g" -lt "$g_max" ] && [ "$b" -lt "$b_max" ]; then
        ok "sequence $seq: gst $label capture is 1920x1080 red ($r,$g,$b)"
    else
        fail "sequence $seq: gst $label capture wrong (dims=$dims avg=$r,$g,$b)"
    fi
}

run_full_sequence() {
    local seq=$1 FMT
    echo "== sequence $seq =="
    start_producer "$seq" || return
    assert_registered "$seq"

    if wpctl status 2>/dev/null | grep -q "Red Virtual Camera"; then
        ok "sequence $seq: visible in wpctl (WirePlumber session)"
    else
        fail "sequence $seq: not visible in wpctl status"
    fi

    for FMT in rgba bgra bgrx rgbx bgr rgb i420 nv12 nv21 yuy2 uyvy grey; do
        oracle_check "$seq" "$FMT" "$FMT red + 1920x1080 + ~30fps" --format "$FMT"
    done

    gst_check "$seq" rgba "video/x-raw,format=RGBA,width=1920,height=1080,framerate=30/1" 200 30 30
    gst_check "$seq" i420 "video/x-raw,format=I420,width=1920,height=1080,framerate=30/1" 150 60 60
    gst_check "$seq" nv12 "video/x-raw,format=NV12,width=1920,height=1080,framerate=30/1" 150 60 60

    assert_clean_teardown "$seq"
}

run_sizefps_sequence() {
    local seq=$1 SPEC
    echo "== sequence $seq (sizes/framerates) =="
    start_producer "$seq" \
        --mode 1920x1080@30 \
        --mode 1280x720@60 \
        --mode 640x480@15 \
        || return
    assert_registered "$seq"

    for SPEC in \
        "1920x1080 30 rgba" \
        "1920x1080 30 i420" \
        "1280x720 60 rgba" \
        "1280x720 60 nv12" \
        "640x480 15 rgba" \
        "640x480 15 grey"; do
        set -- $SPEC
        oracle_check "$seq" "${1}_${2}_${3}" \
            "$1@$2 $3 red + correct size/fps" \
            --size "$1" --fps "$2" --format "$3"
    done

    assert_clean_teardown "$seq"
}

# --- build -----------------------------------------------------------------
make -s redcam redcam-test
case "$RED_BIN" in
    redcam-c|./redcam-c) make -s redcam-c ;;
esac
case "$RED_BIN" in */*) : ;; *) RED_BIN="./$RED_BIN" ;; esac
[ -x "$RED_BIN" ] || { echo "FAIL: producer binary '$RED_BIN' not found"; exit 1; }

# --- run -------------------------------------------------------------------
pkill -x redcam 2>/dev/null || true
pkill -x redcam-c 2>/dev/null || true
sleep 0.3

run_full_sequence 1
run_full_sequence 2
run_sizefps_sequence 3

echo
echo "total: PASS=$TOTAL_PASS FAIL=$TOTAL_FAIL"
[ "$TOTAL_FAIL" -eq 0 ] || exit 1
echo "ALL E2E CHECKS PASSED"
