"""Build the standalone app: "dist/Orchard Commander.app" (macOS) or dist/orchard-commander/ (Windows, Linux).

    uv run python packaging/build.py [--sign IDENTITY | --adhoc]

On macOS the app is signed with a stable identity when one is available
(FCMD_CODESIGN_IDENTITY, else the first "Developer ID Application", else
"Apple Development" certificate in the keychain). Ad-hoc signatures change
with every build, and macOS then forgets the app's Local Network permission
and Keychain access each time.
"""

from __future__ import annotations

import argparse
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


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--sign", metavar="IDENTITY", help="macOS code-signing identity to use")
    parser.add_argument("--adhoc", action="store_true", help="macOS: ad-hoc signature (permissions reset per build)")
    args = parser.parse_args()

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
    return 0


if __name__ == "__main__":
    sys.exit(main())
