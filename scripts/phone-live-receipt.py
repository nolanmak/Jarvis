#!/usr/bin/env python3
"""Validate #1496 live receipt coverage and annotated audible timing.

This checks recorded evidence structure, not whether annotations are truthful.
Review the opt-in audio and native tool/approval logs before accepting the receipt.
"""
import argparse
import hashlib
import json
import math
from pathlib import Path
import re
import wave


def require(condition, message):
    if not condition:
        raise ValueError(message)


def milliseconds(value):
    require(type(value) in (int, float) and math.isfinite(value) and value >= 0,
            'audio offsets must be finite nonnegative milliseconds')
    return value


def audio_duration(evidence, root):
    path = Path(evidence['path'])
    require(not path.is_absolute() and '..' not in path.parts, 'audio path must stay inside the receipt directory')
    path = (root / path).resolve(strict=True)
    require(path.is_relative_to(root.resolve()), 'audio path escapes the receipt directory')
    require(path.stat().st_size <= 512 * 1024 * 1024, 'audio artifact exceeds 512 MiB')
    with path.open('rb') as stream:
        digest = hashlib.file_digest(stream, 'sha256').hexdigest()
    require(digest == evidence['sha256'], 'audio artifact hash mismatch')
    with wave.open(str(path), 'rb') as stream:
        require(stream.getnframes() > 0 and stream.getframerate() > 0, 'audio artifact is empty')
        expected = stream.getnframes() * stream.getnchannels() * stream.getsampwidth()
        observed = 0
        while chunk := stream.readframes(65536):
            observed += len(chunk)
        require(observed == expected, 'audio artifact is truncated')
        return 1000 * stream.getnframes() / stream.getframerate()


def validate(receipt, root):
    require(receipt['version'] == 1 and receipt['kind'] == 'phone-live', 'unsupported receipt format')
    require(re.fullmatch(r'[0-9a-f]{40}', receipt['commit']) is not None, 'receipt must identify the tested commit')
    require(receipt['owner_audio_consent'] is True, 'owner audio consent is required')
    require(receipt['number_setup_verified'] is True, 'live private number verification is missing')
    measurement = receipt['measurement']
    require(measurement['method'] == 'recorded_audio' and bool(measurement['description'].strip()),
            'document audible measurement from recorded audio, not server-send times')
    coverage, origins, seen_samples = set(), set(), set()
    response_times, interruptions = [], []
    calls = receipt['calls']
    require(bool(calls), 'live calls are missing')
    for call in calls:
        require(call['provider'] in ('claude', 'codex'), 'unsupported native provider')
        require(call['host'] in ('linux', 'macos') and call['architecture'] in ('x86_64', 'arm64'), 'unsupported host')
        host = 'linux' if call['host'] == 'linux' else 'macos-' + call['architecture']
        coverage.add((host, call['provider']))
        require(call['origin'] in ('discord', 'slack', 'whatsapp'), 'unsupported mandatory origin')
        origins.add(call['origin'])
        require(call['outcome'] == 'completed' and call['hangup_confirmed'] is True, 'call must complete and confirm hangup')
        require(bool(call['voice']) and call['provider_settings']['transport'] == 'conversationrelay', 'provider and voice settings are missing')
        turns, sessions = call['turns'], call['sessions']
        require(len(turns) >= 3 and len(sessions) == len(turns) + 2, 'record text, three or more phone turns, then text')
        require(all(isinstance(s, str) and s and s == sessions[0] for s in sessions), 'native session changed during the call')
        tool = call['read_only_tool']
        require(tool['completed'] is True and tool['session_id'] == sessions[0], 'read-only tool must complete in the native session')
        duration = audio_duration(call['audio'], root)
        for turn in turns:
            end, start = milliseconds(turn['owner_end_ms']), milliseconds(turn['response_start_ms'])
            require(end <= start <= duration, 'response sample is reversed or outside recorded audio')
            require(type(turn['used_tools']) is bool, 'tool-use classification is required')
            sample = (call['audio']['sha256'], end, start)
            require(sample not in seen_samples, 'duplicate audio timing sample')
            seen_samples.add(sample)
            if not turn['used_tools']:
                response_times.append(start - end)
        require(bool(call['interruptions']), 'each call must include an interruption')
        for trial in call['interruptions']:
            start, stop = milliseconds(trial['owner_start_ms']), milliseconds(trial['playback_stop_ms'])
            require(start <= stop <= duration, 'interruption sample is reversed or outside recorded audio')
            interruptions.append(stop - start)
    expected = {(host, provider) for host in ('linux', 'macos-arm64', 'macos-x86_64') for provider in ('claude', 'codex')}
    require(coverage == expected and origins == {'discord', 'slack', 'whatsapp'}, 'required live host/provider/origin coverage is missing')
    require(len(response_times) >= 20, 'at least twenty no-tool turns are required')
    p95 = sorted(response_times)[math.ceil(0.95 * len(response_times)) - 1]
    require(p95 <= 5000, 'audible response p95 exceeds five seconds')
    require(len(interruptions) >= 5 and max(interruptions) <= 1000, 'at least five interruptions must stop playback within one second')
    approval = receipt['approval']
    index = approval['call_index']
    require(type(index) is int and 0 <= index < len(calls), 'approval must identify a measured call')
    require(approval['pending_observed'] is True and approval['executed_before_resolution'] is False
            and approval['resolution'] == 'approved' and approval['executed_after_resolution'] is True
            and approval['session_id'] == calls[index]['sessions'][0], 'approval gating evidence is incomplete')
    return {'no_tool_turns': len(response_times), 'p95_response_ms': p95,
            'interruption_trials': len(interruptions), 'max_interruption_ms': max(interruptions),
            'calls': len(calls)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('receipt', type=Path)
    args = parser.parse_args()
    try:
        result = validate(json.loads(args.receipt.read_text()), args.receipt.parent)
    except (ValueError, KeyError, TypeError, AttributeError, IndexError, OSError, wave.Error, EOFError):
        # Never echo receipt contents, audio paths or native session identifiers.
        parser.exit(1, 'Live receipt failed validation; check coverage, evidence and timing fields.\n')
    print(json.dumps({'validated_structure_and_timing': True, **result}, sort_keys=True))


if __name__ == '__main__':
    main()
