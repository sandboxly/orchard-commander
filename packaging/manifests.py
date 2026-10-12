"""Package-manager definitions for a published release.

    uv run python packaging/manifests.py 1.0.0 [--out dist/manifests]

Downloads the release's files from GitHub to compute their SHA-256 sums and
writes, below --out:

    homebrew/Casks/orchard-commander.rb                for the sandboxly/homebrew-tap repository
    winget/manifests/s/Sandboxly/OrchardCommander/<v>/ for a pull request to microsoft/winget-pkgs
    chocolatey/                                        `choco pack`, then `choco push`
    aur/PKGBUILD, aur/.SRCINFO                         for the orchard-commander-bin AUR package
"""

from __future__ import annotations

import argparse
import hashlib
import sys
import urllib.request
from pathlib import Path

REPO = "sandboxly/orchard-commander"
HOMEPAGE = "https://sandboxly.github.io/orchard-commander/"
SUMMARY = "Two-panel file manager with previews, archives, servers, search and a terminal"
DESCRIPTION = (
    "Orchard Commander is a keyboard-driven two-panel file manager in the spirit of Norton and "
    "Total Commander. It previews code, images, PDFs, audio and video, opens archives and SFTP, SMB, "
    "WebDAV and S3 servers as folders, and has find files, multi-rename and a built-in terminal."
)
ASSETS = {
    "mac": "Orchard-Commander-macOS.dmg",
    "setup": "Orchard-Commander-Setup.exe",
    "linux": "Orchard-Commander-Linux.tar.gz",
}


def url(version: str, asset: str) -> str:
    return f"https://github.com/{REPO}/releases/download/v{version}/{asset}"


def sha256_of(address: str) -> str:
    digest = hashlib.sha256()
    with urllib.request.urlopen(address) as response:  # noqa: S310 (a fixed https URL)
        while chunk := response.read(1 << 20):
            digest.update(chunk)
    return digest.hexdigest()


def homebrew(version: str, sha: dict[str, str]) -> str:
    return f'''cask "orchard-commander" do
  version "{version}"
  sha256 "{sha['mac']}"

  url "https://github.com/{REPO}/releases/download/v#{{version}}/{ASSETS['mac']}"
  name "Orchard Commander"
  desc "{SUMMARY}"
  homepage "{HOMEPAGE}"

  livecheck do
    url :url
    strategy :github_latest
  end

  depends_on arch: :arm64
  depends_on macos: ">= :sonoma"

  app "Orchard Commander.app"

  zap trash: [
    "~/Library/Application Support/Orchard Commander",
    "~/Library/Caches/Orchard Commander",
    "~/Library/Preferences/io.github.orchard-commander.plist",
    "~/Library/Saved Application State/io.github.orchard-commander.savedState",
  ]
end
'''


