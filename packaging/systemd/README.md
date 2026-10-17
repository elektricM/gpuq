# systemd units

`gpuq.service` runs gpuq as the `gpuq` system user. The packages install it but do not
enable it, because the configuration has to be written first:

```sh
gpuq check-config
sudo systemctl enable --now gpuq
```

Logs are in `journalctl -u gpuq`.

To control `systemctl --user` services such as ComfyUI, use `gpuq.user.service` instead
(`systemctl --user enable --now gpuq`, with `loginctl enable-linger` so it runs without a
login session).

When installing with `cargo install`, copy the binary, the unit and the system user:

```sh
sudo install -m 755 ~/.cargo/bin/gpuq /usr/bin/gpuq
sudo install -m 644 packaging/systemd/gpuq.service /etc/systemd/system/
sudo install -m 644 packaging/systemd/gpuq.sysusers /usr/lib/sysusers.d/gpuq.conf
sudo systemd-sysusers && sudo systemctl daemon-reload
```
