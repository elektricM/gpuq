#!/bin/sh
# Records docs/demo.gif with a throwaway gpuq daemon: its own config and ports under
# $GPUQ_DEMO, two pretend cards (fake-vram.sh) and an Ollama stand-in (mock-ollama.py).
# No real GPU is used and a gpuq already running on the machine is left alone.
#
# Needs: a built gpuq (GPUQ_BIN, default target/release/gpuq), python3, curl, asciinema 2
# and agg (https://github.com/asciinema/agg).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
gpuq=${GPUQ_BIN:-$root/target/release/gpuq}
export GPUQ_DEMO="${GPUQ_DEMO:-/tmp/gpuq-demo}"
export GPUQ_DEMO_CARDS=rtx3090-0,rtx3090-1
export GPUQ_API=127.0.0.1:19390
export TERM=xterm-256color

rm -rf "$GPUQ_DEMO"
mkdir -p "$GPUQ_DEMO/bin"
ln -s "$gpuq" "$GPUQ_DEMO/bin/gpuq"
export PATH="$GPUQ_DEMO/bin:$here/bin:$PATH"
sed "s|@DIR@|$here|g" "$here/demo.toml" >"$GPUQ_DEMO/config.toml"

python3 "$here/mock-ollama.py" 19435 >"$GPUQ_DEMO/mock.log" 2>&1 &
mock=$!
until curl -sf localhost:19435/api/ps >/dev/null; do sleep 0.2; done
gpuq daemon -c "$GPUQ_DEMO/config.toml" >"$GPUQ_DEMO/daemon.log" 2>&1 &
daemon=$!
trap 'kill $daemon $mock 2>/dev/null' EXIT
sleep 2
# Load the model on both cards, then let it go idle so jobs may take the cards.
curl -s localhost:19434/api/generate -d '{"model":"qwen2.5:32b","prompt":"hi"}' >/dev/null
sleep 4

asciinema rec --overwrite --cols 92 --rows 28 -c "bash $here/scene.sh" "$GPUQ_DEMO/demo.cast"
agg --font-size 15 --theme monokai --last-frame-duration 4 "$GPUQ_DEMO/demo.cast" "$root/docs/demo.gif"
ls -l "$root/docs/demo.gif"
