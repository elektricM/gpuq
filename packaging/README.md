# Packaging

Recipes for each distribution channel, and the commands to publish them. Nothing here is
published automatically except the GitHub release.

| Directory      | Channel                                  |
| -------------- | ---------------------------------------- |
| `aur/`         | Arch Linux AUR                           |
| `rpm/`         | Fedora COPR, openSUSE OBS                |
| `debian/`      | Ubuntu PPA (Launchpad), base for Debian  |
| `nix/`         | nixpkgs                                  |
| `systemd/`     | units and sysusers file, used by all     |
| `deb/`         | maintainer script for the cargo-deb .deb |

Every package installs the binary, the system and user units, the sysusers file, the
default configuration as `/etc/gpuq/config.toml`, the man pages, the bash, zsh and fish
completions, and the documentation.

## Release assets

The release workflow (`.github/workflows/release.yml`) runs on `v*` tags and attaches:

- `gpuq-<version>-x86_64-unknown-linux-musl.tar.gz` and the aarch64 equivalent: static
  binary, man pages, completions, units, examples and docs
- `.deb` and `.rpm` packages built from the static binaries (cargo-deb, cargo-generate-rpm)
- `gpuq-<version>-vendor.tar.xz`: output of `cargo vendor`, a `vendor/` directory
- `gpuq-<version>-vendor-config.toml`: the Cargo configuration that points at `vendor/`
- `SHA256SUMS`

To build without network access from the source tarball:

```sh
tar -xzf gpuq-0.2.0.tar.gz
cd gpuq-0.2.0
tar -xJf ../gpuq-0.2.0-vendor.tar.xz
cat packaging/vendor-config.toml >> .cargo/config.toml
cargo build --release --offline --locked
```

## Man pages and completions

`man/` and `completions/` are generated from the clap definitions and committed. After
changing the command line, run:

```sh
cargo xtask gen
```

CI fails when the committed files differ from the generated ones (`cargo xtask check`).

## Release checklist

1. Bump `version` in `Cargo.toml`, run `cargo xtask gen`, update `CHANGELOG.md`, the
   version in `aur/PKGBUILD`, `rpm/gpuq.spec`, `debian/changelog` and `nix/package.nix`.
2. Tag `vX.Y.Z` and push the tag. Wait for the release workflow.
3. Update the checksums: `sha256sums` in `aur/PKGBUILD` (and regenerate `.SRCINFO`), `hash`
   and `cargoHash` in `nix/package.nix`.

## crates.io

Needs a crates.io account (GitHub login) and an API token with the `publish-new` and
`publish-update` scopes, from https://crates.io/settings/tokens.

```sh
cargo login            # paste the token
cargo publish --dry-run -p gpuq
cargo publish -p gpuq
```

## Arch Linux (AUR)

