#!/usr/bin/env python3
"""Stateful test-only Herdr CLI. No terminals, providers, or network access."""
import fcntl
import json
import os
from pathlib import Path
import sys

root = Path(__file__).parent
lock = (root / 'layout.lock').open('a')
fcntl.flock(lock, fcntl.LOCK_EX)
file = root / 'layout-state.json'
state = json.loads(file.read_text()) if file.exists() else {'next': 0, 'spaces': {}, 'tabs': {}, 'panes': {}}
args = sys.argv[1:]
if os.environ.get('PROOF_HERDR_LOG'):
    with open(os.environ['PROOF_HERDR_LOG'], 'a') as log:
        log.write(json.dumps(args) + '\n')
with (root / 'calls.log').open('a') as log:
    log.write('<call>\n' + '\n'.join(args) + '\n</call>\n')
while args and args[0] in ('--machine', '--session'):
    args = args[2:]
area, verb = args[:2]

def flag(name, default=None):
    return args[args.index(name) + 1] if name in args else default

def new_id(prefix):
    state['next'] += 1
    return prefix + str(state['next'])

def fail(message):
    print(json.dumps({'error': {'code': 'pane_not_found', 'message': message}}), file=sys.stderr)
    sys.exit(1)

def pane_ref(ref):
    pane = state['panes'].get(ref) or next((p for p in state['panes'].values() if p.get('name') == ref), None)
    if pane is None:
        fail('pane not found')
    return pane

result = {}
if (area, verb) == ('status', 'server'):
    result = {'running': True, 'runtime_generation': state.get('generation', 'fixture-v1')}
elif (area, verb) == ('api', 'snapshot'):
    result = {}
elif (area, verb) == ('workspace', 'list'):
    result = {'workspaces': list(state['spaces'].values())}
elif (area, verb) == ('tab', 'list'):
    result = {'tabs': list(state['tabs'].values())}
elif (area, verb) == ('pane', 'list'):
    result = {'panes': list(state['panes'].values())}
elif (area, verb) in [('workspace', 'create'), ('tab', 'create'), ('pane', 'split')]:
    if area == 'workspace':
        workspace = new_id('w')
        state['spaces'][workspace] = {'workspace_id': workspace, 'label': flag('--label')}
    elif area == 'tab':
        workspace = flag('--workspace')
    else:
        parent = pane_ref(flag('--pane'))
        workspace = parent['workspace_id']
    if area != 'pane':
        tab_id = workspace + ':' + new_id('t')
        state['tabs'][tab_id] = {'tab_id': tab_id, 'workspace_id': workspace, 'label': flag('--label')}
    else:
        tab_id = parent['tab_id']
    pane_id = workspace + ':' + new_id('p')
    env = {}
    for i, arg in enumerate(args):
        if arg == '--env':
            key, value = args[i + 1].split('=', 1)
            env[key] = value
    pane = {'pane_id': pane_id, 'terminal_id': new_id('term'), 'tab_id': tab_id,
        'workspace_id': workspace, 'agent': None, 'agent_status': 'unknown',
        'agent_session': None, 'cwd': flag('--cwd'), 'launch_env': env, 'label': flag('--label')}
    state['panes'][pane_id] = pane
    result = {'workspace': state['spaces'][workspace], 'tab': state['tabs'][tab_id], 'root_pane': pane}
elif verb == 'rename':
    records = state['tabs'] if area == 'tab' else state['panes']
    records[args[2]]['label'] = args[3]
elif (area, verb) == ('agent', 'start'):
    pane = pane_ref(flag('--pane'))
    pane['name'] = args[2]
    pane['agent'] = flag('--kind')
    pane['agent_status'] = 'idle'
    forwarded = args[args.index('--') + 1:] if '--' in args else []
    session = None
    for selector in ('resume', '--resume', '--session'):
        if selector in forwarded:
            session = forwarded[forwarded.index(selector) + 1]
    pane['agent_session'] = {'value': session or new_id('native')}
    if os.environ.get('PROOF_WRITE_NATIVE_HISTORY') == '1':
        home = Path(pane['launch_env']['CODEX_HOME'])
        proof_root = root.parent.resolve()
        if not home.resolve().is_relative_to(proof_root):
            raise RuntimeError('fixture home outside proof root')
        history = home / 'sessions' / (pane['agent_session']['value'] + '.jsonl')
        history.parent.mkdir(exist_ok=True)
        if not history.exists():
            history.write_text('fixture conversation\n')
    result = {'agent': pane.copy(), 'argv': forwarded}
elif area == 'agent' and verb in ('get', 'wait'):
    result = {'agent': pane_ref(args[2]).copy()}
elif (area, verb) == ('agent', 'list'):
    result = {'agents': [p.copy() for p in state['panes'].values() if p.get('agent')]}
elif (area, verb) == ('agent', 'prompt'):
    pane = pane_ref(args[2])
    text = args[3]
    if text.startswith('/rename '):
        pane['session_name'] = text[len('/rename '):]
    elif text == '/exit':
        pane['agent'] = None
        pane['agent_status'] = 'unknown'
        pane['agent_session'] = None
    result = {'agent': pane.copy()}
elif (area, verb) == ('agent', 'send-keys'):
    pass
elif (area, verb) == ('pane', 'get'):
    result = {'pane': pane_ref(args[2]).copy()}
elif (area, verb) == ('pane', 'process-info'):
    pane = pane_ref(flag('--pane'))
    pid = 501 if pane.get('agent') else 500
    result = {'process_info': {'shell_pid': 500, 'foreground_process_group_id': pid,
        'foreground_processes': [{'pid': pid}]}}
elif (area, verb) == ('pane', 'close'):
    pane = pane_ref(args[2])
    del state['panes'][pane['pane_id']]
    if not any(p['tab_id'] == pane['tab_id'] for p in state['panes'].values()):
        del state['tabs'][pane['tab_id']]
    if not any(t['workspace_id'] == pane['workspace_id'] for t in state['tabs'].values()):
        del state['spaces'][pane['workspace_id']]
elif area in ('agent', 'pane') and verb == 'read':
    print('fixture transcript')
    sys.exit(0)
elif (area, verb) == ('machine', 'list'):
    result = {'machines': json.loads(os.environ.get('PROOF_MACHINE_CATALOG', '[{"id":"remote-a","target":"fixture-worker","enabled":true}]'))}
else:
    raise RuntimeError(args)
file.write_text(json.dumps(state))
print(json.dumps({'result': result}))
