#!/usr/bin/env python3
"""Every integration command the yolo preset grants is read-only (#2216).

The list lives in fixtures/yolo-readonly-commands.json and is also what the
Rust preset builds its permissions from. A yolo turn is answered by an
unfiltered model that reads untrusted web pages, so nothing on this list may
send, post, schedule or otherwise change anything.
"""
import importlib.util
import json
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('codex_tool_bridge', SCRIPTS / 'codex-tool-bridge.py')
bridge = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bridge)

FIXTURE = json.loads((Path(__file__).parent / 'fixtures' / 'yolo-readonly-commands.json').read_text())


def commands():
    for program in ('augmentagent', 'aa-gh'):
        for pattern in FIXTURE[program]:
            # A trailing `*` stands for arguments the model supplies.
            yield program, pattern, (program + ' ' + pattern.replace('*', '--limit 5')).strip()


class YoloReadonlyCommands(unittest.TestCase):
    def test_every_granted_command_is_read_only_for_the_bridge(self):
        seen = 0
        for program, pattern, command in commands():
            with self.subTest(command=command):
                self.assertTrue(
                    bridge.read_only_operation('Bash', {'command': command}),
                    f'{program} {pattern} is not on the bridge read-only contract')
            seen += 1
        self.assertEqual(seen, 20)

    def test_the_list_has_no_mutating_verb(self):
        verbs = {pattern.split()[1] if program == 'augmentagent' else pattern.split()[1]
                 for program, pattern, _ in commands()}
        for verb in ('send', 'send-now', 'compose', 'dm', 'comment', 'create', 'create-event', 'rsvp',
                     'reschedule', 'sign', 'upload'):
            self.assertNotIn(verb, verbs)

    def test_a_mutating_neighbour_is_not_read_only(self):
        # The contract is about verbs, not programs: the same program's
        # sending verbs must stay outside it.
        for command in ('augmentagent gmail send --to someone@example.com', 'aa-gh issue create --title x'):
            self.assertFalse(bridge.read_only_operation('Bash', {'command': command}), command)


if __name__ == '__main__':
    unittest.main()