def winget(version: str, sha: dict[str, str]) -> dict[str, str]:
    ident = "Sandboxly.OrchardCommander"
    head = "# yaml-language-server: $schema=https://aka.ms/winget-manifest.{kind}.1.6.0.schema.json\n\n"
    return {
        f"{ident}.yaml": head.format(kind="version") + f"""PackageIdentifier: {ident}
PackageVersion: {version}
DefaultLocale: en-US
ManifestType: version
ManifestVersion: 1.6.0
""",
        f"{ident}.installer.yaml": head.format(kind="installer") + f"""PackageIdentifier: {ident}
PackageVersion: {version}
InstallerType: inno
Scope: user
UpgradeBehavior: install
MinimumOSVersion: 10.0.0.0
Installers:
- Architecture: x64
  Scope: user
  InstallerUrl: {url(version, ASSETS['setup'])}
  InstallerSha256: {sha['setup'].upper()}
  InstallerSwitches:
    Custom: /CURRENTUSER
- Architecture: x64
  Scope: machine
  InstallerUrl: {url(version, ASSETS['setup'])}
  InstallerSha256: {sha['setup'].upper()}
  InstallerSwitches:
    Custom: /ALLUSERS
ManifestType: installer
ManifestVersion: 1.6.0
""",
        f"{ident}.locale.en-US.yaml": head.format(kind="defaultLocale") + f"""PackageIdentifier: {ident}
PackageVersion: {version}
PackageLocale: en-US
Publisher: Stephan Lengl
PublisherUrl: https://github.com/sandboxly
PublisherSupportUrl: https://github.com/{REPO}/issues
Author: Stephan Lengl
PackageName: Orchard Commander
PackageUrl: {HOMEPAGE}
License: MIT
LicenseUrl: https://github.com/{REPO}/blob/main/LICENSE
ShortDescription: {SUMMARY}
Description: {DESCRIPTION}
Moniker: orchard-commander
Tags:
- file-manager
- total-commander
- norton-commander
- sftp
- terminal
ReleaseNotesUrl: https://github.com/{REPO}/releases/tag/v{version}
Documentations:
- DocumentLabel: User guide
  DocumentUrl: {HOMEPAGE}manual/
ManifestType: defaultLocale
ManifestVersion: 1.6.0
""",
    }


def chocolatey(version: str, sha: dict[str, str]) -> dict[str, str]:
    return {
        "orchard-commander.nuspec": f"""<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://schemas.microsoft.com/packaging/2015/06/nuspec.xsd">
  <metadata>
    <id>orchard-commander</id>
    <version>{version}</version>
    <title>Orchard Commander</title>
    <authors>Stephan Lengl</authors>
    <owners>sandboxly</owners>
    <projectUrl>{HOMEPAGE}</projectUrl>
    <iconUrl>{HOMEPAGE}icon.png</iconUrl>
    <licenseUrl>https://github.com/{REPO}/blob/main/LICENSE</licenseUrl>
    <requireLicenseAcceptance>false</requireLicenseAcceptance>
    <projectSourceUrl>https://github.com/{REPO}</projectSourceUrl>
    <docsUrl>{HOMEPAGE}manual/</docsUrl>
    <bugTrackerUrl>https://github.com/{REPO}/issues</bugTrackerUrl>
    <releaseNotes>https://github.com/{REPO}/releases/tag/v{version}</releaseNotes>
    <tags>file-manager commander total-commander norton-commander sftp terminal foss</tags>
    <summary>{SUMMARY}</summary>
    <description>{DESCRIPTION}</description>
  </metadata>
  <files>
    <file src="tools\\**" target="tools" />
  </files>
</package>
""",
        "tools/chocolateyinstall.ps1": f"""$ErrorActionPreference = 'Stop'

$packageArgs = @{{
  packageName    = $env:ChocolateyPackageName
  fileType       = 'exe'
  url64bit       = '{url(version, ASSETS['setup'])}'
  checksum64     = '{sha['setup']}'
  checksumType64 = 'sha256'
  silentArgs     = '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP- /ALLUSERS'
  validExitCodes = @(0)
}}

Install-ChocolateyPackage @packageArgs
""",
    }


