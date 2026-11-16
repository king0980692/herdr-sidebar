#!/usr/bin/env python3
"""Hermetic installer regression tests: fake mise, no network or real config."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().with_name("install-mise.sh")
SHELL = shutil.which("sh")
PATTERN = ('herdr-sidebar-{{ arch(x64="x86_64", arm64="aarch64") }}-'
           '{{ os(linux="unknown-linux-musl", macos="apple-darwin", '
           'windows="pc-windows-msvc") }}.tar.gz')
FAKE = r'''
import json, os, pathlib, sys
home = pathlib.Path(os.environ["HOME"])
args = sys.argv[1:]
with (home / "calls.jsonl").open("a") as log:
    log.write(json.dumps({"args": args, "cwd": os.getcwd()}) + "\n")
assert args[:2] == ["--cd", str(home)], args
assert os.getcwd() == str(home)
assert not any(a in args for a in ["--yes", "-y", "trust", "exec", "install"])
command = args[2]
if command == os.environ.get("FAIL_COMMAND"):
    sys.exit(37)
if command == "use":
    if "--dry-run" not in args:
        config = home / "global-tools.json"
        tools = json.loads(config.read_text())
        tools["github:king0980692/herdr-sidebar"] = args[-1]
        config.write_text(json.dumps(tools))
elif command == "where":
    print(home / "resolved installation")
else:
    sys.exit(99)
'''


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="sidebar-mise-test-")
        self.addCleanup(self.temp.cleanup)
        # macOS /var aliases /private/var; compare canonical cwd paths.
        self.root = Path(self.temp.name).resolve()
        self.home = self.root / "home with spaces"
        self.home.mkdir()
        self.project = self.root / "untrusted project"
        self.project.mkdir()
        (self.project / "mise.toml").write_text('[hooks]\nenter = "exit 99"\n')
        self.bin = self.root / "path-bin"
        self.bin.mkdir()
        self.config = self.home / "global-tools.json"
        self.config.write_text(json.dumps({"node": "22.0.0", "python": "3.13.0"}))
        # Deliberately do not inherit operator mise/config/auth environment.
        self.env = {"HOME": str(self.home), "PATH": str(self.bin)}
        self.fake(self.bin / "mise")

    def fake(self, path):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("#!" + sys.executable + "\n" + FAKE)
        path.chmod(0o755)

    def run_script(self, *args):
        return subprocess.run([SHELL, str(SCRIPT), *args], cwd=self.project,
                              env=self.env, text=True, capture_output=True)

    def calls(self):
        path = self.home / "calls.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def expected_use(self, version="0.16.4"):
        return ["--cd", str(self.home), "use", "--global", "--pin",
                "--minimum-release-age", "0", "--tool-option",
                "asset_pattern=" + PATTERN, "--tool-option", "bin_path=bin",
                "github:king0980692/herdr-sidebar@" + version]

    def test_default_and_literal_options(self):
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        self.assertEqual(calls[0]["args"], self.expected_use())
        self.assertEqual(calls[1]["args"], ["--cd", str(self.home), "where",
                                          "github:king0980692/herdr-sidebar@0.16.4"])
        self.assertEqual(result.stdout.strip(), str(self.home / "resolved installation/bin/herdr-sidebar"))
        tools = json.loads(self.config.read_text())
        self.assertEqual(tools["node"], "22.0.0")
        self.assertEqual(tools["python"], "3.13.0")
        self.assertEqual(len(calls), 2)

    def test_explicit_version(self):
        result = self.run_script("1.20.300")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls()[0]["args"], self.expected_use("1.20.300"))

    def test_dry_run_same_argv_without_mutation(self):
        before = self.config.read_bytes()
        result = self.run_script("--dry-run", "0.16.4")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([call["args"] for call in self.calls()],
                         [self.expected_use() + ["--dry-run"]])
        self.assertEqual(self.config.read_bytes(), before)

    def test_reject_version_injection_before_mise(self):
        for version in ["", "latest", "v0.16.4", "0.16", "0.16.4.4", "0..3",
                        "0.16.4;touch injected", "$(touch injected)", "--yes",
                        "0.16.4\n1.2.3", "0.16.4@latest", "0.16.4-beta"]:
            with self.subTest(version=version):
                self.assertEqual(self.run_script(version).returncode, 2)
        self.assertEqual(self.run_script("0.16.4", "1.2.3").returncode, 2)
        self.assertEqual(self.calls(), [])
        self.assertFalse((self.project / "injected").exists())

    def test_errors_propagate(self):
        self.env["FAIL_COMMAND"] = "use"
        self.assertEqual(self.run_script().returncode, 37)
        self.assertEqual(len(self.calls()), 1)
        self.assertEqual(self.run_script("--dry-run").returncode, 37)

    def test_where_error_propagates(self):
        self.env["FAIL_COMMAND"] = "where"
        result = self.run_script()
        self.assertEqual(result.returncode, 37)
        self.assertEqual(result.stdout, "")

    def test_standard_locations_and_priority(self):
        # Only temp-owned files are removed; operator installations are unreachable.
        (self.bin / "mise").unlink()
        local = self.home / ".local/bin/mise"
        shared = self.home / ".local/share/mise/bin/mise"
        self.fake(local)
        self.fake(shared)
        shared.write_text("#!/bin/sh\nexit 88\n")
        self.assertEqual(self.run_script("--dry-run").returncode, 0)
        local.unlink()
        self.fake(shared)
        self.assertEqual(self.run_script("--dry-run").returncode, 0)

    def test_missing_mise_instructions(self):
        (self.bin / "mise").unlink()
        result = self.run_script()
        self.assertEqual(result.returncode, 127)
        self.assertIn("Install mise yourself", result.stderr)
        self.assertEqual(self.calls(), [])

    def test_help_does_not_call_mise(self):
        self.assertEqual(self.run_script("--help").returncode, 0)
        self.assertEqual(self.calls(), [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
