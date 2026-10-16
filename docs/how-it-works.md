# How gpuq works

gpuq rests on four ideas.

- **Lease.** A program holds the card through a lease that names its class and how much VRAM it
  needs. The lease is an open connection, so it ends when the program exits or crashes.
- **Class.** Each lease has a priority class. Higher classes go first; programs that fit in the
  free VRAM can share the card.
- **Yield.** Nothing is killed. When a higher class needs the card, gpuq asks a yieldable
  program to step aside (a signal, a file, or stopping its service) and waits for it.
- **Resident.** One program can be kept loaded whenever the card is idle, usually an LLM so
  chat answers right away. gpuq unloads it for other work and reloads it after, KV cache
  included where the server supports it. This is optional.

## An evening with gpuq

| Time | What happens | What gpuq does |
|---|---|---|
| 18:00 | You chat with your local LLM | It is the resident program, already loaded |
| 18:05 | You queue a ComfyUI render | Waits for a 10 s pause in the chat, unloads the LLM, starts ComfyUI |
| 18:20 | The render is done | ComfyUI idles for 2 min, gpuq stops it and reloads the LLM with its cache |
| 18:30 | `gpuq run --class batch --yieldable -- train.py` | Waits until the chat has been quiet for 5 min, then runs it |
| 18:45 | A chat message arrives | Asks the training to yield; it saves and exits, the LLM reloads and answers |

All the timings are settings.

## Several cards

With more than one card, every card has its own state, its own VRAM accounting and its own
yields, but there is still one queue.

- A waiter only blocks on the cards it can use. A job pinned to one card never holds up a
  job for another.
- Yields are per card: when the LLM needs its card back, only the yieldable jobs on that card
  are asked to step aside.
- `--card any` takes the first card with enough room. If none has, it waits under the rules of
  the LLM's card.

Details are in [integrations.md](integrations.md#several-gpus).

## Priorities

| Class | Default | Typical use |
|---|---|---|
| `realtime` | 0 | Must never wait, like live transcription |
| `llm` | 1 | Requests through the LLM gate |
| `interactive` | 2 | You at the keyboard: ComfyUI, `gpuq hold`, a game |
| `background` | 3 | Services that can wait, like Immich ML |
| `batch` | 4 | Training, bulk processing |

Lower number wins. The order is yours to change, for example to put your own work above the LLM:

```toml
[classes]
interactive = 1
llm = 2
```

Details of every timing and class setting are in [config.md](config.md).
