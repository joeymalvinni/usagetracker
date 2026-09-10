#!/usr/bin/env python3
"""Exercise installer trust decisions with synthetic archives and stubbed macOS tools.

No downloads, credentials, running apps, or real installation directories are used.
"""
import hashlib
import json
import os
from pathlib import Path
import plistlib
import subprocess
import tarfile
import tempfile
import unittest
import zipfile

REPO = Path(__file__).resolve().parent.parent
STUB = r'''#!/usr/bin/env python3
import os, pathlib, shutil, sys
command = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
mode = os.environ["TEST_MODE"]
if command == "uname":
    print("Darwin" if args == ["-s"] else "arm64")
elif command == "sw_vers":
    print("14.0")
elif command == "curl":
    shutil.copyfile(pathlib.Path(os.environ["TEST_ASSETS"]) / args[-1].split("/")[-1],
                    args[args.index("--output") + 1])
elif command == "codesign":
    path = pathlib.Path(args[-1])
    if "-dv" in args:
        suffix = "" if path.suffix == ".app" else ".daemon" if path.name == "usage-daemon" else ".cli"
        print("Identifier=app.usagetracker" + suffix)
        if mode == "adhoc":
            print("Signature=adhoc\nTeamIdentifier=not set")
        else:
            mismatch = (mode == "mixed_teams" and suffix == ".daemon") or (mode == "mixed_cli" and suffix == ".cli")
            team = "OTHER" if mismatch else "EXAMPLE"
            authority = "Local Certificate" if mode == "untrusted" else "Developer ID Application: Example"
            print("Authority=" + authority + "\nTeamIdentifier=" + team)
    elif mode == "damaged":
        sys.exit(1)
    elif "-R" in args and mode == "untrusted_anchor":
        sys.exit(1)
elif command == "spctl":
    sys.exit(1 if mode == "notarization_failed" else 0)
elif command == "pgrep":
    sys.exit(1)
elif command == "launchctl":
    pass
else:
    raise SystemExit("Unexpected command: " + command)
'''


@unittest.skipUnless(os.uname().sysname == "Darwin", "installer uses macOS ditto and PlistBuddy")
class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="usagetracker-distribution-test-")
        self.root = Path(self.temporary.name)
        self.addCleanup(self.temporary.cleanup)
        self.assets = self.root / "assets"
        self.assets.mkdir()
        self.stub_dir = self.root / "stubs"
        self.stub_dir.mkdir()
        for command in ["uname", "sw_vers", "curl", "codesign", "spctl", "pgrep", "launchctl"]:
            path = self.stub_dir / command
            path.write_text(STUB.replace("#!/usr/bin/env python3", "#!" + os.sys.executable, 1))
            path.chmod(0o755)
        app = self.root / "source" / "UsageTracker.app"
        (app / "Contents/MacOS").mkdir(parents=True)
        (app / "Contents/Info.plist").write_bytes(plistlib.dumps({"CFBundleIdentifier": "app.usagetracker"}))
        for name in ["UsageMenuBar", "usage-daemon"]:
            binary = app / "Contents/MacOS" / name
            binary.write_text("#!/bin/sh\nexit 0\n")
            binary.chmod(0o755)
        with zipfile.ZipFile(self.assets / "UsageTracker-macos-arm64.zip", "w") as archive:
            for path in app.rglob("*"):
                archive.write(path, path.relative_to(app.parent))
        cli = self.root / "source" / "usage"
        cli.write_text("#!/bin/sh\nexit 0\n")
        cli.chmod(0o755)
        with tarfile.open(self.assets / "usage-macos-arm64.tar.gz", "w:gz") as archive:
            archive.add(cli, arcname="usage")
        self.write_checksums()

    def write_checksums(self):
        (self.assets / "SHA256SUMS").write_text("".join(
            f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n"
            for path in sorted(self.assets.iterdir()) if path.name != "SHA256SUMS"
        ))

    def install(self, mode, app=True):
        env = dict(os.environ, PATH=f"{self.stub_dir}:{os.environ['PATH']}",
                   TEST_MODE=mode, TEST_ASSETS=str(self.assets),
                   USAGE_TRACKER_HOME=str(self.root / "data"),
                   USAGE_TRACKER_SOCKET=str(self.root / "data/usage.sock"),
                   USAGETRACKER_UPDATE_STATUS_FILE="", NO_COLOR="1")
        selection = [] if app is None else ["--app-only" if app else "--cli-only"]
        return subprocess.run(["bash", str(REPO / "scripts/install.sh"), "--no-launch", *selection,
                               "--app-dir", str(self.root / "apps"),
                               "--bin-dir", str(self.root / "bin")],
                              env=env, text=True, capture_output=True, timeout=30)

    def test_legacy_and_developer_id_apps_install(self):
        for mode in ["adhoc", "developer_id"]:
            with self.subTest(mode=mode):
                result = self.install(mode)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertTrue((self.root / "apps/UsageTracker.app").is_dir())

    def test_legacy_and_developer_id_cli_install(self):
        for mode in ["adhoc", "developer_id"]:
            with self.subTest(mode=mode):
                result = self.install(mode, app=False)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertTrue((self.root / "bin/usage").is_file())

    def test_default_install_accepts_matching_signed_app_and_cli(self):
        result = self.install("developer_id", app=None)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue((self.root / "apps/UsageTracker.app").is_dir())
        self.assertTrue((self.root / "bin/usage").is_file())

    def test_rejected_artifacts_preserve_existing_installation(self):
        destination = self.root / "apps/UsageTracker.app"
        destination.mkdir(parents=True)
        marker = destination / "keep-me"
        marker.write_text("previous installation")
        for mode in ["untrusted", "untrusted_anchor", "damaged", "notarization_failed", "mixed_teams", "mixed_cli"]:
            with self.subTest(mode=mode):
                result = self.install(mode, app=None)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(marker.read_text(), "previous installation")
                self.assertFalse((destination / "Contents").exists())

    def test_checksum_mismatch_never_installs(self):
        (self.assets / "UsageTracker-macos-arm64.zip").write_bytes(b"changed archive")
        result = self.install("developer_id")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Checksum verification failed", result.stderr)
        self.assertFalse((self.root / "apps").exists())


