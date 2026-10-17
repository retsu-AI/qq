"""Offline behavioral checks for the live-goal evidence auditor."""
import copy
import hashlib
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest

AUDITOR = pathlib.Path(__file__).with_name('audit_live.py')
SOURCE = 'a' * 40
MODEL = 'openai-codex/gpt-6.1-sol'


class LiveAuditTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = pathlib.Path(self.directory.name)
        binary = self.root / 'qq'
        binary.write_text(f'#!{sys.executable}\nprint("qq 0.1.7 ({SOURCE} 2026-10-16)")\n')
        binary.chmod(0o700)
        self.launch = {
            'pid': -1, 'supervisor_start_ticks': 0, 'started_epoch': 1000,
            'source': SOURCE, 'binary': str(binary),
            'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
            'billing': 'subscription-only; no paid API fallback', 'max_goal_runs': 8,
            'credential_isolation': {
                'captured_at_ms': 999999,
                'credentials': [{'provider': 'openai-codex', 'auth_kind': 'oauth'}],
                'api_key_environment_names': [], 'paid_fallback_enabled': False,
            },
        }
        self.exit = {'exit_code': 8, 'finished_epoch': 8200.1}
        self.goal = {
            'session_id': 'session', 'goal_id': 'goal', 'revision': 1,
            'budget': {'deadline_ms': 15400000, 'max_goal_runs': 8, 'per_run': {
                'max_duration_ms': 3600000, 'max_model_turns': 64,
                'max_total_tokens': 3000000,
            }},
            'checklist': [{'id': 'item', 'text': 'Complete useful work', 'state': 'done'}],
            'status': {'type': 'active'},
            'counters': {'runs': 8, 'usage': {
                'input_tokens': 1, 'cache_read_input_tokens': 0,
                'cache_write_input_tokens': 0, 'output_tokens': 1,
            }, 'estimated_cost_usd_nanos': None},
        }
        self.records = [self.event({'type': 'goal_updated', 'goal': copy.deepcopy(self.goal)}, 1000000)]
        self.records.append(self.event({'type': 'model_turn_completed', 'model': {'model': MODEL},
                                       'usage': copy.deepcopy(self.goal['counters']['usage'])}, 1000001))
        for n in range(3):
            self.records.append(self.event({'type': 'session_compacted'}, 1010000 + n))
        final = copy.deepcopy(self.goal)
        final['status'] = {'type': 'budget_exhausted'}
        self.records.append(self.event({'type': 'goal_updated', 'goal': final}, 8200000))
        self.records.append({'type': 'goal', 'workspace_id': 'workspace', 'goal': final,
                             'status': 'budget_exhausted', 'exit_code': 8})

    @staticmethod
    def event(event, timestamp, session='session'):
        return {'type': 'event', 'envelope': {
            'session_id': session, 'cursor': {'workspace_id': 'workspace'},
            'occurred_at_ms': timestamp, 'event': event,
        }}

    def audit(self, source=SOURCE):
        (self.root / 'g5-launch.json').write_text(json.dumps(self.launch))
        (self.root / 'g5-exit.json').write_text(json.dumps(self.exit))
        (self.root / 'g5-live.jsonl').write_text(''.join(json.dumps(r) + '\n' for r in self.records))
        completed = subprocess.run([sys.executable, str(AUDITOR), str(self.root), source],
                                   text=True, capture_output=True, check=True)
        return json.loads(completed.stdout)

    def test_valid_isolated_trial_qualifies(self):
        self.assertTrue(self.audit()['qualifies'])

    def test_paid_billing_is_not_subscription_evidence(self):
        self.launch['billing'] = 'paid_api'
        self.assertFalse(self.audit()['qualifies'])

    def test_missing_or_nonisolated_credentials_do_not_qualify(self):
        valid = copy.deepcopy(self.launch['credential_isolation'])
        for evidence in [None, {**valid, 'api_key_environment_names': ['OPENAI_API_KEY']},
                         {**valid, 'paid_fallback_enabled': True},
                         {**valid, 'credentials': [{'provider': 'openai', 'auth_kind': 'api_key'}]}]:
            with self.subTest(evidence=evidence):
                if evidence is None:
                    self.launch.pop('credential_isolation', None)
                else:
                    self.launch['credential_isolation'] = evidence
                self.assertFalse(self.audit()['qualifies'])

    def test_foreign_compactions_do_not_satisfy_trial(self):
        for record in self.records:
            envelope = record.get('envelope', {})
            if envelope.get('event', {}).get('type') == 'session_compacted':
                envelope['session_id'] = 'foreign'
        self.assertFalse(self.audit()['qualifies'])

    def test_foreign_goals_models_and_usage_do_not_change_trial(self):
        other = copy.deepcopy(self.goal)
        other['session_id'] = 'foreign'
        other['goal_id'] = 'foreign-goal'
        self.records.insert(2, self.event({'type': 'goal_updated', 'goal': other}, 1000002, 'foreign'))
        self.records.insert(3, self.event({'type': 'model_turn_completed',
                                         'model': {'model': 'foreign-model'},
                                         'usage': {**other['counters']['usage'], 'input_tokens': 999}},
                                        1000003, 'foreign'))
        self.assertTrue(self.audit()['qualifies'])

    def test_replacement_cannot_inherit_old_goal_compactions(self):
        self.records[-2]['envelope']['event']['goal']['goal_id'] = 'replacement'
        self.records[-1]['goal']['goal_id'] = 'replacement'
        self.assertFalse(self.audit()['qualifies'])

    def test_launcher_idle_time_cannot_make_short_goal_multi_hour(self):
        for record in self.records:
            if 'envelope' in record:
                record['envelope']['occurred_at_ms'] += 9000000
        self.records[-2]['envelope']['occurred_at_ms'] = 10300000
        self.exit['finished_epoch'] = 10300.1
        result = self.audit()
        self.assertFalse(result['qualifies'])
        self.assertFalse(result['checks']['real_multi_hour_duration'])

    def test_other_runtime_is_not_qualified(self):
        self.assertFalse(self.audit('b' * 40)['qualifies'])


if __name__ == '__main__':
    unittest.main()
