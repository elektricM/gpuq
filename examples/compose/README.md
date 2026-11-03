# Ollama, Open WebUI and ComfyUI with gpuq

A common home lab stack on one GPU: Ollama for chat, Open WebUI in front of it, ComfyUI for
images. Without gpuq, a ComfyUI workflow started while Ollama holds a model runs out of VRAM,
or the next chat does.

With this setup:

- Chat requests go to gpuq on 11434, which passes them to Ollama when the card is free.
- A ComfyUI prompt (from its web page on 8188 or from Open WebUI's image generation) makes gpuq
  unload the Ollama model, start the `comfyui` container and pass the prompt on. Opening the
  page alone does not start it.
- After 120 s without prompts gpuq stops ComfyUI, and the model is loaded again for chat.
- Chat outranks ComfyUI (class `llm` before `interactive`). A chat request while ComfyUI works
  lets the running prompt finish, then gpuq stops ComfyUI and answers the chat; prompts sent
  meanwhile wait for ComfyUI's next turn. With `yieldable = false`, chat waits until ComfyUI
  has been idle for `idle_release_s` instead.

gpuq runs on the host, where it can see the card and run `docker start` and `docker stop`.

## Setup

1. Install gpuq on the host (`install.sh` in the repository root, or a package).
2. `cp gpuq.toml /etc/gpuq/config.toml`. If gpuq runs as the `gpuq` user of the packaged unit,
   give it the docker group: `systemctl edit gpuq`, then
   ```
   [Service]
   SupplementaryGroups=video render docker
   ```
3. `docker compose up -d`. The `comfyui` container may start once here; gpuq stops it when it
   starts.
4. `gpuq check-config`, then `sudo systemctl enable --now gpuq`.
5. Open WebUI on http://HOST:3000, ComfyUI on http://HOST:8188. `gpuq status` shows who has
   the card.

The NVIDIA Container Toolkit must be installed for the `deploy.resources` GPU reservations.
For an AMD card, use `ollama/ollama:rocm` and a ROCm ComfyUI image, and replace the `deploy`
blocks with `devices: ["/dev/kfd", "/dev/dri"]`.
