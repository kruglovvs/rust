#!/usr/bin/env bash
# Todo 6 gate (b): feed our RFC 2435 fragments to TWO independent depacketizers —
# ffmpeg (libavformat rtpdec_jpeg.c) over RTP/UDP via SDP, and GStreamer
# rtpjpegdepay — then assert: decode error-free, ffprobe mjpeg 320x240, and the
# decoded image equals the source frame's decode (PSNR + raw-pixel cmp).
# The ffmpeg output is the RAW depacketized JPEG (-c:v copy), never re-encoded.
# Run from /workspaces/rust/harness.
set -u
EV=/workspaces/rust/.omo/evidence/task-6
FIX=fixtures
BIN=./target/x86_64-unknown-linux-gnu/debug/gate6
mkdir -p "$EV"
[ -x "$BIN" ] || cargo build --quiet
FAILURES=0

note() { echo "$*" | tee -a "$EV/gate-b.txt"; }

send_twice() {
    local fixture=$1 port=$2
    "$BIN" send "$FIX/$fixture" "127.0.0.1:$port" >> "$EV/gate-b.txt" 2>&1 || {
        sleep 1
        "$BIN" send "$FIX/$fixture" "127.0.0.1:$port" >> "$EV/gate-b.txt" 2>&1
    }
    sleep 0.5
    "$BIN" send "$FIX/$fixture" "127.0.0.1:$port" >> "$EV/gate-b.txt" 2>&1 || true
}

run_ffmpeg() {
    local fixture=$1 port=$2 tag=$3
    local sdp=/tmp/opencode/session_${tag}.sdp
    local out=$EV/out_${tag}.mjpeg
    mkdir -p /tmp/opencode
    cat > "$sdp" <<EOF
v=0
o=- 0 0 IN IP4 127.0.0.1
s=No Name
c=IN IP4 127.0.0.1
t=0 0
m=video $port RTP/AVP 26
a=rtpmap:26 JPEG/90000
EOF
    rm -f "$out"
    note "==== ffmpeg SDP depacketizer: fixture=$fixture port=$port ===="
    ffmpeg -y -v warning -protocol_whitelist file,rtp,udp -i "$sdp" \
        -frames:v 1 -c:v copy -f mjpeg "$out" > "$EV/ffmpeg-${tag}.log" 2>&1 &
    local pid=$!
    sleep 2
    send_twice "$fixture" "$port"
    local rc=0
    timeout 25 tail --pid="$pid" -f /dev/null || rc=timeout
    if kill -0 "$pid" 2>/dev/null; then kill "$pid" 2>/dev/null; rc=timeout; fi
    wait "$pid"; local ffrc=$?
    if [ "$rc" = timeout ]; then
        note "ffmpeg[$tag]: TIMEOUT waiting for a frame — FAIL"; FAILURES=$((FAILURES+1)); return
    fi
    if [ "$ffrc" -ne 0 ] || [ ! -s "$out" ]; then
        note "ffmpeg[$tag]: exit=$ffrc out-size=$(stat -c%s "$out" 2>/dev/null || echo 0) — FAIL (ffmpeg-${tag}.log)"; tail -3 "$EV/ffmpeg-${tag}.log" | tee -a "$EV/gate-b.txt"
        FAILURES=$((FAILURES+1)); return
    fi
    local errs dec
    errs=$(ffmpeg -v error -i "$out" -f null - 2>&1); dec=$?
    if [ "$dec" -ne 0 ] || [ -n "$errs" ]; then
        note "ffmpeg[$tag]: decode errors: ${errs:-<rc=$dec>} — FAIL"; FAILURES=$((FAILURES+1)); return
    fi
    local probe=$EV/ffprobe-${tag}.json
    ffprobe -v quiet -print_format json -show_streams "$out" > "$probe"
    local codec w h pf
    codec=$(ffprobe -v error -select_streams v:0 -show_entries stream=codec_name -of csv=p=0 "$out")
    w=$(ffprobe -v error -select_streams v:0 -show_entries stream=width -of csv=p=0 "$out")
    h=$(ffprobe -v error -select_streams v:0 -show_entries stream=height -of csv=p=0 "$out")
    pf=$(ffprobe -v error -select_streams v:0 -show_entries stream=pix_fmt -of csv=p=0 "$out")
    if [ "$codec" != mjpeg ] || [ "$w" != 320 ] || [ "$h" != 240 ]; then
        note "ffmpeg[$tag]: ffprobe got $codec ${w}x${h} — FAIL"; FAILURES=$((FAILURES+1)); return
    fi
    local psnr
    psnr=$(ffmpeg -v info -i "$out" -i "$FIX/$fixture" \
        -lavfi "[0:v]format=yuvj422p[a];[1:v]format=yuvj422p[b];[a][b]psnr" -f null - 2>&1 \
        | grep -o 'average:[^ ]*' | tail -1)
    ffmpeg -v error -i "$out" -pix_fmt "$pf" -f rawvideo /tmp/opencode/pix_${tag}_rtp.raw || true
    ffmpeg -v error -i "$FIX/$fixture" -pix_fmt "$pf" -f rawvideo /tmp/opencode/pix_${tag}_src.raw || true
    local same=false
    cmp -s /tmp/opencode/pix_${tag}_rtp.raw /tmp/opencode/pix_${tag}_src.raw && same=true
    note "ffmpeg[$tag]: exit=0 decode-error-free=true ffprobe=$codec ${w}x${h} pix_fmt=$pf ${psnr:-psnr-n/a} raw_pixels_identical=$same PASS"
    if [ "$same" != true ]; then note "ffmpeg[$tag]: raw pixel mismatch — FAIL"; FAILURES=$((FAILURES+1)); fi
}

