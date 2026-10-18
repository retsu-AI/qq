"""Sanitize and reconcile a private G5 stream without copying session content."""
import collections
import datetime
import hashlib
import json
import pathlib
import re
import subprocess
import sys
import time

root = pathlib.Path(sys.argv[1])
launch = json.loads((root / 'g5-launch.json').read_text())
runtime_source = sys.argv[2]
assert len(runtime_source) == 40 and all(c in '0123456789abcdef' for c in runtime_source), 'explicit runtime commit required'
exit_path = root / 'g5-exit.json'
exit_record = json.loads(exit_path.read_text()) if exit_path.exists() else None
identity = launch
if (root / 'g5-resume.json').exists():
    identity = json.loads((root / 'g5-resume.json').read_text())
    path = root / 'g5-resume-exit.json'
    exit_record = json.loads(path.read_text()) if path.exists() else None
try:
    stat = pathlib.Path(f"/proc/{identity['pid']}/stat").read_text()
    fields = stat[stat.rfind(')') + 2:].split()
    live = fields[19] == str(identity['supervisor_start_ticks']) and fields[0] != 'Z'
except FileNotFoundError:
    live = False

usage = collections.Counter()
models = collections.Counter()
missing_usage = 0
goal = None
initial_budget = None
finals = []
compactions = []
outcomes = collections.Counter()
invalid_lines = 0
record_count = 0
for line in (root / 'g5-live.jsonl').open():
    try:
        record = json.loads(line)
    except json.JSONDecodeError:
        invalid_lines += 1
        continue
    record_count += 1
    if record.get('type') == 'goal':
        finals.append((record_count, record))

terminal = finals[-1][1] if finals else None
session_id = terminal['goal'].get('session_id') if terminal else None
workspace_id = terminal.get('workspace_id') if terminal else None
terminal_goal_id = terminal['goal'].get('goal_id') if terminal else None
goal_ids = set()
snapshot_sessions_match = True
budgets_unchanged = True
admitted_at_ms = None
terminal_at_ms = None
for line in (root / 'g5-live.jsonl').open():
    try:
        record = json.loads(line)
    except json.JSONDecodeError:
        continue
    if record.get('type') != 'event':
        continue
    envelope = record.get('envelope', {})
    if not session_id or not workspace_id or envelope.get('session_id') != session_id or envelope.get('cursor', {}).get('workspace_id') != workspace_id:
        continue
    event = envelope.get('event', {})
    event_type = event.get('type')
    if event_type == 'goal_updated':
        goal = event['goal']
        goal_ids.add(goal.get('goal_id'))
        snapshot_sessions_match &= goal.get('session_id') == session_id
        if initial_budget is None:
            initial_budget = goal['budget']
            admitted_at_ms = envelope.get('occurred_at_ms')
        budgets_unchanged &= goal['budget'] == initial_budget
        if terminal and goal.get('goal_id') == terminal_goal_id and goal.get('status', {}).get('type') == terminal.get('status'):
            terminal_at_ms = envelope.get('occurred_at_ms')
    elif goal is None:
        continue
    elif event_type == 'model_turn_completed':
        models[str(event.get('model', {}).get('model'))] += 1
        measured = event.get('usage')
        if measured is None:
            missing_usage += 1
        else:
            for name in ['input_tokens', 'cache_read_input_tokens', 'cache_write_input_tokens', 'output_tokens']:
                value = measured[name]
                assert type(value) is int and value >= 0, 'invalid token measurement'
                usage[name] += value
    elif event_type == 'session_compacted':
        assert goal is not None, 'compaction lacks a preceding goal snapshot'
        checklist = goal.get('checklist', [])
        items = [(item['id'], item['text']) for item in checklist]
        complete_items = [(item['id'], item['text'], item['state']) for item in checklist]
        compactions.append({
            'goal_id': goal.get('goal_id'),
            'session_id': goal.get('session_id'),
            'revision': goal['revision'],
            'items': len(checklist),
            'identity_sha256': hashlib.sha256(json.dumps(items, sort_keys=True).encode()).hexdigest(),
            'state_sha256': hashlib.sha256(json.dumps(complete_items, sort_keys=True).encode()).hexdigest(),
            'states': dict(collections.Counter(str(item['state']) for item in checklist)),
        })
    elif event_type == 'run_finished':
        origin = (event.get('origin') or {}).get('type', 'ordinary')
        outcome = event.get('outcome', {})
        status = outcome.get('type', 'unknown')
        qualifier = (outcome.get('exhaustion') or {}).get('limit') or (outcome.get('failure') or {}).get('kind')
        outcomes['/'.join(str(x) for x in [origin, status, qualifier] if x)] += 1

finished = exit_record.get('finished_epoch') if exit_record else None
launcher_elapsed = (finished if finished is not None else time.time()) - launch['started_epoch']
event_duration_valid = type(admitted_at_ms) is int and type(terminal_at_ms) is int and terminal_at_ms >= admitted_at_ms
elapsed = (terminal_at_ms - admitted_at_ms) / 1000 if event_duration_valid else None
isolation = launch.get('credential_isolation')
if not isinstance(isolation, dict):
    isolation = {}