Needs an AUR account (https://aur.archlinux.org/register) with an SSH public key added in
the account settings.

```sh
git clone ssh://aur@aur.archlinux.org/gpuq.git aur-gpuq
cp packaging/aur/PKGBUILD aur-gpuq/
cd aur-gpuq
makepkg --printsrcinfo > .SRCINFO
makepkg -f                          # build and test once
namcap PKGBUILD *.pkg.tar.zst
git add PKGBUILD .SRCINFO
git commit -m "Update to 0.2.0"
git push
```

The first push creates the package. `options=('!lto')` must stay: makepkg's LTO flags break
ring at link time.

## Fedora COPR

Needs a Fedora account (https://accounts.fedoraproject.org) and the COPR API token from
https://copr.fedorainfracloud.org/api/ saved as `~/.config/copr`.

```sh
sudo dnf install copr-cli rpm-build rpmdevtools
copr-cli create gpuq --chroot fedora-rawhide-x86_64 --chroot fedora-rawhide-aarch64 \
  --chroot fedora-44-x86_64 --chroot fedora-44-aarch64 \
  --description "GPU scheduler for Linux" --enable-net off
spectool -g -C ~/rpmbuild/SOURCES packaging/rpm/gpuq.spec
rpmbuild -bs --define "_sourcedir $HOME/rpmbuild/SOURCES" packaging/rpm/gpuq.spec
copr-cli build gpuq ~/rpmbuild/SRPMS/gpuq-0.2.0-1.*.src.rpm
```

Users then run `sudo dnf copr enable <user>/gpuq && sudo dnf install gpuq`.

## openSUSE OBS

Needs an openSUSE account (https://build.opensuse.org) and `osc` configured with it
(`osc` asks for the credentials on first use).

```sh
sudo zypper install osc     # or: pip install osc
osc checkout home:<user>
cd home:<user>
osc mkpac gpuq
cd gpuq
cp ../../packaging/rpm/gpuq.spec .
curl -L -o gpuq-0.2.0.tar.gz https://github.com/elektricM/gpuq/archive/refs/tags/v0.2.0.tar.gz
curl -LO https://github.com/elektricM/gpuq/releases/download/v0.2.0/gpuq-0.2.0-vendor.tar.xz
osc add gpuq.spec gpuq-0.2.0.tar.gz gpuq-0.2.0-vendor.tar.xz
osc build openSUSE_Tumbleweed x86_64   # local test build
osc commit -m "Update to 0.2.0"
```

Enable the openSUSE Tumbleweed and Leap repositories for the project in the web interface
(Repositories, Add from a Distribution).

## Ubuntu PPA (Launchpad)

Needs a Launchpad account, an OpenPGP key uploaded to keyserver.ubuntu.com and imported in
Launchpad, the signed Ubuntu Code of Conduct, and a PPA created at
https://launchpad.net/~<user>/+activate-ppa.

`debian/rules` uses `cargo-1.85` and `rustc-1.85` from the Ubuntu 24.04 archive. Launchpad
builders have no network, so the vendored crates are shipped as a second orig tarball.

```sh
sudo apt install devscripts debhelper dput
curl -LO https://github.com/elektricM/gpuq/archive/refs/tags/v0.2.0.tar.gz
curl -LO https://github.com/elektricM/gpuq/releases/download/v0.2.0/gpuq-0.2.0-vendor.tar.xz
mv v0.2.0.tar.gz gpuq_0.2.0.orig.tar.gz
mv gpuq-0.2.0-vendor.tar.xz gpuq_0.2.0.orig-vendor.tar.xz
tar -xzf gpuq_0.2.0.orig.tar.gz
cd gpuq-0.2.0
tar -xJf ../gpuq_0.2.0.orig-vendor.tar.xz
cp -r packaging/debian debian
dpkg-buildpackage -S -sa -k<KEYID>     # signed source package
cd ..
dput ppa:<user>/gpuq gpuq_0.2.0-1~noble1_source.changes
```

For another Ubuntu release, add a `debian/changelog` entry with the release name in the
version and distribution (for example `0.2.0-1~questing1 questing`) and upload again.

## Debian

Debian builds Rust programs against crates packaged in Debian, not vendored ones, so the
`debian/` directory here is a starting point only.

1. File an ITP bug against `wnpp` (`reportbug wnpp`, choose ITP) for gpuq.
2. Package the missing crates through the Debian Rust team
   (https://salsa.debian.org/rust-team/debcargo-conf, `debcargo`), or ask the team about
   vendoring.
3. Upload the source package to https://mentors.debian.net (account and OpenPGP key
   needed) with `dput mentors gpuq_<version>_source.changes`, and file an RFS bug against
   `sponsorship-requests` to find a sponsor.

## Fedora

Official Fedora packages also build against packaged crates. To get gpuq into Fedora:

1. Generate a spec with `rust2rpm gpuq` once the crate is on crates.io, and check that every
   dependency is packaged (`dnf repoquery 'crate(...)'`).
2. Build it in COPR or with `fedpkg --release rawhide mockbuild`.
3. Request the review: open a Review Request bug at
   https://bugzilla.redhat.com/enter_bug.cgi?product=Fedora&component=Package%20Review
   with the spec and SRPM URLs. You need to be in the `packager` group, which needs a
   sponsor (https://docs.fedoraproject.org/en-US/package-maintainers/How_to_Get_Sponsored_into_the_Packager_Group/).

## nixpkgs

Needs a GitHub fork of NixOS/nixpkgs. See [nix/README.md](nix/README.md) for the
maintainer entry and the pull request steps.
