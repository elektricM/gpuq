# Lease API

For programs that want to talk to gpud directly. If you only need to wrap a command, use
`gpud run` instead (see `integrations.md`).

The API listens on `127.0.0.1:9390` (`[api]` in the config). One idea: **a lease lives exactly as
long as its HTTP connection.** Open a request, wait for `granted`, do your work, close the
connection to release. If your process dies, the connection drops and gpud frees the card.
There is no release call and nothing to clean up.

## POST /lease

Body: a JSON object (at most 64 KiB). Every field is optional.

| Field | Default | Meaning |
|---|---|---|
| `class` | `"batch"` | Priority class from `[classes]`. |
| `vram` | whole card | `"8G"`, `"3000M"`, `"512"` (MiB) or `"all"`. Wins over `vram_mib`. |
| `vram_mib` | whole card | Integer MiB; `-1` = whole card. |
| `yieldable` | `false` | If true, gpud may ask you to give the card back. |
| `name` | none | Shown in `gpud status`. |
| `pid` | none | Your process id, shown in status. |
| `resume_id` | none | Re-adopt a lease you held before (for example across a gpud restart) under the same id, without queueing. |

Errors come back as plain HTTP 400 with a JSON body, for example
`{"error": "unknown class 'x'; known: realtime, llm, interactive, background, batch"}`.

On success the answer is `200` with `Content-Type: application/x-ndjson`, chunked, one JSON
object per line:

| Event | When |
|---|---|
| `{"event":"queued"}` | Immediately. |
| `{"event":"ping"}` | Every 15 s while queued and while holding. Ignore it. |
| `{"event":"granted","id":"...","yield_signal":"SIGUSR1"}` | Once, when the card is yours. `yield_signal` is gpud's configured `[run] yield_signal`, for clients that pass yields on as a signal. |
| `{"event":"yield"}` | At most once, only for yieldable leases, when something more important is waiting. |

There is no terminal event. The stream just stays open.

- Close while queued: you leave the queue.
- Close after `granted`: the lease is released.
- On `yield`: save state, stop using the card, close the connection. gpud does not kill you.
  A yieldable lease that ignores the request keeps the card; the waiter keeps waiting.
- Dead clients are noticed at once when the socket closes. A peer that vanishes without
  closing is noticed by a failed ping write or the TCP timeout (about two ping intervals).
- If your connection drops but you are still working, reconnect with the same `resume_id`.
  `gpud run` does this every 2 s.

## Other routes

| Route | Answer |
|---|---|
| `GET /status` | JSON: arbiter state, leases, queue, and `backend` (`kind`, `models` with `state`). |
| `GET /healthz` | `ok` |

Unknown path: 404. Wrong method: 405. Arbiter stopped: 503.

## curl

```sh
# Wait for the card, hold it until Ctrl-C
curl -N -X POST http://127.0.0.1:9390/lease \
  -d '{"class":"batch","vram":"8G","yieldable":true,"name":"my-job"}'
# {"event":"queued"}
# {"event":"granted","id":"L3f2a1-3","yield_signal":"SIGUSR1"}
# {"event":"ping"}
# {"event":"yield"}        <- something more important wants the card

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
