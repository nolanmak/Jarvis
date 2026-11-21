import importlib.util
import json
import sqlite3
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path.cwd()))
module_path = Path.cwd() / "scripts" / "imessage" / "owner_events.py"
spec = importlib.util.spec_from_file_location("owner_events", module_path)
owner_events = importlib.util.module_from_spec(spec)
spec.loader.exec_module(owner_events)

with tempfile.TemporaryDirectory() as directory:
    database = Path(directory) / "fixture.sqlite"
    connection = sqlite3.connect(database)
    connection.execute("CREATE TABLE message(guid TEXT)")
    connection.commit()
    connection.close()
    binding = {
        "revision": 7,
        "enabled": True,
        "chat_guid": "fixture-chat",
        "owner_handle": "owner@example.invalid",
        "self_chat": False,
        "cursor": 0,
        "activated_at_ms": 0,
    }
    try:
        owner_events.capture(database, binding)
        observed = "capture unexpectedly succeeded"
    except RuntimeError as error:
        observed = str(error)
print(json.dumps([observed]))