credential_isolation_valid = (
    isolation.get('credentials') == [{'provider': 'openai-codex', 'auth_kind': 'oauth'}]
    and isolation.get('api_key_environment_names') == []
    and isolation.get('paid_fallback_enabled') is False
    and type(isolation.get('captured_at_ms')) is int
    and type(admitted_at_ms) is int
    and isolation['captured_at_ms'] <= admitted_at_ms
)
checks = {
    'supervisor_stopped': not live,
    'exit_record_present': exit_record is not None,
    'single_final_record': len(finals) == 1,
    'final_record_is_last': bool(finals) and finals[-1][0] == record_count,
    'valid_complete_jsonl': invalid_lines == 0,
    'goal_event_duration_present': event_duration_valid,
    'real_multi_hour_duration': elapsed is not None and elapsed >= 2 * 60 * 60,
    'one_evaluated_goal_identity': bool(terminal_goal_id) and goal_ids == {terminal_goal_id} and snapshot_sessions_match and all(c['goal_id'] == terminal_goal_id and c['session_id'] == session_id for c in compactions),
    'subscription_only_billing': launch.get('billing') == 'subscription-only; no paid API fallback',
    'credential_isolation_recorded': credential_isolation_valid,
    'at_least_three_real_compactions': len(compactions) >= 3,
    'one_subscription_model_route': set(models) == {'openai-codex/gpt-6.1-sol'},
    'no_runner_resume': identity is launch,
    'finite_admission_bounds': initial_budget is not None and type(initial_budget.get('deadline_ms')) is int and type(admitted_at_ms) is int and initial_budget['deadline_ms'] > admitted_at_ms and type(initial_budget.get('max_goal_runs')) is int and initial_budget['max_goal_runs'] > 0 and all(type(initial_budget.get('per_run', {}).get(k)) is int and initial_budget['per_run'][k] > 0 for k in ['max_duration_ms', 'max_model_turns', 'max_total_tokens']),
}
if terminal is not None:
    expected_codes = {'achieved': 0, 'harness_failure': 4, 'paused': 6, 'blocked': 7, 'budget_exhausted': 8, 'interrupted': 130}
    selected_code = terminal.get('exit_code')
    checks['final_status_code_matches'] = expected_codes.get(terminal.get('status')) == selected_code
    checks['process_exit_matches_final'] = exit_record is not None and exit_record.get('exit_code') == selected_code
    counters = terminal['goal']['counters']
    measured = counters.get('usage')
    checks['all_turn_usage_reported'] = missing_usage == 0
    checks['goal_usage_equals_all_completed_turns'] = measured is not None and all(measured.get(name, 0) == value for name, value in usage.items())
    items = [(item['id'], item['text']) for item in terminal['goal'].get('checklist', [])]
    digest = hashlib.sha256(json.dumps(items, sort_keys=True).encode()).hexdigest()
    checks['checklist_identity_survives_compactions'] = bool(items) and all(c['identity_sha256'] == digest for c in compactions)
    checks['goal_terminal'] = terminal.get('status') in {'achieved', 'blocked', 'budget_exhausted'} and terminal['goal']['status'].get('type') == terminal.get('status')
    checks['goal_budget_unchanged'] = initial_budget is not None and budgets_unchanged and terminal['goal']['budget'] == initial_budget
    checks['run_cap_respected'] = counters['runs'] <= launch['max_goal_runs']
    checks['cost_metadata_reported'] = 'estimated_cost_usd_nanos' in counters
else:
    counters = goal.get('counters') if goal else None

binary = pathlib.Path(launch['binary'])
binary_matches = hashlib.sha256(binary.read_bytes()).hexdigest() == launch['binary_sha256'] if binary.is_file() else None
runtime_version = None
revision_matches = False
if binary.is_file():
    version = subprocess.run([str(binary), '--version'], capture_output=True, text=True, timeout=30)
    if version.returncode == 0:
        runtime_version = version.stdout.strip()
        revision = re.search(r'\(([0-9a-f]{7,40}) ', runtime_version)
        revision_matches = bool(revision and runtime_source.startswith(revision.group(1)))
checks['launch_binary_hash_matches'] = binary_matches is True
checks['runtime_revision_matches_binary'] = revision_matches
result = {
    'runtime_source': runtime_source,
    'runtime_version': runtime_version,
    'audit_script_sha256': hashlib.sha256(pathlib.Path(__file__).read_bytes()).hexdigest(),
    'launch_source': launch['source'],
    'binary_sha256': launch['binary_sha256'],
    'binary_path_current_matches_launch': binary_matches,
    'supervisor_live': live,
    'started_at': datetime.datetime.fromtimestamp(launch['started_epoch'], datetime.timezone.utc).isoformat(),
    'finished_at': datetime.datetime.fromtimestamp(finished, datetime.timezone.utc).isoformat() if finished is not None else None,
    'elapsed_seconds': round(elapsed, 3) if elapsed is not None else None,
    'launcher_elapsed_seconds': round(launcher_elapsed, 3),
    'goal_admitted_at_ms': admitted_at_ms,
    'goal_terminal_at_ms': terminal_at_ms,
    'session_id': session_id,
    'goal_id': terminal_goal_id,
    'model_turns': sum(models.values()),
    'observed_usage': dict(usage),
    'missing_turn_usage': missing_usage,
    'final_records': len(finals),
    'final_status': terminal.get('status') if terminal else None,
    'process_exit': exit_record.get('exit_code') if exit_record else None,
    'goal_counters': counters,
    'admission_budget': initial_budget,
    'compactions': compactions,
    'settled_outcomes': dict(outcomes),
    'checks': checks,
    'qualifies': all(checks.values()),
    'billing': launch['billing'],
    'estimated_cost_usd_nanos': counters.get('estimated_cost_usd_nanos') if counters else None,
    'subscription_value': 'unknown',
}
output = root / 'g5-terminal-audit-v2.json'
output.write_text(json.dumps(result, indent=2) + '\n')
output.chmod(0o600)
print(json.dumps({key: result[key] for key in ['runtime_source', 'launch_source', 'supervisor_live', 'elapsed_seconds', 'model_turns', 'final_status', 'process_exit', 'checks', 'qualifies']}, indent=2))
