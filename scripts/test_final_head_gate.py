#!/usr/bin/env python3
"""Focused runner tests: no Rust builds, npm installs, or gate receipts."""
import importlib.machinery
import importlib.util
import pathlib
import json
import time
import subprocess
import tempfile
import unittest
from unittest.mock import patch

loader = importlib.machinery.SourceFileLoader('final_head_gate', str(pathlib.Path(__file__).resolve().parents[1] / 'bin/check'))
spec = importlib.util.spec_from_loader(loader.name, loader)
gate = importlib.util.module_from_spec(spec)
loader.exec_module(gate)


class GateTests(unittest.TestCase):
    def test_real_draft_commands_without_workspace_suite_or_skips(self):
        commands = dict(gate.COMMANDS)
        self.assertEqual(len(commands), len(gate.COMMANDS))
        self.assertTrue(all('--skip' not in command for _, command in gate.COMMANDS))
        self.assertFalse(any('test' in command and '--workspace' in command for _, command in gate.COMMANDS))

    def test_exact_non_test_commands(self):
        self.assertEqual([(label, argv) for label, argv in gate.COMMANDS
                          if argv[:2] != ['cargo', 'test'] and argv[:5] != ['npm', '--prefix', 'web', 'run', 'test']], [
            ('gate-tests', ['python3', '-B', 'scripts/test_final_head_gate.py']),
            ('fmt', ['cargo', 'fmt', '--all', '--', '--check']),
            ('check', ['cargo', 'check', '--locked', '--workspace']),
            ('clippy', ['cargo', 'clippy', '--locked', '--workspace', '--all-targets', '--', '-D', 'warnings']),
            ('deny', ['cargo', 'deny', 'check']),
            ('moq-check', ['cargo', 'check', '--locked', '-p', 'multiview-cli', '--features', 'moq-output,gpl-codecs']),
            ('moq-clippy', ['cargo', 'clippy', '--locked', '-p', 'multiview-cli', '--features', 'moq-output,gpl-codecs', '--all-targets', '--', '-D', 'warnings']),
            ('web-install', ['npm', '--prefix', 'web', 'ci']),
            ('web-lint', ['npm', '--prefix', 'web', 'run', 'lint']),
            ('web-build', ['npm', '--prefix', 'web', 'run', 'build']),
            ('probe-syntax', ['node', '--check', 'web/scripts/moq-cloudflare-probe.mjs']),
        ])

    def test_exact_rust_test_commands(self):
        self.assertEqual([(label, argv) for label, argv in gate.COMMANDS if argv[:2] == ['cargo', 'test']], [
            ('moq-test', ['cargo', 'test', '--locked', '-p', 'multiview-moq', '--lib']),
            ('moq-outputs-test', ['cargo', 'test', '--locked', '-p', 'multiview-cli', '--features', 'moq-output', '--lib', 'moq_outputs::']),
            ('outgoing-including-handshake-test', ['cargo', 'test', '--locked', '-p', 'multiview-cli', '--features', 'moq-output', '--lib', 'outgoing_']),
            ('handshake-test', ['cargo', 'test', '--locked', '-p', 'multiview-cli', '--features', 'moq-output', '--lib', 'pending_outgoing_handshake']),
            ('relay-url-test', ['cargo', 'test', '--locked', '-p', 'multiview-cli', '--features', 'moq-output', '--lib', 'relay_url_file']),
            ('authority-bridge-unit-test', ['cargo', 'test', '--locked', '-p', 'multiview-control', '--features', 'moq-transport', '--example', 'abr_authority']),
            ('native-h264-http-test', ['cargo', 'test', '--locked', '-p', 'multiview-cli', '--features', 'moq-output,gpl-codecs', '--lib', 'actual_h264_pipeline_http', '--', '--test-threads=1']),
        ])

    def test_exact_vitest_path_tuples(self):
        self.assertEqual([(label, argv) for label, argv in gate.COMMANDS if argv[:5] == ['npm', '--prefix', 'web', 'run', 'test']], [
            ('web-moq-abr', ['npm', '--prefix', 'web', 'run', 'test', '--', 'src/preview/moqAbr.test.ts', 'src/preview/MoqPlayerAbr.test.tsx', '--maxWorkers=2']),
            ('web-preview', ['npm', '--prefix', 'web', 'run', 'test', '--', 'src/preview/moqConnection.test.ts', 'src/preview/moqPlayback.test.ts', 'src/preview/moqLeaseApi.test.ts', 'src/preview/MoqSelection.test.tsx', 'src/preview/PreviewSurface.test.tsx', 'src/preview/capabilities.test.ts', 'src/preview/outputPlayback.test.ts', 'src/preview/diagnostics.test.ts', 'src/preview/PreviewDiagnostics.test.tsx', '--maxWorkers=2']),
        ])
        for _, argv in gate.COMMANDS:
            for token in argv:
                if token.endswith(('.test.ts', '.test.tsx')):
                    self.assertTrue((gate.ROOT / 'web' / token).is_file(), token)

    def test_missing_openssl_fails_preflight_before_commands(self):
        for args in (['bin/check'], ['bin/check', '--validate-only']):
            with self.subTest(args=args), patch.object(gate.sys, 'argv', args), patch.object(gate, 'validate', return_value='head'), patch.object(gate, 'environment', return_value={'PATH': '/missing'}), patch.object(gate.shutil, 'which', return_value=None) as which, patch.object(gate, 'execute') as execute, patch.object(gate, 'resources') as resources:
                with self.assertRaisesRegex(RuntimeError, '^required native prerequisite unavailable: openssl executable on PATH$'):
                    gate.main()
                which.assert_called_once_with('openssl', path='/missing')
                execute.assert_not_called()
                resources.assert_not_called()

    def test_available_openssl_passes_preflight(self):
        with patch.object(gate.shutil, 'which', return_value='/usr/bin/openssl'):
            gate.prerequisites({'PATH': '/usr/bin'})

    def test_dirty_tree_rejected(self):
        with patch.object(gate, 'git', return_value=' M bin/check'):
            with self.assertRaisesRegex(RuntimeError, 'clean'):
                gate.validate()

    def test_changed_head_rejected(self):
        with patch.object(gate, 'git', side_effect=['', 'new']):
            with self.assertRaisesRegex(RuntimeError, 'HEAD changed'):
                gate.validate('old')

    def test_merge_head_accepted_without_historical_chain(self):
        with patch.object(gate, 'git', side_effect=['', 'merge-head']):
            self.assertEqual(gate.validate(), 'merge-head')

    def test_environment_bounded_and_target_override_rejected(self):
        with patch.dict(gate.os.environ, {}, clear=True):
            env = gate.environment()
            self.assertEqual(env['CARGO_BUILD_JOBS'], '2')
            self.assertEqual(env['CARGO_INCREMENTAL'], '0')
        for name in ('CARGO_TARGET_DIR', 'CARGO_BUILD_TARGET_DIR'):
            with patch.dict(gate.os.environ, {name: '/other-lane'}, clear=True):
                with self.assertRaises(RuntimeError):
                    gate.environment()

    def test_timeout_budgets_are_proportional(self):
        for label in ('gate-tests', 'fmt', 'deny', 'moq-test', 'moq-outputs-test',
                      'outgoing-including-handshake-test', 'handshake-test', 'relay-url-test',
                      'authority-bridge-unit-test', 'native-h264-http-test', 'web-install',
                      'web-moq-abr', 'web-preview', 'web-lint', 'web-build', 'probe-syntax'):
            self.assertEqual(gate.command_timeout(label), 600)
        for label in ('check', 'clippy', 'moq-check', 'moq-clippy'):
            self.assertEqual(gate.command_timeout(label), 1200)
        self.assertTrue(all(gate.command_timeout(label) > 0 for label, _ in gate.COMMANDS))

    def test_no_historical_evidence_or_attempt_ledger(self):
        with tempfile.TemporaryDirectory(dir=gate.ROOT / 'target') as tmp:
            directory = pathlib.Path(tmp)
            with patch.object(gate, 'COMMANDS', []), patch.object(gate, 'validate'):
                gate.execute(directory, 'head', {})
            self.assertEqual(list(directory.iterdir()), [])

    def test_real_timeout_kills_owned_descendant_and_records_failure(self):
        with tempfile.TemporaryDirectory(dir=gate.ROOT / 'target') as tmp:
            directory = pathlib.Path(tmp)
            pidfile = directory / 'child.pid'
            code = ("import subprocess,time; p=subprocess.Popen(['python3','-c',"
                    "'import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)']); "
                    f"open({str(pidfile)!r}, 'w').write(str(p.pid)); time.sleep(60)")
            with patch.object(gate, 'COMMANDS', [('sleep', ['python3', '-c', code])]), patch.object(gate, 'validate'), patch.object(gate, 'resources', return_value=0), patch.object(gate, 'target_bytes', return_value=0), patch.object(gate, 'command_timeout', return_value=0.3):
                with self.assertRaisesRegex(RuntimeError, 'wall timeout'):
                    gate.execute(directory, 'head', gate.os.environ.copy())
            pid = int(pidfile.read_text())
            # A killed child can briefly remain a zombie until its reaper runs.
            stat = pathlib.Path(f'/proc/{pid}/stat')

            def gone_or_zombie():
                # The process can be reaped between an existence check and the
                # read. A vanished entry is the "gone" outcome, not an error.
                try:
                    return stat.read_text().split()[2] == 'Z'
                except (FileNotFoundError, ProcessLookupError):
                    return True

            deadline = time.monotonic() + 1
            while not gone_or_zombie() and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertTrue(gone_or_zombie())
            record = json.loads((directory / 'commands.jsonl').read_text())
            self.assertEqual(record['status'], 'failed')
            self.assertIn('wall timeout', record['error'])

    def test_low_disk_fails_closed(self):
        with patch.object(gate.shutil, 'disk_usage') as usage:
            usage.return_value.free = gate.MIN_FREE - 1
            with self.assertRaisesRegex(RuntimeError, 'floor'):
                gate.resources()

    def test_disk_floor_value_and_message(self):
        self.assertEqual(gate.MIN_FREE, 150 * gate.GIB)
        with patch.object(gate.shutil, 'disk_usage') as usage:
            usage.return_value.free = 149 * gate.GIB
            with self.assertRaisesRegex(RuntimeError, 'below 150 GiB floor'):
                gate.resources()

    def test_large_target_fails_closed(self):
        with tempfile.TemporaryDirectory(dir=gate.ROOT / 'target') as tmp:
            with patch.object(gate.shutil, 'disk_usage') as usage, patch.object(gate, 'target_bytes', return_value=gate.MAX_TARGET + 1):
                usage.return_value.free = gate.MIN_FREE
                with self.assertRaisesRegex(RuntimeError, '15 GiB'):
                    gate.resources()

    def test_target_walk_disappearing_file_is_the_only_ignored_error(self):
        with tempfile.TemporaryDirectory(dir=gate.ROOT / 'target') as tmp:
            root = pathlib.Path(tmp)
            vanished = root / 'temporary.rmeta'
            vanished.write_bytes(b'x' * 100)
            survivor = root / 'survivor'
            survivor.write_bytes(b'y' * 200)
            real_stat = gate.os.stat

            def racing_stat(path, **kwargs):
                if path == vanished.name:
                    vanished.unlink()
                return real_stat(path, **kwargs)

            with patch.object(gate, 'target_directories', return_value=[root]), patch.object(gate.os, 'stat', side_effect=racing_stat):
                measured = gate.target_bytes()
            self.assertGreaterEqual(measured, 200)
            self.assertFalse(vanished.exists())
            for error in (PermissionError('denied'), OSError('I/O failure')):
                with patch.object(gate, 'target_directories', return_value=[root]), patch.object(gate.os, 'stat', side_effect=error):
                    with self.assertRaises(type(error)):
                        gate.target_bytes()

    def test_target_walk_counts_nested_sparse_files_and_does_not_follow_links(self):
        with tempfile.TemporaryDirectory(dir=gate.ROOT / 'target') as tmp:
            root = pathlib.Path(tmp)
            nested = root / 'nested'
            nested.mkdir()
            sparse = nested / 'sparse'
            with sparse.open('wb') as output:
                output.truncate(gate.MAX_TARGET + 1)
            (root / 'link').symlink_to(root, target_is_directory=True)
            with patch.object(gate, 'target_directories', return_value=[root]):
                self.assertGreater(gate.target_bytes(), gate.MAX_TARGET)
            with patch.object(gate, 'target_directories', return_value=[root / 'link']):
                with self.assertRaises(OSError):
                    gate.target_bytes()

    def test_sampled_peak_never_shrinks_after_transient_growth(self):
        with tempfile.TemporaryDirectory(dir=gate.ROOT / 'target') as tmp:
            log = pathlib.Path(tmp) / 'sample.log'
            with patch.object(gate, 'resources', side_effect=[10, 100, 20]), patch.object(gate, 'target_bytes', return_value=15), patch.object(gate.subprocess, 'Popen') as start, patch.object(gate, 'stop_process_group'):
                start.return_value.wait.side_effect = [subprocess.TimeoutExpired('sample', 5), 0]
                gate.run_command(['sample'], log, {})
            self.assertIn('sampled_peak_bytes=100', log.read_text())
            self.assertIn('growth_bytes=5', log.read_text())

    def test_nonzero_command_fails_with_real_process(self):
        with tempfile.TemporaryDirectory(dir=gate.ROOT / 'target') as tmp:
            with patch.object(gate, 'resources', return_value=0):
                with self.assertRaisesRegex(RuntimeError, 'exited 7'):
                    gate.run_command(['python3', '-c', 'raise SystemExit(7)'], pathlib.Path(tmp) / 'failure.log', gate.os.environ.copy())

    def test_required_test_execution_rejects_zero_tests(self):
        with tempfile.TemporaryDirectory(dir=gate.ROOT / 'target') as tmp:
            log = pathlib.Path(tmp) / 'test.log'
            command = ['cargo', 'test', '--lib', 'absent']
            for text in ('', 'test result: ok. 0 passed; 0 failed; 20 filtered out;'):
                log.write_text(text)
                with self.assertRaisesRegex(RuntimeError, 'no passing Rust'):
                    gate.verify_test_execution(command, log)
            log.write_text('test result: ok. 1 passed; 0 failed; 20 filtered out;')
            gate.verify_test_execution(command, log)
            web = ['npm', '--prefix', 'web', 'run', 'test']
            for text in ('No test files found', ' Tests  0 passed (0)', ''):
                log.write_text(text)
                with self.assertRaisesRegex(RuntimeError, 'no passing browser'):
                    gate.verify_test_execution(web, log)
            log.write_text(' Tests  3 passed (3)')
            gate.verify_test_execution(web, log)

    def test_no_success_record_or_next_command_after_failure(self):
        with tempfile.TemporaryDirectory(dir=gate.ROOT / 'target') as tmp:
            directory = pathlib.Path(tmp)
            with patch.object(gate, 'COMMANDS', [('first', ['false']), ('next', ['true'])]), patch.object(gate, 'validate'), patch.object(gate, 'run_command', side_effect=RuntimeError('failure')) as run:
                with self.assertRaises(RuntimeError):
                    gate.execute(directory, 'head', {})
                self.assertEqual(run.call_count, 1)
                records = [json.loads(line) for line in (directory / 'commands.jsonl').read_text().splitlines()]
                self.assertEqual([record['status'] for record in records], ['failed'])

    def test_head_change_after_command_not_recorded_as_success(self):
        with tempfile.TemporaryDirectory(dir=gate.ROOT / 'target') as tmp:
            directory = pathlib.Path(tmp)
            with patch.object(gate, 'COMMANDS', [('first', ['true'])]), patch.object(gate, 'validate', side_effect=['head', RuntimeError('HEAD changed')]), patch.object(gate, 'run_command'):
                with self.assertRaises(RuntimeError):
                    gate.execute(directory, 'head', {})
                records = [json.loads(line) for line in (directory / 'commands.jsonl').read_text().splitlines()]
                self.assertEqual([record['status'] for record in records], ['failed'])


if __name__ == '__main__':
    unittest.main()
