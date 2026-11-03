#!/usr/bin/env python3
"""Runs the scene of docs/demo.gif and draws gpuq's state while it plays.

Everything on screen comes from the demo daemon's GET /status, polled five times a
second: the VRAM bars are each card's used/total, split into the leases gpuq granted
and the LLM model; the waiting list is gpuq's queue with its own reason; the log is
gpuq's history. The script only types the commands and writes the captions.
"""

import json
import re
import os
import subprocess
import sys
import threading
import time
import urllib.request

API = "http://" + os.environ.get("GPUQ_API", "127.0.0.1:19390")
GATE = "http://127.0.0.1:19434"
MODEL = "qwen2.5:32b"
COLS, ROWS, BAR = 64, 19, 29

RESET, BOLD, DIM = "\033[0m", "\033[1m", "\033[2m"


def fg(rgb):
    return "\033[38;2;%d;%d;%dm" % rgb


def bg(rgb):
    return "\033[48;2;%d;%d;%dm" % rgb


WHITE = (230, 233, 239)
GREY = (125, 133, 148)
EMPTY = (48, 54, 66)
BASE = (88, 96, 110)
LLM = (199, 146, 234)
CAPTION = (255, 203, 107)
GREEN = (126, 211, 33)
JOB_COLORS = [(97, 175, 239), (86, 214, 194), (240, 140, 90), (230, 110, 140)]

START = int(time.time())
state = {}
shell = []          # typed lines shown under the cards
caption = ""
chat_answer = None
chat_started = False
job_color = {}


def poll():
    global state
    while True:
        try:
            with urllib.request.urlopen(API + "/status", timeout=1) as r:
                state = json.load(r)
        except Exception:
            pass
        time.sleep(0.2)


def color_of(name):
    if name not in job_color:
        job_color[name] = JOB_COLORS[len(job_color) % len(JOB_COLORS)]
    return job_color[name]


def mib(v):
    if isinstance(v, str) and v.endswith("MiB"):
        return int(v[:-3])
    return 0


def gb(m):
    return "%.1f" % (m / 1024)


def card_lines(card):
    name, total = card["name"], card["vram"]["total_mib"]
    used = card["vram"]["used_mib"]
    base = card.get("baseline_mib") or card.get("floor_mib") or 0
    segs = [(base, BASE, None)]
    leases = [l for l in state.get("leases", []) if name in l.get("cards", [l["card"]])]
    held = 0
    for l in leases:
        m = min(mib(l["vram"]), max(used - base - held, 0))
        held += m
        segs.append((m, color_of(l["name"]), l["name"]))
    rest = max(used - base - held, 0)
    loaded = name in state.get("llm_cards", []) and rest >= 512
    if rest:
        segs.append((rest, LLM if loaded else BASE, MODEL if loaded else None))
    bar, filled = "", 0
    for m, col, _ in segs:
        n = round(m / total * BAR)
        n = min(n, BAR - filled)
        bar += bg(col) + " " * n
        filled += n
    bar += bg(EMPTY) + " " * (BAR - filled) + RESET
    st = card.get("state", "")
    tag_col = {"llm": LLM, "jobs": JOB_COLORS[0]}.get(st, CAPTION)
    tag = {"llm": "LLM"}.get(st, st)
    head = " %s%-9s%s %s %s%5s/%d GB%s  %s%s%s" % (
        BOLD, name, RESET, bar, fg(WHITE), gb(used), round(total / 1024), RESET,
        BOLD + fg(tag_col), tag, RESET)
    labels = [(lab, col) for m, col, lab in segs if lab]
    sub = "           "
    width = 11
    for lab, col in labels:
        piece = "■ " + lab + "  "
        if width + len(piece) > COLS:
            break
        sub += fg(col) + piece + RESET
        width += len(piece)
    if not labels:
        sub += fg(GREY) + "free" + RESET
    return [head, sub]


def chat_held():
    """gpuq logs "hold llm request" when the gate holds one, "release held" when it goes."""
    held = False
    for h in state.get("history", []):
        if h["msg"].startswith("hold llm request"):
            held = True
        elif h["msg"].startswith("release held llm request"):
            held = False
    return held and chat_started and chat_answer is None


