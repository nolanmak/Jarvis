#!/usr/bin/env python3
"""Claude Code PreToolUse guard (#1754): keep Cargo target dirs registered.

A build that picks its own target dir with `CARGO_TARGET_DIR=...` or
`--target-dir` outside `scripts/cargo-managed.sh` writes a cache nothing
registers, so `disk prune`, pressure reclamation and `disk status` never see
it; tens of GB accumulated that way. The wrapper gives each worktree its own
registered dir (the collision-safe path) and honours an explicit
`CARGO_TARGET_DIR` placed before it.

Best effort by design: it reads one Bash command, split on `;`, `&&`, `||`,
`|` and newlines. It does not follow `bash -c`, make/just, an exported
variable from an earlier call, or `.cargo/config.toml`; `augmentagent disk
status` lists whatever slips through. `disk build` itself refuses a child
`--target-dir`, for every client.

Protocol (same as agent-pr-verify-gate.sh): read the PreToolUse JSON on
stdin; print nothing to allow, or a block decision JSON; always exit 0.
Anything unparseable is allowed.
"""
import json
import os
import shlex
import sys

TARGET_VARS = ("CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR")
ESCAPE = "AUGMENTAGENT_ALLOW_UNMANAGED_TARGET"
OPERATORS = {";", "&&", "||", "|", "|&", "&"}

REASON = (
    "Blocked by scripts/cargo-target-guard.py (#1754): this Cargo build picks "
    "its own target dir ({why}), which creates a cache that disk housekeeping "
    "never registers or prunes.\n"
    "Use the managed wrapper instead: `./scripts/cargo-managed.sh <cargo args>` "
    "(e.g. `./scripts/cargo-managed.sh test -p augmentagent-cli`). It gives each "
    "worktree its own registered target, so worktrees never clobber each other. "
    "If you need a specific cache, put `CARGO_TARGET_DIR=<dir>` before the "
    "wrapper, never `--target-dir` after it. Plain `cargo` in the checkout's own "
    "`target/` is fine. Bootstrapping the storage commands themselves: prefix "
    f"the command with `{ESCAPE}=1`. See docs/disk-storage.md."
)


def segments(command):
    for line in command.splitlines():
        lexer = shlex.shlex(line, posix=True, punctuation_chars=";&|")
        lexer.whitespace_split = True
        lexer.commenters = ""
        current = []
        for token in lexer:
            if token in OPERATORS:
                if current:
                    yield current
                current = []
            else:
                current.append(token)
        if current:
            yield current


def is_assignment(token):
    name, eq, _ = token.partition("=")
    return bool(eq) and name.isidentifier()


def target_flag(args):
    """The first `--target-dir` among Cargo's own arguments (before `--`)."""
    for arg in args:
        if arg == "--":
            return None
        if arg == "--target-dir" or arg.startswith("--target-dir="):
            return arg
    return None


def bypass(tokens):
    """Why this segment bypasses the wrapper, or None."""
    assigned = []
    i = 0
    while True:
        while i < len(tokens) and is_assignment(tokens[i]):
            assigned.append(tokens[i].partition("=")[0])
            i += 1
        if i < len(tokens) and os.path.basename(tokens[i]) == "env":
            i += 1
            while i < len(tokens) and tokens[i].startswith("-"):
                i += 1
            continue
        if i < len(tokens) and tokens[i] in ("command", "exec", "time", "nice"):
            i += 1
            continue
        break
    if ESCAPE in assigned or i >= len(tokens):
        return None
    program, args = os.path.basename(tokens[i]), tokens[i + 1:]
    if program in ("bash", "sh") and args and not args[0].startswith("-"):
        program, args = os.path.basename(args[0]), args[1:]
    if program == "cargo-managed.sh":
        flag = target_flag(args)
        return f"`{flag}` passed through the wrapper overrides its registered target" if flag else None
    if program == "augmentagent":
        if args[:2] == ["disk", "build"] and "--" in args:
            child = args[args.index("--") + 1:]
            flag = target_flag(child[1:]) if child[:1] == ["cargo"] else None
            return f"`{flag}` in the child command overrides the leased target" if flag else None
        return None
    if program != "cargo":
        return None
    for var in TARGET_VARS:
        if var in assigned:
            return f"`{var}=` on a bare cargo command"
    flag = target_flag(args)
    return f"`{flag}` on a bare cargo command" if flag else None


def main():
    try:
        event = json.load(sys.stdin)
        if event.get("tool_name") != "Bash":
            return
        command = (event.get("tool_input") or {}).get("command") or ""
        for tokens in segments(command):
            why = bypass(tokens)
            if why:
                reason = REASON.format(why=why)
                print(json.dumps({
                    "decision": "block",
                    "reason": reason,
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "deny",
                        "permissionDecisionReason": reason,
                    },
                }))
                return
    except Exception:  # fail open: never wedge the session on a parse error
        return


if __name__ == "__main__":
    main()
