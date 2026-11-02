#!/usr/bin/env python3
"""Controller cache allocation inside an already leased CI cache filesystem.

The privileged guest controller calls this after validating inherited storage
admission and source inputs. Neither a directory nor this preparation record is
a passing CI/freshness proof. Compilation, input sealing, trusted dependency
seeds and final proof publication remain separate obligations of that caller.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import tempfile
import uuid


FORMAT_VERSION = 1
RECOVERY = {
    "legacy_format": "await_compatible_controller",
    "metadata_missing": "rebuild_source_artifacts",
    "metadata_torn": "rebuild_source_artifacts",
    "generation_mismatch": "rebuild_source_artifacts",
    "input_census_unknown": "revalidate_inputs",
    "artifact_provenance_unknown": "restore_trusted_dependencies",
    "authority_changed": "await_compatible_controller",
    "invalidation_unsafe": "await_verified_cleanup",
    "cleanup_uncertain": "await_verified_cleanup",
    "freshness_unknown": "await_compatible_controller",
}


class FreshnessHold(Exception):
    def __init__(self, reason, evidence=b""):
        self.reason = reason if reason in RECOVERY else "freshness_unknown"
        self.recovery = RECOVERY[self.reason]
        self.evidence_sha256 = hashlib.sha256(evidence).hexdigest()
        super().__init__("shared CI prerequisite unavailable: freshness " + self.reason)

    def diagnostic(self):
        return {"version": 1, "reason": self.reason, "recovery": self.recovery,
                "evidence_sha256": self.evidence_sha256}


def validate_hold(value):
    if (isinstance(value, dict) and set(value) == {"version", "reason", "recovery", "evidence_sha256"}
            and type(value["version"]) is int and value["version"] == 1
            and isinstance(value["reason"], str) and value["reason"] in RECOVERY
            and value["recovery"] == RECOVERY[value["reason"]] and _digest(value["evidence_sha256"])):
        return value
    return FreshnessHold("freshness_unknown").diagnostic()


def _unique_object(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError("duplicate metadata key")
        value[key] = item
    return value


def _uuid(value):
    try:
        return isinstance(value, str) and str(uuid.UUID(value, version=4)) == value
    except (ValueError, AttributeError):
        return False


def _digest(value):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value) is not None


def validate_binding(binding):
    keys = {"generation", "head", "tree", "contract_sha256", "authority_sha256",
            "cache_identity_sha256", "source_inputs_sha256"}
    if not isinstance(binding, dict) or set(binding) != keys or not _uuid(binding["generation"]):
        raise FreshnessHold("generation_mismatch")
    if any(not isinstance(binding[key], str) or re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", binding[key]) is None
           for key in ("head", "tree")):
        raise FreshnessHold("input_census_unknown")
    if any(not _digest(binding[key]) for key in keys - {"generation", "head", "tree"}):
        raise FreshnessHold("input_census_unknown")


def source_census(workspace, objects, head, *, generated_directories=()):
    """Verify actual exported bytes against immutable Git, with no hooks/filters.

    Generated directories here are empty controller-created mount points. Their
    eventual guest contents need a separate generated-input provenance proof.
    """
    workspace, objects = Path(workspace), Path(objects)
    if any(not path.is_absolute() or path.resolve() != path or not path.is_dir()
           for path in (workspace, objects)):
        raise FreshnessHold("input_census_unknown")
    if not isinstance(head, str) or re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", head) is None:
        raise FreshnessHold("input_census_unknown")
    generated = set()
    for value in generated_directories:
        if (not isinstance(value, str) or value.startswith("/") or "\\" in value
                or any(part in ("", ".", "..", ".git") for part in value.split("/"))):
            raise FreshnessHold("input_census_unknown")
        generated.add(value)
    result = subprocess.run([
        "/usr/bin/git", "--no-replace-objects", "-c", "core.hooksPath=/dev/null",
        "-c", "core.fsmonitor=false", "-c", "safe.directory=" + str(workspace),
        "-C", str(workspace), "ls-tree", "-rz", "--full-tree", head,
    ], env={"PATH": os.defpath, "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES": str(objects)},
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=30)
    if result.returncode or len(result.stdout) > 16 * 1024 * 1024:
        raise FreshnessHold("input_census_unknown")
    files, parents, census, total = set(), set(), hashlib.sha256(), 0
    for row in result.stdout.split(b"\0"):
        if not row:
            continue
        try:
            metadata, raw_name = row.split(b"\t", 1)
            mode, kind, oid = metadata.split()
            name = raw_name.decode("utf-8")
        except (ValueError, UnicodeError):
            raise FreshnessHold("input_census_unknown") from None
        relative = Path(name)
        if (mode not in (b"100644", b"100755") or kind != b"blob" or relative.is_absolute()
                or any(part in (".", "..", ".git") for part in relative.parts)
                or any(name == prefix or name.startswith(prefix + "/") for prefix in generated)):
            raise FreshnessHold("input_census_unknown")
        path = workspace / relative
        if path.resolve() != path:
            raise FreshnessHold("input_census_unknown")
        try:
            fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
            with os.fdopen(fd, "rb") as stream:
                info = os.fstat(stream.fileno())
                if (not stat.S_ISREG(info.st_mode) or info.st_nlink != 1
                        or bool(info.st_mode & 0o111) != (mode == b"100755")):
                    raise FreshnessHold("input_census_unknown")
                total += info.st_size
                if total > 8 * 1024 ** 3 or len(files) >= 100_000:
                    raise FreshnessHold("input_census_unknown")
                blob = hashlib.new("sha1" if len(oid) == 40 else "sha256")
                blob.update(b"blob " + str(info.st_size).encode() + b"\0")
                while block := stream.read(1024 * 1024):
                    blob.update(block)
                if blob.hexdigest().encode() != oid:
                    raise FreshnessHold("input_census_unknown")
        except OSError:
            raise FreshnessHold("input_census_unknown") from None
        files.add(name)
        parents.update(str(parent) for parent in relative.parents if str(parent) != ".")
        census.update(row + b"\0")
    for base, directories, filenames in os.walk(workspace, followlinks=False):
        for directory in list(directories):
            path = Path(base) / directory
            name = str(path.relative_to(workspace))
            if name == ".git":
                directories.remove(directory)
            elif name in generated:
                if path.is_symlink() or any(path.iterdir()):
                    raise FreshnessHold("input_census_unknown")
                directories.remove(directory)
            elif path.is_symlink() or name not in parents:
                raise FreshnessHold("input_census_unknown")
        for filename in filenames:
            if str((Path(base) / filename).relative_to(workspace)) not in files:
                raise FreshnessHold("input_census_unknown")
    return census.hexdigest()


def _directory(path, mode, *, create):
    if create:
        try:
            path.mkdir(mode=mode)
        except FileExistsError:
            pass
    info = path.lstat()
    if (not stat.S_ISDIR(info.st_mode) or stat.S_ISLNK(info.st_mode)
            or info.st_uid != os.geteuid() or info.st_mode & 0o022):
        raise FreshnessHold("invalidation_unsafe")


def _read_record(path):
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    except FileNotFoundError:
        raise FreshnessHold("metadata_missing") from None
    except OSError:
        raise FreshnessHold("metadata_torn") from None
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid()
                or info.st_nlink != 1 or info.st_mode & 0o077 or info.st_size > 8192):
            raise FreshnessHold("metadata_torn")
        data = stream.read(8193)
    try:
        value = json.loads(data, object_pairs_hook=_unique_object)
    except (ValueError, UnicodeError):
        raise FreshnessHold("metadata_torn", data) from None
    if not isinstance(value, dict):
        raise FreshnessHold("metadata_torn", data)
    return value


def _write_record(path, value):
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    fd, temporary = tempfile.mkstemp(prefix=".preparing-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            os.fchmod(stream.fileno(), 0o600)
            stream.write(encoded)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass


def prepare(cache, binding, command, worker_uid, worker_gid, *, predecessor=None):
    """Allocate a fresh target or resume after a controller-acknowledged exit.

    `predecessor` comes from the host's verified VM quiescence for the preceding
    command, never from guest output or candidate files. Omitting it refuses an
    interrupted generation. No existing/unknown data is deleted by this API.
    """
    validate_binding(binding)
    if not _uuid(command) or (predecessor is not None and not _uuid(predecessor)):
        raise FreshnessHold("cleanup_uncertain")
    if any(type(value) is not int or value <= 0 for value in (worker_uid, worker_gid)):
        raise FreshnessHold("invalidation_unsafe")
    cache = Path(cache)
    if not cache.is_absolute() or cache.resolve() != cache:
        raise FreshnessHold("invalidation_unsafe")
    _directory(cache, 0o700, create=False)
    root = cache / "ci-source-generations"
    is_new = not root.exists() and not root.is_symlink()
    if is_new and (cache / "target").exists():
        raise FreshnessHold("legacy_format")
    _directory(root, 0o711, create=True)
    format_path = root / "format.json"
    expected = {"version": FORMAT_VERSION, "authority_sha256": binding["authority_sha256"],
                "cache_identity_sha256": binding["cache_identity_sha256"]}
    if is_new:
        _write_record(format_path, expected)
    else:
        actual = _read_record(format_path)
        if actual.get("version") != FORMAT_VERSION:
            raise FreshnessHold("legacy_format")
        if actual != expected:
            raise FreshnessHold("authority_changed")
    generation = root / binding["generation"]
    is_new = not generation.exists() and not generation.is_symlink()
    if is_new and predecessor is not None:
        raise FreshnessHold("generation_mismatch")
    _directory(generation, 0o711, create=True)
    record = generation / "control.json"
    target = generation / "target"
    if is_new:
        target.mkdir(mode=0o700)
        os.chown(target, worker_uid, worker_gid)
    else:
        prior = _read_record(record)
        if set(prior) != {"version", "binding", "active_command"} or prior.get("version") != FORMAT_VERSION:
            raise FreshnessHold("metadata_torn")
        if prior["binding"] != binding:
            raise FreshnessHold("generation_mismatch")
        if not _uuid(prior["active_command"]) or predecessor != prior["active_command"] or command == predecessor:
            raise FreshnessHold("cleanup_uncertain")
        info = target.lstat()
        if (not stat.S_ISDIR(info.st_mode) or stat.S_ISLNK(info.st_mode)
                or info.st_uid != worker_uid or info.st_gid != worker_gid or info.st_mode & 0o077):
            raise FreshnessHold("invalidation_unsafe")
    _write_record(record, {"version": FORMAT_VERSION, "binding": binding, "active_command": command})
    return target
