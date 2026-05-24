Name:           linuxqq-clipsync
Version:        0.1.0
Release:        1%{?dist}
Summary:        Clipboard bridge between X11 and Wayland for LinuxQQ

%global commit 8e8748335b340b8216c409ad47d4f9f3c4c970e9

License:        MIT
URL:            https://github.com/W-874/linuxqq-clipsync
Source0:        %{url}/archive/8e8748335b340b8216c409ad47d4f9f3c4c970e9/%{name}-%{commit}.tar.gz

BuildRequires:  cargo
BuildRequires:  rust-packaging
BuildRequires:  systemd-rpm-macros
Requires:       wl-clipboard
Requires:       xclip

%description
linuxqq-clipsync synchronizes the X11 and Wayland clipboards to work around
clipboard interoperability issues when LinuxQQ runs on Wayland.

%prep
%autosetup -n %{name}-%{commit}

%generate_buildrequires
%cargo_generate_buildrequires

%build
%cargo_build

%install
%cargo_install
install -Dpm0644 linuxqq-clipsync.service %{buildroot}%{_userunitdir}/linuxqq-clipsync.service

%files
%license LICENSE
%doc README.md
%{_bindir}/linuxqq-clipsync
%{_userunitdir}/linuxqq-clipsync.service

%changelog
* Sun May 24 2026 W-874 <1317825684@qq.com> - 0.1.0-1
- Initial COPR package
