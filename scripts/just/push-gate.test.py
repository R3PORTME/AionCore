#!/usr/bin/env python3
"""Exercise the publication recipe against an isolated Git repository."""

import os
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SOURCE = Path(__file__).resolve().parents[2]
AIONRS_CRATES = (
    "aion-agent", "aion-compact", "aion-config", "aion-mcp",
    "aion-memory", "aion-process", "aion-protocol", "aion-providers",
    "aion-skills", "aion-tools", "aion-types",
)


class PushGateTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="aioncore-push-gate-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / "repo"
        self.bin = self.root / "bin"
        self.repo.mkdir()
        self.bin.mkdir()
        for relative in (
            "justfile",
            "scripts/just/cargo.sh",
            "scripts/just/cargo.ps1",
            "scripts/migration/check-immutability.sh",
            "scripts/migration/check-immutability.ps1",
        ):
            target = self.repo / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(SOURCE / relative, target)
        (self.repo / "crates/aionui-db/migrations").mkdir(parents=True)
        (self.repo / "fixture.rs").write_text("valid\n")
        (self.repo / "unrelated.rs").write_text("valid\n")
        (self.bin / "cargo").write_text(
            "#!/usr/bin/env bash\n"
            'printf "%s\\n" "$*" >> "$GATE_CARGO_LOG"\n'
            'case "$1" in\n'
            '  fix) printf "fixed\\n" >> fixture.rs ;;\n'
            '  clippy) grep -q clippy-drift fixture.rs && exit 22 ;;\n'
            '  fmt) grep -q format-drift fixture.rs unrelated.rs && exit 21 ;;\n'
            '  nextest) grep -q test-failure fixture.rs && exit 23 ;;\n'
            'esac\n'
            'exit 0\n'
        )
        (self.bin / "git").write_text(
            "#!/usr/bin/env bash\n"
            'if [[ "$1" == "push" ]]; then\n'
            '  printf "%s\\n" "$*" >> "$GATE_PUSH_LOG"\n'
            '  /usr/bin/git rev-parse HEAD > "$GATE_PUSH_SHA"\n'
            '  exit 0\n'
            'fi\n'
            'exec /usr/bin/git "$@"\n'
        )
        for executable in ("cargo", "git"):
            (self.bin / executable).chmod(0o755)
        self.env = os.environ.copy()
        self.env.update(
            PATH=f"{self.bin}:{self.env['PATH']}",
            GATE_CARGO_LOG=str(self.root / "cargo.log"),
            GATE_PUSH_LOG=str(self.root / "push.log"),
            GATE_PUSH_SHA=str(self.root / "push.sha"),
            GIT_AUTHOR_NAME="Push Gate Test",
            GIT_AUTHOR_EMAIL="push-gate@example.invalid",
            GIT_COMMITTER_NAME="Push Gate Test",
            GIT_COMMITTER_EMAIL="push-gate@example.invalid",
        )
        self.run_command("git", "init", "-q", "-b", "main")
        self.run_command("git", "add", "justfile", "scripts", "fixture.rs", "unrelated.rs")
        self.run_command("git", "commit", "-qm", "candidate")

    def run_command(self, *args):
        return subprocess.run(
            args,
            cwd=self.repo,
            env=self.env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

    def git(self, *args):
        result = self.run_command("git", *args)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def identity(self):
        return (
            self.git("rev-parse", "HEAD"),
            self.git("write-tree"),
            self.git("ls-files", "--stage"),
            self.git("diff", "--binary"),
            self.git("diff", "--cached", "--binary"),
            self.git("status", "--porcelain=v1"),
            (self.repo / "fixture.rs").read_bytes(),
            (self.repo / "unrelated.rs").read_bytes(),
        )

    def use_powershell_path(self):
        if not shutil.which("pwsh"):
            self.skipTest("PowerShell is unavailable")
        justfile = self.repo / "justfile"
        contents = justfile.read_text()
        for name, script in (
            ("cargo_script", "scripts/just/cargo.ps1"),
            ("migration_check_script", "scripts/migration/check-immutability.ps1"),
        ):
            lines = contents.splitlines(keepends=True)
            contents = "".join(
                f'{name} := "pwsh -NoProfile -File {script}"\n'
                if line.startswith(f"{name} :=") else line
                for line in lines
            )
        justfile.write_text(contents)
        self.run_command("git", "add", "justfile")
        self.run_command("git", "commit", "-qm", "select PowerShell wrappers")

    def assert_gate(self, marker=None, expect_success=False, staged=False):
        if marker:
            path = self.repo / ("unrelated.rs" if marker == "format-drift-unrelated" else "fixture.rs")
            path.write_text(marker.replace("-unrelated", "") + "\n")
        if staged:
            (self.repo / "staged.txt").write_text("existing staged change\n")
            self.run_command("git", "add", "staged.txt")
            (self.repo / "untracked.txt").write_text("existing untracked change\n")
        before = self.identity()
        result = self.run_command("just", "push", "-u", "origin", "candidate")
        self.assertEqual(result.returncode == 0, expect_success, result.stdout + result.stderr)
        self.assertEqual(self.identity(), before, result.stdout + result.stderr)
        push_log = self.root / "push.log"
        if expect_success:
            self.assertEqual(push_log.read_text(), "push -u origin candidate\n")
            self.assertEqual((self.root / "push.sha").read_text(), before[0])
            calls = (self.root / "cargo.log").read_text().splitlines()
            self.assertIn("clippy --locked --workspace -- -D warnings", calls)
            self.assertIn("fmt --all -- --check", calls)
            self.assertIn("nextest run --locked --workspace", calls)
            self.assertFalse(any(" fix " in f" {call} " for call in calls))
        else:
            self.assertFalse(push_log.exists(), result.stdout + result.stderr)

    def assert_locked_aionrs_wrapper(self, *command):
        aionrs = self.root / "aionrs"
        packages = []
        for crate in AIONRS_CRATES:
            manifest = aionrs / "crates" / crate / "Cargo.toml"
            manifest.parent.mkdir(parents=True)
            manifest.write_text("[package]\n")
            packages.append({"name": crate, "manifest_path": str(manifest)})
        metadata_path = self.root / "metadata.json"
        metadata_path.write_text(json.dumps({"packages": packages}))
        self.env.update(AIONRS=str(aionrs), GATE_METADATA_JSON=str(metadata_path))
        (self.repo / "Cargo.lock").write_text("existing lockfile\n")
        self.run_command("git", "add", "Cargo.lock")
        (self.repo / "Cargo.lock").write_text("existing dirty lockfile\n")
        (self.bin / "cargo").write_text(
            "#!/usr/bin/env bash\n"
            'printf "%s\\n" "$*" >> "$GATE_CARGO_LOG"\n'
            'if [[ " $* " == *" metadata "* ]]; then\n'
            '  cat "$GATE_METADATA_JSON"\n'
            'elif [[ " $* " == *" update "* ]]; then\n'
            '  printf "unexpected update\\n" >> Cargo.lock\n'
            'fi\n'
            'exit 0\n'
        )
        before = self.identity()
        result = self.run_command(*command, "clippy", "--locked", "--workspace", "--", "-D", "warnings")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.identity(), before, result.stdout + result.stderr)
        calls = (self.root / "cargo.log").read_text().splitlines()
        self.assertTrue(any("metadata --format-version 1 --locked" in call for call in calls), calls)
        self.assertFalse(any(" update " in f" {call} " for call in calls), calls)

    def test_shell_success(self):
        self.assert_gate(expect_success=True)

    def test_shell_format_drift_preserves_unrelated_changes(self):
        self.assert_gate("format-drift-unrelated", staged=True)

    def test_shell_clippy_drift(self):
        self.assert_gate("clippy-drift")

    def test_shell_test_failure(self):
        self.assert_gate("test-failure")

    def test_shell_migration_failure(self):
        (self.repo / "crates/aionui-db/migrations/001_first.sql").write_text("SELECT 1;\n")
        (self.repo / "crates/aionui-db/migrations/001_second.sql").write_text("SELECT 2;\n")
        self.assert_gate()

    def test_shell_locked_aionrs_preserves_dirty_lockfile(self):
        self.assert_locked_aionrs_wrapper("bash", "scripts/just/cargo.sh")

    def test_explicit_auto_fix_is_development_only(self):
        before = self.git("rev-parse", "HEAD")
        result = self.run_command("just", "lint-fix")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.git("rev-parse", "HEAD"), before)
        self.assertIn("fixed", (self.repo / "fixture.rs").read_text())
        self.assertFalse((self.root / "push.log").exists())

    def test_powershell_success(self):
        self.use_powershell_path()
        self.assert_gate(expect_success=True)

    def test_powershell_format_drift_preserves_unrelated_changes(self):
        self.use_powershell_path()
        self.assert_gate("format-drift-unrelated", staged=True)

    def test_powershell_clippy_drift(self):
        self.use_powershell_path()
        self.assert_gate("clippy-drift")

    def test_powershell_test_failure(self):
        self.use_powershell_path()
        self.assert_gate("test-failure")

    def test_powershell_migration_failure(self):
        self.use_powershell_path()
        (self.repo / "crates/aionui-db/migrations/001_first.sql").write_text("SELECT 1;\n")
        (self.repo / "crates/aionui-db/migrations/001_second.sql").write_text("SELECT 2;\n")
        self.assert_gate()

    def test_powershell_locked_aionrs_preserves_dirty_lockfile(self):
        if not shutil.which("pwsh"):
            self.skipTest("PowerShell is unavailable")
        self.assert_locked_aionrs_wrapper("pwsh", "-NoProfile", "-File", "scripts/just/cargo.ps1")


if __name__ == "__main__":
    unittest.main()
