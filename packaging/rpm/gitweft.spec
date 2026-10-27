# Release RPM, built offline from the source and vendored-crates tarballs that
# `just dist-src` writes (scripts/dist/sources.py) with scripts/dist/build.sh. `just rpm`
# makes the SRPM and RPM in target/rpm; COPR builds the SRPM as it is, and
# scripts/dist/rpm.sh does the same in a Fedora 44 container.

Name:           gitweft
Version:        1.0.0
Release:        1%{?dist}
Summary:        A fast, offline Git client

# %%bundled_license (the vendored crates' licenses) and %%bundled_provides_1 … _count come
# with the tarballs.
%{load:%{_sourcedir}/gitweft-%{version}-bundled.inc}

# Gitweft is GPL-3.0-or-later; the bundled IBM Plex, Inter and JetBrains Mono fonts are OFL-1.1.
License:        GPL-3.0-or-later AND OFL-1.1 AND %{bundled_license}
URL:            https://github.com/sorrelfox/gitweft
ExclusiveArch:  x86_64

Source0:        gitweft-%{version}.tar.xz
Source1:        gitweft-%{version}-vendor.tar.xz
Source2:        gitweft-%{version}-bundled.inc

BuildRequires:  cargo >= 1.98
BuildRequires:  rust >= 1.98
BuildRequires:  gcc
# rsvg-convert: scripts/dist/icons.sh renders the PNG icons.
BuildRequires:  librsvg2-tools
BuildRequires:  pkgconfig(xcb)
BuildRequires:  pkgconfig(xkbcommon)
BuildRequires:  pkgconfig(xkbcommon-x11)
# Found with pkg-config, so the crates don't compile their bundled copies.
BuildRequires:  pkgconfig(freetype2)
BuildRequires:  pkgconfig(fontconfig)
BuildRequires:  pkgconfig(egl)
BuildRequires:  desktop-file-utils
BuildRequires:  appstream

Requires:       git-core >= 2.39
# Loaded at run time (dlopen), so the automatic dependency scan doesn't see them.
Requires:       libvulkan.so.1()(64bit)
Requires:       libwayland-client.so.0()(64bit)
Requires:       libwayland-cursor.so.0()(64bit)
Recommends:     mesa-vulkan-drivers

%{lua:
for i = 1, tonumber(rpm.expand("%{bundled_provides_count}")) do
    print("Provides: " .. rpm.expand("%{bundled_provides_" .. i .. "}") .. "\n")
end
}

# The dist profile strips the binary's debug info.
%global debug_package %{nil}

%description
Gitweft is a native Git client with a visual commit graph, staging by file, hunk or line,
a merge conflict tool, branches, remotes, tags and stashes, and undo for local operations.
It has no accounts, no cloud features and no telemetry, and works without an internet
connection: the only network traffic comes from git itself, talking to your remotes.

%prep
%setup -q -b 1

%build
bash scripts/dist/build.sh

%install
id=io.github.sorrelfox.Gitweft
install -Dm0755 target/dist/gitweft %{buildroot}%{_bindir}/gitweft
install -Dm0644 packaging/linux/$id.desktop %{buildroot}%{_datadir}/applications/$id.desktop
install -Dm0644 packaging/linux/$id.metainfo.xml %{buildroot}%{_metainfodir}/$id.metainfo.xml
bash scripts/dist/icons.sh %{buildroot}%{_datadir}
bash scripts/dist/manpage.sh target/dist/gitweft %{buildroot}%{_mandir}/man1/gitweft.1
cp -p assets/fonts/IBMPlex-OFL.txt assets/fonts/Inter-OFL.txt assets/fonts/JetBrainsMono-OFL.txt .

%check
desktop-file-validate %{buildroot}%{_datadir}/applications/io.github.sorrelfox.Gitweft.desktop
appstreamcli validate --no-net %{buildroot}%{_metainfodir}/io.github.sorrelfox.Gitweft.metainfo.xml
# The packaged binary runs (its argument handling needs no display).
%{buildroot}%{_bindir}/gitweft --version

%files
%license LICENSE IBMPlex-OFL.txt Inter-OFL.txt JetBrainsMono-OFL.txt THIRD-PARTY-LICENSES.txt
%{_bindir}/gitweft
%{_datadir}/applications/io.github.sorrelfox.Gitweft.desktop
%{_metainfodir}/io.github.sorrelfox.Gitweft.metainfo.xml
%{_datadir}/icons/hicolor/*/apps/io.github.sorrelfox.Gitweft*
%{_mandir}/man1/gitweft.1*

# The newest entry says what the metainfo's newest <release> says, on the same day
# (scripts/dist/check-version.sh).
%changelog
* Tue Oct 27 2026 sorrelfox <sorrelfox@users.noreply.github.com> - 1.0.0-1
- The first release: a commit graph that stays quick on very large repositories, staging
  by file, hunk or line, a merge conflict tool, an interactive rebase editor, local code
  review, undo for local operations, twelve interface languages, and RPM, DEB, AppImage
  and Flatpak packages.

* Sat Sep 12 2026 sorrelfox <sorrelfox@users.noreply.github.com> - 0.1.0-1
- Development build: graph, staging, commits, branches, remotes, merge conflicts, tabs and undo.

* Fri Sep 11 2026 sorrelfox <sorrelfox@users.noreply.github.com> - 0.1.0-0.dev
- Development build.
