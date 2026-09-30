#!/usr/bin/env python3
"""Send approved iMessage replies queued by the agent; run on the Mac with Messages."""
import argparse
from pathlib import Path
import sys

from imessage_send import AgentCli, AgentError, ChatDbUnreadable, Sender

DEFAULT_DB = Path.home() / "Library/Messages/chat.db"
DEFAULT_STATE = Path.home() / ".local/state/augmentagent/imessage-send"


def parser():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--agent-dir", required=True,
                    help="absolute path of the agent checkout (on the remote host with --remote)")
    ap.add_argument("--remote", help="[user@]host of a remote agent, reached with SSH batch mode")
    ap.add_argument("--binary", default="target/release/augmentagent",
                    help="agent binary, relative to --agent-dir unless absolute")
    ap.add_argument("--db", type=lambda p: Path(p).expanduser().resolve(), default=DEFAULT_DB)
    ap.add_argument("--state-dir", type=lambda p: Path(p).expanduser().resolve(),
                    default=DEFAULT_STATE)
    ap.add_argument("--max-items", type=int, default=10)
    ap.add_argument("--deadline", type=int, default=60, metavar="SECONDS",
                    help="how long to wait for chat.db to show the sent message")
    return ap


def main(argv=None, sender_factory=Sender, agent_factory=AgentCli):
    ap = parser()
    args = ap.parse_args(argv)
    try:
        agent = agent_factory(agent_dir=args.agent_dir, binary=args.binary, remote=args.remote)
    except ValueError as error:
        ap.error(str(error))
    sender = sender_factory(agent, args.db, args.state_dir, deadline_s=args.deadline,
                            max_items=args.max_items)
    try:
        sender.run_once()
    except ChatDbUnreadable as error:
        print(str(error), file=sys.stderr)
        return 1
    except AgentError as error:
        print(f"agent unavailable: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
