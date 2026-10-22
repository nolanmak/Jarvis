"""Synthetic validator tests; these fixtures are never live qualification evidence."""
import copy
import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest
import wave

SPEC = importlib.util.spec_from_file_location('phone_live', Path(__file__).parents[1] / 'phone-live-receipt.py')
live = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(live)


class ReceiptTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        calls = []
        for i, (host, arch) in enumerate([('linux', 'x86_64'), ('macos', 'arm64'), ('macos', 'x86_64')]):
            for provider in ['claude', 'codex']:
                audio = self.root / f'synthetic-{len(calls)}.wav'
                with wave.open(str(audio), 'wb') as stream:
                    stream.setparams((1, 2, 100, 0, 'NONE', 'not compressed'))
                    stream.writeframes(bytes([len(calls) + 1, 0]) * 6000)
                evidence = {'path': audio.name, 'sha256': hashlib.sha256(audio.read_bytes()).hexdigest()}
                calls.append({'host': host, 'architecture': arch, 'provider': provider,
                    'origin': ['discord', 'slack', 'whatsapp'][i], 'outcome': 'completed',
                    'voice': 'fixture', 'provider_settings': {'transport': 'conversationrelay'},
                    'sessions': ['native-fixture'] * 6, 'audio': dict(evidence),
                    'turns': [{'owner_end_ms': n * 6000 + 1000, 'response_start_ms': n * 6000 + 2000, 'used_tools': False} for n in range(4)],
                    'interruptions': [{'owner_start_ms': 30000, 'playback_stop_ms': 30500}],
                    'read_only_tool': {'session_id': 'native-fixture', 'completed': True},
                    'hangup_confirmed': True})
        self.receipt = {'version': 1, 'kind': 'phone-live', 'commit': 'a' * 40,
            'owner_audio_consent': True, 'measurement': {'method': 'recorded_audio', 'description': 'Synthetic annotated fixture for validator tests only'},
            'number_setup_verified': True, 'calls': calls,
            'approval': {'pending_observed': True, 'executed_before_resolution': False,
                'resolution': 'approved', 'executed_after_resolution': True, 'call_index': 0,
                'session_id': 'native-fixture'}}

    def test_complete_structure_and_timing(self):
        result = live.validate(self.receipt, self.root)
        self.assertEqual(result['no_tool_turns'], 24)
        self.assertEqual(result['p95_response_ms'], 1000)
        self.assertEqual(result['interruption_trials'], 6)

    def test_missing_matrix_session_consent_approval_and_audio_fail(self):
        mutations = [
            lambda r: r['calls'].pop(),
            lambda r: r['calls'][0]['sessions'].__setitem__(2, 'different'),
            lambda r: r.update(owner_audio_consent=False),
            lambda r: r['approval'].update(executed_before_resolution=True),
            lambda r: r['calls'][0]['audio'].update(sha256='0' * 64),
            lambda r: r['calls'][0]['audio'].update(path='../outside.wav'),
            lambda r: r.update(number_setup_verified=False),
            lambda r: r['calls'][0].update(hangup_confirmed=False),
        ]
        for mutate in mutations:
            with self.subTest(mutation=mutate):
                receipt = copy.deepcopy(self.receipt)
                mutate(receipt)
                with self.assertRaises(ValueError):
                    live.validate(receipt, self.root)

    def test_exact_thresholds_pass_and_p95_uses_nearest_rank(self):
        for call in self.receipt['calls']:
            for turn in call['turns']:
                turn['response_start_ms'] = turn['owner_end_ms'] + 5000
            call['interruptions'][0]['playback_stop_ms'] = 31000
        self.assertEqual(live.validate(self.receipt, self.root)['p95_response_ms'], 5000)
        self.receipt['calls'][0]['turns'][0]['response_start_ms'] = 11000
        self.assertEqual(live.validate(self.receipt, self.root)['p95_response_ms'], 5000)
        self.receipt['calls'][0]['turns'][1]['response_start_ms'] = 17000
        with self.assertRaises(ValueError):
            live.validate(self.receipt, self.root)

    def test_truncated_audio_is_rejected_even_with_an_updated_hash(self):
        evidence = self.receipt['calls'][0]['audio']
        audio = self.root / evidence['path']
        audio.write_bytes(audio.read_bytes()[:-100])
        evidence['sha256'] = hashlib.sha256(audio.read_bytes()).hexdigest()
        with self.assertRaises(ValueError):
            live.validate(self.receipt, self.root)

    def test_reusing_the_same_recorded_sample_does_not_inflate_coverage(self):
        self.receipt['calls'][1]['audio'] = dict(self.receipt['calls'][0]['audio'])
        with self.assertRaises(ValueError):
            live.validate(self.receipt, self.root)

    def test_thresholds_and_unmeasured_or_invalid_samples_fail(self):
        mutations = [
            lambda r: [t.update(response_start_ms=t['owner_end_ms'] + 5001) for c in r['calls'] for t in c['turns']],
            lambda r: r['calls'][0]['interruptions'][0].update(playback_stop_ms=31001),
            lambda r: r['calls'][0]['turns'][0].update(response_start_ms=61000),
            lambda r: r['calls'][0]['turns'][0].update(response_start_ms=float('nan')),
            lambda r: [t.update(used_tools=True) for c in r['calls'] for t in c['turns']],
            lambda r: r['measurement'].update(method='server_send_timestamps'),
        ]
        for mutate in mutations:
            receipt = copy.deepcopy(self.receipt)
            mutate(receipt)
            with self.assertRaises(ValueError):
                live.validate(receipt, self.root)
