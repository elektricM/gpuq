#!/bin/sh
# Example for hooks.offload: free VRAM held by a desktop session so a large job fits.
#
# gpud runs this once when a waiting job does not fit and no lease is active, then measures
# VRAM again after hooks.offload_settle_s. The command is run directly (no shell), so point
# the config at this script:
#
#   [hooks]
#   offload = ["/usr/local/bin/offload-hook.sh"]
#   offload_timeout_s = 180
#   offload_settle_s = 3
#
# What "free the GPU" means depends on the machine. Two common shapes:
#   * stop the display manager:        systemctl stop display-manager
#   * switch the compositor to the iGPU or a lighter session
# Use whatever returns the VRAM, and make sure something restores the desktop afterwards.
# The hook needs the right to do this: run gpud as root, or add a sudoers or polkit rule.
set -eu
systemctl stop display-manager.service