run_gst() {
    local fixture=$1 port=$2 tag=$3
    note "==== gstreamer rtpjpegdepay depacketizer: fixture=$fixture port=$port ===="
    timeout 25 gst-launch-1.0 -v \
        udpsrc port="$port" caps="application/x-rtp,media=video,clock-rate=90000,encoding-name=JPEG,payload=26" \
        ! rtpjpegdepay ! jpegdec ! fakesink num-buffers=1 sync=false \
        > "$EV/gst-${tag}.log" 2>&1 &
    local pid=$!
    sleep 1.5
    send_twice "$fixture" "$port"
    local rc=0
    timeout 25 tail --pid="$pid" -f /dev/null || rc=timeout
    if kill -0 "$pid" 2>/dev/null; then kill "$pid" 2>/dev/null; rc=timeout; fi
    wait "$pid"; local gst=$?
    if [ "$rc" = timeout ] || [ "$gst" -ne 0 ]; then
        note "gst[$tag]: rc=$gst ${rc} — FAIL (gst-${tag}.log)"; tail -5 "$EV/gst-${tag}.log" | tee -a "$EV/gate-b.txt"
        FAILURES=$((FAILURES+1)); return
    fi
    if grep -q "Got EOS" "$EV/gst-${tag}.log"; then
        note "gst[$tag]: exit=0 EOS-after-1-frame rtpjpegdepay+jpegdec OK PASS (log: gst-${tag}.log)"
    else
        note "gst[$tag]: no EOS — FAIL"; FAILURES=$((FAILURES+1))
    fi
}

note "gate (b) run — $(date -u +%FT%TZ) — cmd: bash harness/gate_b.sh (cwd /workspaces/rust/harness)"

run_ffmpeg qvga_422_dyn.jpg       5004 dyn
run_ffmpeg qvga_420.jpg           5005 t1
run_ffmpeg qvga_422_restart.jpg   5006 rst
run_ffmpeg qvga_422.jpg           5007 s80

run_gst qvga_422_dyn.jpg          5014 dyn
run_gst qvga_420.jpg              5015 t1
run_gst qvga_422_restart.jpg      5016 rst

if [ "$FAILURES" -eq 0 ]; then
    note "GATE (b): ALL PASS — ffmpeg rtpdec_jpeg.c + gstreamer rtpjpegdepay"
else
    note "GATE (b): $FAILURES FAILURE(S)"
fi
exit "$FAILURES"