def frame():
    out = []
    out.append(" " + BOLD + fg(CAPTION) + caption.ljust(COLS - 1) + RESET)
    out.append("")
    cards = state.get("cards", [])
    for c in cards:
        if c.get("vram"):
            out += card_lines(c)
            out.append("")
    if not cards:
        out += ["", "", "", "", "", ""]
    waiting = list(state.get("queue", []))
    if chat_held() and not any(w.get("kind") == "llm" for w in waiting):
        waiting.append({"kind": "llm"})
    if waiting:
        for w in waiting[:2]:
            if w.get("kind") == "llm":
                text = "chat request for %s, held in the gate" % MODEL
            else:
                why = w.get("why", "").split(": ", 1)[-1]
                text = "%s %s GB  %s" % (w["name"], gb(mib(w.get("vram", ""))), why)
            head = " " + fg(CAPTION) + "waiting  " + RESET
            while len(text) > COLS - 10:
                cut = text[: COLS - 10].rsplit(" ", 1)[0]
                out.append(head + cut)
                head, text = " " * 10, text[len(cut) + 1:]
            out.append(head + text)
    else:
        out.append(" " + fg(GREY) + "waiting  nothing" + RESET)
    out.append("")
    for line in shell[-2:]:
        out.append(line)
    for _ in range(2 - len(shell[-2:])):
        out.append("")
    out.append("")
    out.append(" " + fg(GREY) + "gpuq log" + RESET)
    log = [h for h in state.get("history", []) if h["t"] >= START]
    if not log:
        out.append(" " + fg(GREY) + "(nothing yet)" + RESET)
    for h in log[-4:]:
        t = time.strftime("%H:%M:%S", time.localtime(h["t"]))
        # Lease ids and the class prefix only make the lines longer here.
        msg = re.sub(r"(lease )?L[0-9a-f]+-\d+ ", "", h["msg"]).replace("batch/", "")
        if len(msg) > COLS - 11:
            msg = msg[: COLS - 12] + "…"
        out.append(" " + fg(GREY) + t + RESET + " " + msg)
    while len(out) < ROWS:
        out.append("")
    return "\033[H" + "\n".join(l + "\033[K" for l in out[:ROWS])


def draw_loop():
    last = None
    while True:
        f = frame()
        if f != last:
            sys.stdout.write(f)
            sys.stdout.flush()
            last = f
        time.sleep(0.05)


def prompt(text):
    return " " + fg(GREEN) + "$ " + RESET + text


def type_cmd(text):
    shell.append(prompt(""))
    for i in range(len(text) + 1):
        shell[-1] = prompt(text[:i])
        time.sleep(0.012)
    time.sleep(0.15)


def job(vram, prog, name):
    argv = ["gpuq", "run", "--name", name, "--vram", vram, "--", prog]
    type_cmd(" ".join(argv) + " &")
    subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def chat():
    global chat_answer, chat_started
    chat_started = True
    req = urllib.request.Request(
        GATE + "/api/generate",
        data=json.dumps({"model": MODEL, "prompt": "hi"}).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=120) as r:
        chat_answer = json.load(r).get("response", "")


def wait(pred, timeout=30):
    end = time.time() + timeout
    while time.time() < end and not pred():
        time.sleep(0.05)


def main():
    global caption
    sys.stdout.write("\033[?25l\033[2J")
    threading.Thread(target=poll, daemon=True).start()
    wait(lambda: state.get("cards"))
    caption = "Two 24 GB GPUs. A 32B model is split over both."
    threading.Thread(target=draw_loop, daemon=True).start()
    time.sleep(3.5)

    caption = "Jobs ask gpuq for VRAM. It unloads the model from both."
    job("8G", "render", "render-1")
    job("22G", "train", "train")
    job("8G", "render", "render-2")
    job("8G", "render", "render-3")
    wait(lambda: state.get("queue"))
    caption = "Each job lands where it fits. render-3 waits for room."
    time.sleep(3.0)

    caption = "A chat request arrives. It waits in gpuq's gate."
    type_cmd("curl localhost:19434/api/generate -d '{\"model\": ...}'")
    threading.Thread(target=chat, daemon=True).start()
    wait(lambda: card_state() == {"restoring"})
    caption = "Jobs done. Chat outranks batch: the model comes back first."
    wait(lambda: chat_answer is not None, 60)
    shell.append(" " + fg(GREY) + '{"response": "%s"}' % chat_answer + RESET)
    wait(lambda: any(l["name"] == "render-3" for l in state.get("leases", [])))
    caption = "Then render-3 gets its turn."
    wait(lambda: not state.get("leases") and card_state() == {"llm"}
         and len(state.get("llm_cards", [])) == 2)
    caption = "Done. Nothing ran out of memory; the model is back on both."
    time.sleep(4.5)


def card_state():
    return {c.get("state") for c in state.get("cards", [])}


if __name__ == "__main__":
    main()