def aur(version: str, sha: dict[str, str]) -> dict[str, str]:
    pkgbuild = f"""# Maintainer: Stephan Lengl <lengl.stephan@gmail.com>
pkgname=orchard-commander-bin
pkgver={version}
pkgrel=1
pkgdesc="{SUMMARY}"
arch=('x86_64')
url="{HOMEPAGE}"
license=('MIT')
depends=('libglvnd' 'libxkbcommon' 'libxkbcommon-x11' 'fontconfig' 'dbus' 'xcb-util-cursor' 'libpulse')
optdepends=('unrar: RAR archives' '7zip: RAR and 7z archives')
provides=('orchard-commander')
conflicts=('orchard-commander')
options=('!strip' '!debug')
source=("{ASSETS['linux'].replace('.tar.gz', '')}-$pkgver.tar.gz::https://github.com/{REPO}/releases/download/v$pkgver/{ASSETS['linux']}"
        "orchard-commander.desktop::https://raw.githubusercontent.com/{REPO}/v$pkgver/packaging/linux/orchard-commander.desktop"
        "orchard-commander.png::https://raw.githubusercontent.com/{REPO}/v$pkgver/packaging/linux/orchard-commander.png"
        "LICENSE::https://raw.githubusercontent.com/{REPO}/v$pkgver/LICENSE")
sha256sums=('{sha['linux']}'
            '{sha['desktop']}'
            '{sha['icon']}'
            '{sha['license']}')

package() {{
  install -d "$pkgdir/opt" "$pkgdir/usr/bin"
  cp -a orchard-commander "$pkgdir/opt/orchard-commander"
  ln -s /opt/orchard-commander/orchard-commander "$pkgdir/usr/bin/orchard-commander"
  install -Dm644 orchard-commander.desktop "$pkgdir/usr/share/applications/orchard-commander.desktop"
  install -Dm644 orchard-commander.png "$pkgdir/usr/share/icons/hicolor/256x256/apps/orchard-commander.png"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}}
"""
    srcinfo = f"""pkgbase = orchard-commander-bin
\tpkgdesc = {SUMMARY}
\tpkgver = {version}
\tpkgrel = 1
\turl = {HOMEPAGE}
\tarch = x86_64
\tlicense = MIT
\tdepends = libglvnd
\tdepends = libxkbcommon
\tdepends = libxkbcommon-x11
\tdepends = fontconfig
\tdepends = dbus
\tdepends = xcb-util-cursor
\tdepends = libpulse
\toptdepends = unrar: RAR archives
\toptdepends = 7zip: RAR and 7z archives
\tprovides = orchard-commander
\tconflicts = orchard-commander
\toptions = !strip
\toptions = !debug
\tsource = Orchard-Commander-Linux-{version}.tar.gz::https://github.com/{REPO}/releases/download/v{version}/{ASSETS['linux']}
\tsource = orchard-commander.desktop::https://raw.githubusercontent.com/{REPO}/v{version}/packaging/linux/orchard-commander.desktop
\tsource = orchard-commander.png::https://raw.githubusercontent.com/{REPO}/v{version}/packaging/linux/orchard-commander.png
\tsource = LICENSE::https://raw.githubusercontent.com/{REPO}/v{version}/LICENSE
\tsha256sums = {sha['linux']}
\tsha256sums = {sha['desktop']}
\tsha256sums = {sha['icon']}
\tsha256sums = {sha['license']}

pkgname = orchard-commander-bin
"""
    return {"PKGBUILD": pkgbuild, ".SRCINFO": srcinfo}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("version")
    parser.add_argument("--out", type=Path, default=Path("dist/manifests"))
    args = parser.parse_args()
    v = args.version
    raw = f"https://raw.githubusercontent.com/{REPO}/v{v}"
    sources = {key: url(v, name) for key, name in ASSETS.items()}
    sources.update(desktop=f"{raw}/packaging/linux/orchard-commander.desktop",
                   icon=f"{raw}/packaging/linux/orchard-commander.png", license=f"{raw}/LICENSE")
    sha = {}
    for key, address in sources.items():
        sha[key] = sha256_of(address)
        print(f"{sha[key]}  {address}")

    files = {"homebrew/Casks/orchard-commander.rb": homebrew(v, sha)}
    files |= {f"winget/manifests/s/Sandboxly/OrchardCommander/{v}/{n}": t for n, t in winget(v, sha).items()}
    files |= {f"chocolatey/{n}": t for n, t in chocolatey(v, sha).items()}
    files |= {f"aur/{n}": t for n, t in aur(v, sha).items()}
    for name, text in files.items():
        path = args.out / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8", newline="\n")
        print("wrote", path)
    return 0


if __name__ == "__main__":
    sys.exit(main())
