#!/bin/sh
# Install the gpuq binary from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/elektricM/gpuq/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/elektricM/gpuq/main/install.sh | sh -s -- --system
#
# Options:
#   --system         install to /usr/local/bin, with sudo when not root
#   --bindir DIR     install to DIR (default ~/.local/bin)
#   --version X.Y.Z  a given release instead of the latest
#
# Downloads the static Linux binary for this machine (x86_64 or aarch64), checks it against
# the release's SHA256SUMS and copies it to the chosen directory. Nothing else is changed:
# the systemd unit and example configs are in the tarball, see the README.
set -eu

repo=elektricM/gpuq
bindir="${HOME:-}/.local/bin"
version=
sudo=

die() {
    echo "install.sh: $*" >&2
    exit 1
}

while [ $# -gt 0 ]; do
    case $1 in
    --system) bindir=/usr/local/bin ;;
    --bindir)
        [ $# -ge 2 ] || die "--bindir needs a directory"
        bindir=$2
        shift
        ;;
    --version)
        [ $# -ge 2 ] || die "--version needs a version"
        version=${2#v}
        shift
        ;;
    -h | --help)
        sed -n '2,14s/^# \{0,1\}//p' "$0" 2>/dev/null || echo "see the comment at the top of install.sh"
        exit 0
        ;;
    *) die "unknown option $1" ;;
    esac
    shift
done

[ "$(uname -s)" = Linux ] || die "gpuq runs on Linux only"
case $(uname -m) in
x86_64 | amd64) arch=x86_64 ;;
aarch64 | arm64) arch=aarch64 ;;
*) die "no prebuilt binary for $(uname -m); build it with: cargo install gpuq" ;;
esac

command -v curl >/dev/null || die "needs curl"
command -v tar >/dev/null || die "needs tar"
if command -v sha256sum >/dev/null; then
    sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null; then
    sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
    die "needs sha256sum or shasum"
fi

if [ -z "$version" ]; then
    # /releases/latest redirects to /releases/tag/vX.Y.Z; no API token or rate limit involved.
    url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest") ||
        die "cannot reach github.com"
    version=${url##*/v}
    case $version in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) die "cannot tell the latest version from $url" ;;
    esac
fi

name="gpuq-$version-$arch-unknown-linux-musl"
base="https://github.com/$repo/releases/download/v$version"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading gpuq $version for $arch"
curl -fsSL -o "$tmp/$name.tar.gz" "$base/$name.tar.gz" || die "download failed: $base/$name.tar.gz"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || die "download failed: $base/SHA256SUMS"

want=$(awk -v f="$name.tar.gz" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")
[ -n "$want" ] || die "$name.tar.gz is not listed in SHA256SUMS"
got=$(sha256 "$tmp/$name.tar.gz")
[ "$got" = "$want" ] || die "checksum mismatch for $name.tar.gz: expected $want, got $got"
echo "Checksum OK"

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
[ -x "$tmp/$name/gpuq" ] || die "the tarball has no gpuq binary"

if [ ! -d "$bindir" ]; then
    mkdir -p "$bindir" 2>/dev/null || true
fi
if [ ! -w "$bindir" ] && [ "$(id -u)" -ne 0 ]; then
    command -v sudo >/dev/null || die "$bindir is not writable and sudo is missing"
    sudo=sudo
fi
$sudo mkdir -p "$bindir"
$sudo install -m 0755 "$tmp/$name/gpuq" "$bindir/gpuq"
echo "Installed $bindir/gpuq ($("$bindir/gpuq" --version))"

case :${PATH:-}: in
*:"$bindir":*) ;;
*) echo "Note: $bindir is not on your PATH" ;;
esac
cat <<EOF

Next:
  mkdir -p ~/.config/gpuq
  printf '[llm]\\nbackend = "none"\\n' > ~/.config/gpuq/config.toml
  gpuq check-config
  gpuq daemon

Examples for Ollama, ComfyUI and others: https://github.com/$repo/tree/main/examples
EOF
