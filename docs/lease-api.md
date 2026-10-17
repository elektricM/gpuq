# Lease API

For programs that want to talk to gpuq directly. If you only need to wrap a command, use
`gpuq run` instead (see `integrations.md`).

The API listens on `127.0.0.1:9390` (`[api]` in the config). One idea: **a lease lives exactly as
long as its HTTP connection.** Open a request, wait for `granted`, do your work, close the
connection to release. If your process dies, the connection drops and gpuq frees the card.
There is no release call and nothing to clean up.

## POST /lease

Body: a JSON object (at most 64 KiB). Every field is optional.

| Field | Default | Meaning |
|---|---|---|
| `class` | `"batch"` | Priority class from `[classes]`. |
| `vram` | whole card | `"8G"`, `"3000M"`, `"512"` (MiB) or `"all"`. Wins over `vram_mib`. |
| `vram_mib` | whole card | Integer MiB; `-1` = whole card. |
| `yieldable` | `false` | If true, gpuq may ask you to give the card back. |
| `name` | none | Shown in `gpuq status`. |
| `pid` | none | Your process id, shown in status. |
| `card` | any card with room | A card name from `gpuq status`; `"any"` for the first card with room; `"all"` for every card at once; a comma list such as `"gpu0,gpu1"` for those cards at once; `"primary"` for the first card. Missing or `null`: any card with room, or the primary card when no card has `visible_devices`. With `resume_id`, the lease is re-adopted on the cards named. |
| `cards` | none | A list of card names, all held at once: `["gpu0", "gpu1"]`. Same as a comma list in `card`. |
| `card_count` | none | A number of cards, all held at once, any that have room (like `"any"`, for several). |
| `resume_id` | none | Re-adopt a lease you held before (for example across a gpuq restart) under the same id, without queueing. |

Use at most one of `card`, `cards` and `card_count`. `vram` applies to each card of the lease.

A lease on several cards is granted all at once or not at all. While it waits it holds
nothing, so two leases can never wait for each other. Leases of a lower class, and later
leases of the same class, are not served on its cards before it: each card it needs is kept
for it as soon as its current holders leave. More important leases still go first. If something more important needs any one of its cards,
a yieldable multi-card lease is asked to yield, as a single-card one would be.

Errors come back as plain HTTP 400 with a JSON body, for example
`{"error": "unknown class 'x'; known: realtime, llm, interactive, background, batch"}`. An
unknown card is `{"error": "unknown card 'x'; known: gpu0, gpu1"}`. `"any"` and `card_count`
are refused when not enough cards can be steered, that is, have an env (with several cards).
A lease on several cards is refused when one of them has no env, when a card is listed twice,
and when two cards of different vendors set the same variable (see `config.md`).

On success the answer is `200` with `Content-Type: application/x-ndjson`, chunked, one JSON
object per line:

| Event | When |
|---|---|
| `{"event":"queued"}` | Immediately. |
| `{"event":"ping"}` | Every 15 s while queued and while holding. Ignore it. |
| `{"event":"granted","id":"...","card":"gpu0","cards":["gpu0"],"env":{},"yield_signal":"SIGUSR1"}` | Once, when the cards are yours. `cards` lists the cards you got, in grant order; `card` is the same names joined with commas (just the name for one card, never `any`). `env` is an object of variables that point your program at them, for example `{"CUDA_VISIBLE_DEVICES":"1"}` or, for two cards, `{"CUDA_VISIBLE_DEVICES":"GPU-a,GPU-b"}`; empty when the cards have none. `yield_signal` is gpuq's configured `[run] yield_signal`, for clients that pass yields on as a signal. |
| `{"event":"yield"}` | At most once, only for yieldable leases, when something more important is waiting. |

There is no terminal event. The stream just stays open.

- Close while queued: you leave the queue.
- Close after `granted`: the lease is released.
- On `yield`: save state, stop using the card, close the connection. gpuq does not kill you.
  A yieldable lease that ignores the request keeps the card; the waiter keeps waiting.
- Dead clients are noticed at once when the socket closes. A peer that vanishes without
  closing is noticed by a failed ping write or the TCP timeout (about two ping intervals).
- If your connection drops but you are still working, reconnect with the same `resume_id`.
  `gpuq run` does this every 2 s.

## Other routes

| Route | Answer |
|---|---|
| `GET /status` | JSON: arbiter state, leases, queue, `cards` (one entry per card: `name`, `device`, `state`, `llm`, `vram`), `llm_cards` (the cards the LLM holds now) and `backend` (`kind`, `models` with `state`). The top-level `state`, `device` and `vram` are the primary card's. Each lease has `card` (names joined with commas) and `cards`; each queue entry has a `card`: a name, a comma list, `any`, or `any N` for N cards. |
| `GET /healthz` | `ok` |

Unknown path: 404. Wrong method: 405. Arbiter stopped: 503.

## curl

```sh
# Wait for the card, hold it until Ctrl-C
curl -N -X POST http://127.0.0.1:9390/lease \
  -d '{"class":"batch","vram":"8G","yieldable":true,"name":"my-job"}'
# {"event":"queued"}
# {"event":"granted","id":"L3f2a1-3","card":"gpu0","cards":["gpu0"],"env":{},"yield_signal":"SIGUSR1"}
# {"event":"ping"}
# {"event":"yield"}        <- something more important wants the card

# Two cards at once, 12 GiB on each
curl -N -X POST http://127.0.0.1:9390/lease \
  -d '{"class":"batch","vram":"12G","cards":["gpu0","gpu1"],"name":"train"}'
# {"event":"granted","id":"L3f2a1-4","card":"gpu0,gpu1","cards":["gpu0","gpu1"],
#  "env":{"CUDA_VISIBLE_DEVICES":"GPU-a,GPU-b"},"yield_signal":"SIGUSR1"}

curl -s http://127.0.0.1:9390/status
```

`-N` turns off curl's output buffering, so you see events as they happen.

## Python client

Standard library only. It waits for the card, runs work in steps, and stops cleanly when asked to
yield.

```python
import http.client, json, threading

def run_with_lease(work_step, body):
    conn = http.client.HTTPConnection("127.0.0.1", 9390)
    conn.request("POST", "/lease", json.dumps(body))
    resp = conn.getresponse()                  # headers arrive immediately
    granted, yield_now = threading.Event(), threading.Event()

    def reader():
        for line in resp:                      # one JSON object per line
            event = json.loads(line)["event"]
            if event == "granted": granted.set()
            elif event == "yield": yield_now.set()
    threading.Thread(target=reader, daemon=True).start()

    granted.wait()
    while not yield_now.is_set() and work_step():   # work_step() returns False when done
        pass
    conn.close()                               # releases the lease
    return not yield_now.is_set()              # False: asked to yield, work is unfinished

# run_with_lease(train_one_step, {"class": "batch", "vram": "8G", "yieldable": True})
```

If `work_step` is long, check `yield_now` inside it and checkpoint before returning. On a
`False` result, call `run_with_lease` again to queue for the next turn.
