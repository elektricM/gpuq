# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 Martin Dolez
#
# Spec file for Fedora (COPR) and openSUSE (OBS). Builds offline from the release
# tarball and the vendored dependencies.

# CFLAGS with -flto break ring's assembly at link time. Cargo still applies its own LTO.
%global _lto_cflags %{nil}

Name:           gpuq
Version:        0.2.0
Release:        1%{?dist}
Summary:        GPU scheduler for Linux
# gpuq is GPL-3.0-or-later; the rest covers the vendored crates linked into the binary.
License:        GPL-3.0-or-later AND Apache-2.0 AND BSD-3-Clause AND CDLA-Permissive-2.0 AND ISC AND MIT AND Unicode-3.0 AND (Apache-2.0 OR BSL-1.0) AND (Apache-2.0 OR MIT) AND (Apache-2.0 OR ISC OR MIT) AND (MIT OR Unlicense)
URL:            https://github.com/elektricM/gpuq
Source0:        %{url}/archive/refs/tags/v%{version}/%{name}-%{version}.tar.gz
Source1:        %{url}/releases/download/v%{version}/%{name}-%{version}-vendor.tar.xz

ExclusiveArch:  x86_64 aarch64

BuildRequires:  cargo >= 1.80
BuildRequires:  rust >= 1.80
BuildRequires:  gcc
%if 0%{?suse_version}
BuildRequires:  sysuser-tools
BuildRequires:  systemd-rpm-macros
%sysusers_requires
# The sysusers file adds gpuq to these groups.
Requires(pre):  group(video)
Requires(pre):  group(render)
%{?systemd_ordering}
%else
BuildRequires:  systemd-rpm-macros
%{?systemd_requires}
%endif

%description
gpuq is a GPU scheduler for Linux machines with one or more GPUs. Programs
request the GPU through gpuq, which grants it by priority and available VRAM.
To free the card, gpuq can signal running jobs, stop and start services, and
unload models from LLM servers.

%prep
%autosetup -n %{name}-%{version} -a 1
cat packaging/vendor-config.toml >> .cargo/config.toml

%build
export RUSTFLAGS="%{?build_rustflags}"
export CARGO_PROFILE_RELEASE_STRIP=false
export CARGO_PROFILE_RELEASE_DEBUG=1
cargo build --release --offline --locked %{?_smp_mflags}
%if 0%{?suse_version}
%sysusers_generate_pre packaging/systemd/%{name}.sysusers %{name} %{name}.conf
%endif

%install
install -Dpm755 target/release/%{name} %{buildroot}%{_bindir}/%{name}
install -Dpm644 packaging/systemd/%{name}.service %{buildroot}%{_unitdir}/%{name}.service
install -Dpm644 packaging/systemd/%{name}.user.service %{buildroot}%{_userunitdir}/%{name}.service
install -Dpm644 packaging/systemd/%{name}.sysusers %{buildroot}%{_sysusersdir}/%{name}.conf
install -Dpm644 examples/%{name}.toml %{buildroot}%{_sysconfdir}/%{name}/config.toml
install -Dpm644 -t %{buildroot}%{_mandir}/man1 man/*.1
install -Dpm644 completions/%{name}.bash %{buildroot}%{_datadir}/bash-completion/completions/%{name}
install -Dpm644 completions/_%{name} %{buildroot}%{_datadir}/zsh/site-functions/_%{name}
install -Dpm644 completions/%{name}.fish %{buildroot}%{_datadir}/fish/vendor_completions.d/%{name}.fish

%check
export RUSTFLAGS="%{?build_rustflags}"
cargo test --offline --locked %{?_smp_mflags}

%if 0%{?suse_version}
%pre -f %{name}.pre
%service_add_pre %{name}.service

%post
%service_add_post %{name}.service

%preun
%service_del_preun %{name}.service

%postun
%service_del_postun %{name}.service
%else
%post
%systemd_post %{name}.service
%systemd_user_post %{name}.service

%preun
%systemd_preun %{name}.service
%systemd_user_preun %{name}.service

%postun
%systemd_postun_with_restart %{name}.service
%systemd_user_postun %{name}.service
%endif

%files
%license LICENSE
%doc README.md CHANGELOG.md docs examples
%{_bindir}/%{name}
%{_unitdir}/%{name}.service
%{_userunitdir}/%{name}.service
%{_sysusersdir}/%{name}.conf
%dir %{_sysconfdir}/%{name}
%config(noreplace) %{_sysconfdir}/%{name}/config.toml
%{_mandir}/man1/%{name}.1*
%{_mandir}/man1/%{name}-*.1*
%dir %{_datadir}/bash-completion
%dir %{_datadir}/bash-completion/completions
%{_datadir}/bash-completion/completions/%{name}
%dir %{_datadir}/zsh
%dir %{_datadir}/zsh/site-functions
%{_datadir}/zsh/site-functions/_%{name}
%dir %{_datadir}/fish
%dir %{_datadir}/fish/vendor_completions.d
%{_datadir}/fish/vendor_completions.d/%{name}.fish

%changelog
* Sat Oct 17 2026 Martin Dolez <martin@dolez.fr> - 0.2.0-1
- Renamed from gpud to gpuq
- Man pages and shell completions
