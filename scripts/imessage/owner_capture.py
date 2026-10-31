#!/usr/bin/env python3
"""Capture bound owner input independently of the outgoing Messages sender."""
import argparse
from pathlib import Path
import sys
from imessage_send import AgentCli, AgentError
from imessage_sync import decode_attributed_body
from owner_events import capture


def parser():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--agent-dir", required=True)
    ap.add_argument("--remote")
    ap.add_argument("--binary", default="target/release/augmentagent")
    ap.add_argument("--db", type=lambda p: Path(p).expanduser().resolve(),
                    default=Path.home() / "Library/Messages/chat.db")
    return ap


def run_once(agent, db):
    binding = agent.owner_binding()
    if not binding or not binding["enabled"]:
        return 0
    batch = capture(db, binding, decode_body=decode_attributed_body)
    agent.owner_events(batch)
    return len(batch["events"])


def main():
    args = parser().parse_args()
    agent = None
    try:
        agent = AgentCli(args.agent_dir, args.binary, args.remote)
        count = run_once(agent, args.db)
        print(f"owner events durably accepted: {count}")
        return 0
    except (AgentError, RuntimeError, ValueError, OSError) as error:
        # Native/private command content is never part of this diagnostic.
        print(f"owner capture unavailable ({type(error).__name__}); check native schema and agent status", file=sys.stderr)
        if agent is not None:
            try:
                agent.owner_capture_error("Mac owner capture failed; inspect native schema, enrollment and capture logs")
            except AgentError:
                pass
        return 1


if __name__ == "__main__":
    sys.exit(main())
