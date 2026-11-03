# The README demo

`docs/demo.gif` is recorded by `record.sh` here. It starts a throwaway gpuq daemon with
`demo.toml` (its own ports, 19390 and 19434), so a gpuq already running on the machine is not
involved and no GPU is used:

- `fake-vram.sh` stands in for the VRAM probe of two 24 GB cards. It adds up the MiB that the
  demo programs write under `/tmp/gpuq-demo/vram/<card>/` while they run.
- `mock-ollama.py` answers the Ollama calls gpuq makes (`/api/ps`, `/api/generate`). A loaded
  model holds 10 GiB on each card.
- `bin/render` and `bin/train` are sleep scripts that hold 8 GiB and 22 GiB on the card gpuq
  granted them (`GPUQ_CARD`).

The scheduling in the recording is gpuq's own: it decides when to unload the model, which card
each job gets, which job waits, and when the model comes back.

```sh
cargo build --release
PATH=/path/to/agg:$PATH docs/demo/record.sh
```

Needs python3, curl, asciinema 2 and [agg](https://github.com/asciinema/agg).
