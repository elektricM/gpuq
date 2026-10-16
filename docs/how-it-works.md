# How gpud works

gpud rests on four ideas.

- **Lease.** A program holds the card through a lease that names its class and how much VRAM it
  needs. The lease is an open connection, so it ends when the program exits or crashes.
- **Class.** Each lease has a priority class. Higher classes go first; programs that fit in the
  free VRAM can share the card.
- **Yield.** Nothing is killed. When a higher class needs the card, gpud asks a yieldable
  program to step aside (a signal, a file, or stopping its service) and waits for it.
- **Resident.** One program can be kept loaded whenever the card is idle, usually an LLM so
  chat answers right away. gpud unloads it for other work and reloads it after, KV cache
  included where the server supports it. This is optional.

## An evening with gpud

| Time | What happens | What gpud does |
|---|---|---|
| 18:00 | You chat with your local LLM | It is the resident program, already loaded |
| 18:05 | You queue a ComfyUI render | Waits for a 10 s pause in the chat, unloads the LLM, starts ComfyUI |
| 18:20 | The render is done | ComfyUI idles for 2 min, gpud stops it and reloads the LLM with its cache |
| 18:30 | `gpud run --class batch --yieldable -- train.py` | Waits until the chat has been quiet for 5 min, then runs it |
| 18:45 | A chat message arrives | Asks the training to yield; it saves and exits, the LLM reloads and answers |

All the timings are settings.

## Priorities

| Class | Default | Typical use |
|---|---|---|
| `realtime` | 0 | Must never wait, like live transcription |
| `llm` | 1 | Requests through the LLM gate |
| `interactive` | 2 | You at the keyboard: ComfyUI, `gpud hold`, a game |
| `background` | 3 | Services that can wait, like Immich ML |
| `batch` | 4 | Training, bulk processing |

Lower number wins. The order is yours to change, for example to put your own work above the LLM:

```toml
[classes]
interactive = 1
llm = 2
```

Details of every timing and class setting are in [config.md](config.md).
