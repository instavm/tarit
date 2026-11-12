"""Exercise the actual workflow validator without executing selected repository code."""
import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parents[2]


class KvmTrustTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.repo = Path(self.tmp.name)
        self.git('init', '-q')
        self.git('config', 'user.name', 'Test')
        self.git('config', 'user.email', 'test@example.invalid')
        self.git('commit', '-q', '--allow-empty', '-m', 'trusted')
        self.trusted = self.git('rev-parse', 'HEAD')
        self.git('update-ref', 'refs/remotes/origin/main', self.trusted)
        self.git('commit', '-q', '--allow-empty', '-m', 'unmerged')
        self.unmerged = self.git('rev-parse', 'HEAD')
        workflow = (ROOT / '.github/workflows/kvm.yml').read_text()
        self.validator = textwrap.dedent(workflow.split('        run: |\n', 1)[1])

    def git(self, *args):
        return subprocess.check_output(['git', *args], cwd=self.repo, text=True).strip()

    def validate(self, sha):
        summary = self.repo / 'summary'
        return subprocess.run(
            ['bash', '-c', self.validator], cwd=self.repo, text=True,
            capture_output=True,
            env={**os.environ, 'TRUSTED_SHA': sha, 'GITHUB_STEP_SUMMARY': str(summary)},
        )

    def test_exact_main_sha_prepares_without_hardware_claim(self):
        result = self.validate(self.trusted)
        self.assertEqual(result.returncode, 0, result.stderr)
        summary = (self.repo / 'summary').read_text()
        self.assertIn(self.trusted, summary)
        self.assertIn('did not run KVM tests', summary)
        self.assertEqual(self.git('rev-parse', 'HEAD'), self.unmerged)

    def test_unmerged_sha_is_rejected(self):
        self.assertNotEqual(self.validate(self.unmerged).returncode, 0)

    def test_noncommit_hash_is_rejected(self):
        self.assertNotEqual(self.validate('0' * 40).returncode, 0)

    def test_mutable_refs_abbreviations_and_shell_input_are_rejected(self):
        for value in ['main', self.trusted[:12], self.trusted.upper(),
                      '$(touch injected)', self.trusted + '\n', '--help']:
            with self.subTest(value=value):
                self.assertNotEqual(self.validate(value).returncode, 0)
        self.assertFalse((self.repo / 'injected').exists())

    def test_retired_setup_refuses_without_registration(self):
        result = subprocess.run(['bash', str(ROOT / 'scripts/setup-kvm-runner.sh')],
                                cwd=self.repo, capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn('registration is disabled', result.stderr)

    def test_manual_runner_requires_explicit_disposable_ack(self):
        env = dict(os.environ)
        env.pop('TARIT_DISPOSABLE_KVM_ACK', None)
        result = subprocess.run(['bash', str(ROOT / 'scripts/run-trusted-kvm.sh'), self.trusted],
                                cwd=self.repo, env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn('acknowledge disposable host destruction', result.stderr)


if __name__ == '__main__':
    unittest.main()
