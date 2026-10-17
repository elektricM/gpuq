# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 Martin Dolez
{
  lib,
  rustPlatform,
  fetchFromGitHub,
  installShellFiles,
}:

rustPlatform.buildRustPackage (finalAttrs: {
  pname = "gpuq";
  version = "0.3.0";

  src = fetchFromGitHub {
    owner = "elektricM";
    repo = "gpuq";
    tag = "v${finalAttrs.version}";
    hash = "sha256-CzE9iY1aNkHwjcHsNbEPbB6Rv6UMZFppK1cX+QtN6AU=";
  };

  cargoHash = "sha256-Y3E12YI87bhUI+N4NdsoJ6BCc0y6lC2+FLe0vLbsUkI=";

  nativeBuildInputs = [ installShellFiles ];

  postInstall = ''
    installManPage man/*.1
    installShellCompletion --cmd gpuq \
      --bash completions/gpuq.bash \
      --zsh completions/_gpuq \
      --fish completions/gpuq.fish

    install -Dm644 packaging/systemd/gpuq.service $out/lib/systemd/system/gpuq.service
    install -Dm644 packaging/systemd/gpuq.user.service $out/lib/systemd/user/gpuq.service
    substituteInPlace $out/lib/systemd/system/gpuq.service $out/lib/systemd/user/gpuq.service \
      --replace-fail /usr/bin/gpuq $out/bin/gpuq
    install -Dm644 examples/* -t $out/share/doc/gpuq/examples
  '';

  meta = {
    description = "GPU scheduler for Linux";
    homepage = "https://github.com/elektricM/gpuq";
    changelog = "https://github.com/elektricM/gpuq/blob/v${finalAttrs.version}/CHANGELOG.md";
    license = lib.licenses.gpl3Plus;
    maintainers = with lib.maintainers; [ ];
    mainProgram = "gpuq";
    platforms = lib.platforms.linux;
  };
})
