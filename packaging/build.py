"""Build the standalone app: "dist/Orchard Commander.app" (macOS) or dist/orchard-commander/ (Windows, Linux).

    uv run python packaging/build.py [--sign IDENTITY | --adhoc] [--dmg [--notarize] [--copy-to DIR]]

On macOS the app is signed with a stable identity when one is available
(FCMD_CODESIGN_IDENTITY, else the first "Developer ID Application", else
"Apple Development" certificate in the keychain). Ad-hoc signatures change
with every build, and macOS then forgets the app's Local Network permission
and Keychain access each time.

macOS releases: --dmg packs the app into a signed disk image (the app plus an
Applications link to drag it onto); --notarize sends it to Apple's notary
service, waits, and staples the ticket, so Gatekeeper opens it without
warnings on other Macs. Notarizing uses credentials stored once with

    xcrun notarytool store-credentials orchard-commander \
        --apple-id YOU@EXAMPLE.COM --team-id TEAMID

(an app-specific password from account.apple.com), named by --profile or
FCMD_NOTARY_PROFILE (default "orchard-commander").
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def find_signing_identity() -> str | None:
    """The best code-signing identity in the keychain, by certificate type."""
    out = subprocess.run(
        ["security", "find-identity", "-v", "-p", "codesigning"], capture_output=True, text=True
    ).stdout
    names = re.findall(r'"([^"]+)"', out)
    for prefix in ("Developer ID Application:", "Apple Development:"):
        for name in names:
            if name.startswith(prefix):
                return name
    return None


def run(cmd: list[str], **kwargs) -> subprocess.CompletedProcess:
    print("$", " ".join(cmd))
    return subprocess.run(cmd, check=True, **kwargs)


def build_dmg(app: Path, identity: str | None) -> Path:
    """dist/Orchard-Commander-<version>.dmg with the app and a link to /Applications; signed."""
    sys.path.insert(0, str(ROOT / "src"))
    from orchard_commander import APP_NAME, __version__

    dmg = ROOT / "dist" / f"{APP_NAME.replace(' ', '-')}-{__version__}.dmg"
    staging = ROOT / "build" / "dmg"
    shutil.rmtree(staging, ignore_errors=True)
    staging.mkdir(parents=True)
    run(["ditto", str(app), str(staging / app.name)])  # ditto keeps signatures, symlinks, attributes
    (staging / "Applications").symlink_to("/Applications")
    dmg.unlink(missing_ok=True)
    run(["hdiutil", "create", "-volname", APP_NAME, "-srcfolder", str(staging), "-ov", "-format", "UDZO",
         str(dmg)], stdout=subprocess.DEVNULL)
    if identity:
        run(["codesign", "--sign", identity, "--timestamp", str(dmg)])
    return dmg


def notarize(dmg: Path, profile: str) -> None:
    """Submit to Apple's notary service, wait for the verdict, staple the ticket."""
    result = subprocess.run(
        ["xcrun", "notarytool", "submit", str(dmg), "--keychain-profile", profile, "--wait",
         "--output-format", "json"], capture_output=True, text=True,
    )
    try:
        verdict = json.loads(result.stdout)
    except ValueError:
        raise SystemExit(f"notarytool failed:\n{result.stdout}{result.stderr}") from None
    print(f"Notarization: {verdict.get('status')} ({verdict.get('id')})")
    if verdict.get("status") != "Accepted":
        log = subprocess.run(["xcrun", "notarytool", "log", verdict.get("id", ""), "--keychain-profile", profile],
                             capture_output=True, text=True)
        raise SystemExit(f"Not notarized: {verdict.get('message')}\n{log.stdout}{log.stderr}")
    run(["xcrun", "stapler", "staple", str(dmg)])
    run(["xcrun", "stapler", "validate", str(dmg)])
    run(["spctl", "--assess", "--type", "open", "--context", "context:primary-signature", "-v", str(dmg)])


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--sign", metavar="IDENTITY", help="macOS code-signing identity to use")
    parser.add_argument("--adhoc", action="store_true", help="macOS: ad-hoc signature (permissions reset per build)")
    parser.add_argument("--dmg", action="store_true", help="macOS: also make a signed disk image")
    parser.add_argument("--notarize", action="store_true", help="macOS: notarize and staple the disk image")
    parser.add_argument("--profile", default=os.environ.get("FCMD_NOTARY_PROFILE", "orchard-commander"),
                        help="notarytool keychain profile (default: %(default)s)")
    parser.add_argument("--copy-to", metavar="DIR", type=Path, help="copy the disk image there")
    args = parser.parse_args()
    if args.notarize and (args.adhoc or sys.platform != "darwin"):
        parser.error("--notarize needs macOS and a Developer ID signature")
    if args.notarize:
        args.dmg = True

    env = dict(os.environ)
    if sys.platform == "darwin" and not args.adhoc:
        identity = args.sign or env.get("FCMD_CODESIGN_IDENTITY") or find_signing_identity()
        if identity:
            env["FCMD_CODESIGN_IDENTITY"] = identity
            print(f"Signing with: {identity}")
        else:
            print("No code-signing certificate found: ad-hoc signing. macOS will ask for Local Network "
                  "and Keychain access again after every build.")
    elif args.adhoc:
        env.pop("FCMD_CODESIGN_IDENTITY", None)

    for d in ("build", "dist"):
        shutil.rmtree(ROOT / d, ignore_errors=True)
    cmd = [sys.executable, "-m", "PyInstaller", "--noconfirm", "--log-level", "WARN",
           str(ROOT / "packaging" / "orchard-commander.spec")]
    result = subprocess.run(cmd, cwd=ROOT, env=env)
    if result.returncode != 0:
        return result.returncode
    out = ROOT / "dist" / ("Orchard Commander.app" if sys.platform == "darwin" else "orchard-commander")
    # Count each file once: macOS bundles contain many symlinks into Frameworks.
    size = sum(p.lstat().st_size for p in out.rglob("*") if p.is_file() and not p.is_symlink())
    print(f"Built {out.relative_to(ROOT)} ({size / 1024**2:.0f} MB)")
    if args.dmg and sys.platform == "darwin":
        identity = env.get("FCMD_CODESIGN_IDENTITY")
        if args.notarize and not (identity or "").startswith("Developer ID Application"):
            raise SystemExit("Notarizing needs a \"Developer ID Application\" certificate")
        check = subprocess.run([str(out / "Contents" / "MacOS" / "orchard-commander"), "--self-test"],
                               capture_output=True, text=True)
        if check.returncode != 0:
            raise SystemExit(f"Self-test of the app failed:\n{check.stdout}{check.stderr}")
        dmg = build_dmg(out, identity)
        if args.notarize:
            notarize(dmg, args.profile)
        print(f"Disk image: {dmg.relative_to(ROOT)} ({dmg.stat().st_size / 1024**2:.0f} MB)")
        if args.copy_to:
            target = args.copy_to.expanduser()
            target.mkdir(parents=True, exist_ok=True)
            shutil.copy2(dmg, target / dmg.name)
            print(f"Copied to {target / dmg.name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