class PackagingTests(unittest.TestCase):
    def packaged_release(self, status):
        temporary = tempfile.TemporaryDirectory(prefix="usagetracker-packaging-test-")
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        (root / "scripts").mkdir()
        (root / "Cargo.toml").write_text('[workspace.package]\nversion = "1.0.0"\n')
        script = root / "scripts/package-release.sh"
        script.write_text((REPO / "scripts/package-release.sh").read_text())
        builder = root / "apps/UsageMenuBar/package-dev-app.sh"
        builder.parent.mkdir(parents=True)
        builder.write_text("#!" + os.sys.executable + '''
import os, pathlib, plistlib
app = pathlib.Path(os.environ["APP_OUTPUT_PATH"])
(app / "Contents/MacOS").mkdir(parents=True)
(app / "Contents/Info.plist").write_bytes(plistlib.dumps({"CFBundleIdentifier": "app.usagetracker"}))
(app / "Contents/MacOS/UsageMenuBar").write_text("synthetic app")
assert os.environ["CODESIGN_IDENTITY"] == "Developer ID Application: Example"
''')
        builder.chmod(0o755)
        stub = root / "stubs"
        stub.mkdir()
        for command in ["cargo", "rustup", "codesign", "xcrun", "spctl"]:
            path = stub / command
            path.write_text("#!" + os.sys.executable + r'''
import json, os, pathlib, sys
command = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
with open(os.environ["TEST_COMMANDS"], "a") as log:
    log.write(json.dumps([command] + args) + "\n")
if command == "cargo":
    output = pathlib.Path(os.environ["CARGO_TARGET_DIR"]) / "aarch64-apple-darwin/release/usage-cli"
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text("synthetic cli")
elif command == "xcrun" and args[:2] == ["notarytool", "submit"]:
    print(json.dumps({"id": "test-submission", "status": os.environ["TEST_NOTARY_STATUS"]}))
elif command == "codesign" and "-dv" in args:
    print("Identifier=app.usagetracker")
''')
            path.chmod(0o755)
        output = root / "dist"
        output.mkdir()
        prior = output / "UsageTracker-macos-arm64.zip"
        prior.write_bytes(b"previous artifact")
        env = dict(os.environ, PATH=f"{stub}:{os.environ['PATH']}",
                   CODESIGN_IDENTITY="Developer ID Application: Example",
                   NOTARYTOOL_PROFILE="test-profile", NOTARYTOOL_KEYCHAIN=str(root / "keychain"),
                   TEST_NOTARY_STATUS=status, TEST_COMMANDS=str(root / "commands.jsonl"))
        result = subprocess.run(["bash", str(script), "aarch64-apple-darwin", str(output)],
                                env=env, text=True, capture_output=True, timeout=30)
        commands = [json.loads(line) for line in (root / "commands.jsonl").read_text().splitlines()]
        return result, prior, commands

    @unittest.skipUnless(os.uname().sysname == "Darwin", "packaging uses macOS tools")
    def test_signed_archive_is_stapled_only_after_notarization_acceptance(self):
        result, archive, commands = self.packaged_release("Accepted")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(zipfile.is_zipfile(archive))
        sign = next(command for command in commands if command[:2] == ["codesign", "--force"])
        self.assertIn("Developer ID Application: Example", sign)
        self.assertIn("--timestamp", sign)
        self.assertIn("runtime", sign)
        submit = next(i for i, command in enumerate(commands) if command[:3] == ["xcrun", "notarytool", "submit"])
        staple = next(i for i, command in enumerate(commands) if command[:3] == ["xcrun", "stapler", "staple"])
        self.assertLess(submit, staple)
        self.assertIn("test-profile", commands[submit])

    @unittest.skipUnless(os.uname().sysname == "Darwin", "packaging uses macOS tools")
    def test_rejected_notarization_preserves_previous_artifacts(self):
        result, archive, commands = self.packaged_release("Invalid")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Notarization was not accepted", result.stderr)
        self.assertEqual(archive.read_bytes(), b"previous artifact")
        self.assertFalse(any(command[:2] == ["xcrun", "stapler"] for command in commands))

    def test_signed_release_requires_notarization_before_building(self):
        env = dict(os.environ, CODESIGN_IDENTITY="Developer ID Application: Example")
        env.pop("NOTARYTOOL_PROFILE", None)
        result = subprocess.run(["bash", str(REPO / "scripts/package-release.sh"),
                                 "aarch64-apple-darwin", "/unused"], env=env,
                                text=True, capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 2)
        self.assertIn("require NOTARYTOOL_PROFILE", result.stderr)

    def test_adhoc_release_cannot_request_notarization(self):
        env = dict(os.environ, CODESIGN_IDENTITY="-", NOTARYTOOL_PROFILE="example")
        result = subprocess.run(["bash", str(REPO / "scripts/package-release.sh"),
                                 "aarch64-apple-darwin", "/unused"], env=env,
                                text=True, capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 2)
        self.assertIn("requires a Developer ID", result.stderr)


if __name__ == "__main__":
    unittest.main()
