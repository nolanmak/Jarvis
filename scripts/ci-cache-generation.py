#!/usr/bin/env python3
"""Controller cache allocation inside an already leased CI cache filesystem.

The privileged guest controller calls this after validating inherited storage
admission and source inputs. Neither a directory nor this preparation record is
a passing CI/freshness proof. Compilation, input sealing, trusted dependency
seeds and final proof publication remain separate obligations of that caller.
"""
import hashlib
import errno
import configparser
import json
import os
from pathlib import Path
import re
import shutil
import selectors
import signal
import stat
import subprocess
import sys
import tempfile
import time
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


def require_cache_capacity(directory):
    """Refuse an already-full cache before worker dispatch, without cleanup.

    Reserved root blocks are unavailable to the unprivileged worker. This is
    only a zero-capacity check; it never proves the admitted plan fits or changes
    any reservation, cache limit, retention decision or reusable-artifact proof.
    """
    info=os.statvfs(directory)
    fields=(info.f_frsize,info.f_blocks,info.f_bavail,info.f_files,info.f_favail)
    if (any(type(value) is not int or value<0 for value in fields)
            or info.f_frsize==0 or info.f_blocks==0):raise FreshnessHold('invalidation_unsafe')
    if info.f_bavail==0 or info.f_files>0 and info.f_favail==0:
        raise OSError(errno.ENOSPC,'CI cache has no worker-available blocks or inodes')


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


def validate_outputs(outputs):
    if not isinstance(outputs, dict) or len(outputs) > 64 or any(not isinstance(name, str) for name in outputs):
        raise FreshnessHold("input_census_unknown")
    for name, kind in outputs.items():
        if (not isinstance(name, str) or name.startswith("/") or "\\" in name
                or len(name) > 512 or kind not in ("directory", "file")
                or any(part in ("", ".", "..", ".git") for part in name.split("/"))
                or any(name.startswith(other + "/") for other in outputs if other != name)):
            raise FreshnessHold("input_census_unknown")


def declared_outputs(workspace):
    """Pinned output namespaces for the existing declared Node CI commands.

    This policy is part of the compiled controller authority. A package cannot
    request another writable source path through its own configuration.
    """
    workspace = Path(workspace)
    outputs = {}
    for relative in ("", "sidecars/computer-use", "sidecars/discord-voice", "sidecars/fetch", "sidecars/renderer"):
        package = workspace / relative / "package.json"
        if package.is_file():
            for name in ("node_modules", "dist"):
                outputs[str(Path(relative) / name)] = "directory"
    if (workspace / "package.json").is_file() and (workspace / "public").is_dir():
        outputs["public/tailwind.css"] = "file"
    validate_outputs(outputs)
    return outputs


def source_census(workspace, objects, head, *, generated_directories=(), generated_files=()):
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
    output_files = set(generated_files)
    validate_outputs({**{name: "directory" for name in generated_directories},
                      **{name: "file" for name in generated_files}})
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
                or name in output_files
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
            path = Path(base) / filename
            name = str(path.relative_to(workspace))
            if name in output_files:
                info = path.lstat()
                if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size != 0:
                    raise FreshnessHold("input_census_unknown")
            elif name not in files:
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


def _read_record(path, *, max_bytes=8192):
    if type(max_bytes) is not int or not 0<max_bytes<=16*1024**2:
        raise FreshnessHold("metadata_torn")
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    except FileNotFoundError:
        raise FreshnessHold("metadata_missing") from None
    except OSError:
        raise FreshnessHold("metadata_torn") from None
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid()
                or info.st_nlink != 1 or info.st_mode & 0o077 or info.st_size > max_bytes):
            raise FreshnessHold("metadata_torn")
        data = stream.read(max_bytes+1)
        if len(data)>max_bytes:raise FreshnessHold('metadata_torn')
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


def seed_layout(generation, binding):
    """Only a privileged, binding-specific installation may seal job roots."""
    path = Path(generation) / "dependency-layout.json"
    if not path.exists() and not path.is_symlink():
        return None
    value = _read_record(path)
    if (set(value) != {"version", "binding", "seed", "state"}
            or type(value["version"]) is not int or value["version"] != 1
            or value["binding"] != binding or not _digest(value["seed"])
            or value["state"] not in ("installing", "ready")):
        raise FreshnessHold("artifact_provenance_unknown")
    return value


def begin_seed_install(target, binding, seed):
    target = Path(target)
    validate_binding(binding)
    if target.name != "target" or target.resolve() != target or not _digest(seed):
        raise FreshnessHold("invalidation_unsafe")
    control = _read_record(target.parent / "control.json")
    if control.get("binding") != binding:
        raise FreshnessHold("generation_mismatch")
    prior = seed_layout(target.parent, binding)
    if prior is not None and prior["seed"] != seed:
        raise FreshnessHold("artifact_provenance_unknown")
    _write_record(target.parent / "dependency-layout.json",
                  {"version": 1, "binding": binding, "seed": seed, "state": "installing"})


def finish_seed_install(target, binding, seed):
    target = Path(target)
    prior = seed_layout(target.parent, binding)
    if prior is None or prior["seed"] != seed or prior["state"] != "installing":
        raise FreshnessHold("artifact_provenance_unknown")
    _write_record(target.parent / "dependency-layout.json", prior | {"state": "ready"})


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
    retirement = cache / "ci-generation-retirements" / (binding["generation"] + ".json")
    if retirement.exists() or retirement.is_symlink():
        raise FreshnessHold("generation_mismatch")
    root = cache / "ci-source-generations"
    # This parent is only an allocation namespace, never a reusable-cache
    # certificate. A legacy target or old/torn parent format receipt cannot
    # authorize any artifact, and must not prevent the next admitted attempt
    # from constructing a fresh, empty generation. Every reusable target is
    # instead fenced by its own controller-owned versioned binding below.
    # Retain old bytes/receipts here; reclamation requires the storage owner's
    # checked quiescence and budget policy, not a metadata-triggered deletion.
    _directory(root, 0o711, create=True)
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
        if (set(prior) != {"version", "binding", "active_command"}
                or type(prior.get("version")) is not int or prior["version"] != FORMAT_VERSION):
            raise FreshnessHold("metadata_torn")
        if prior["binding"] != binding:
            raise FreshnessHold("generation_mismatch")
        if not _uuid(prior["active_command"]) or predecessor != prior["active_command"] or command == predecessor:
            raise FreshnessHold("cleanup_uncertain")
        layout = seed_layout(generation, binding)
        if layout is not None and layout["state"] != "ready":
            raise FreshnessHold("artifact_provenance_unknown")
        info = target.lstat()
        owner = os.geteuid() if layout else worker_uid
        group = os.getegid() if layout else worker_gid
        mode = 0o1777 if layout else 0o700
        if (not stat.S_ISDIR(info.st_mode) or stat.S_ISLNK(info.st_mode)
                or info.st_uid != owner or info.st_gid != group or stat.S_IMODE(info.st_mode) != mode):
            raise FreshnessHold("invalidation_unsafe")
    _write_record(record, {"version": FORMAT_VERSION, "binding": binding, "active_command": command})
    return target


def preparation_receipt(target, binding, command, outputs):
    """Guest-root preparation only; never a compiled-input or passing-CI proof."""
    validate_binding(binding)
    validate_outputs(outputs)
    target = Path(target)
    record = _read_record(target.parent / "control.json")
    if (type(record.get("version")) is not int
            or record != {"version": FORMAT_VERSION, "binding": binding, "active_command": command} or not _uuid(command)):
        raise FreshnessHold("generation_mismatch")
    layout = seed_layout(target.parent, binding)
    if layout is not None and layout["state"] != "ready":
        raise FreshnessHold("artifact_provenance_unknown")
    return {"version": 1, "binding": dict(binding), "command": command,
            "outputs_sha256": hashlib.sha256(json.dumps(outputs, sort_keys=True, separators=(",", ":")).encode()).hexdigest(),
            "dependency_seed": layout["seed"] if layout else None}


def validate_preparation(value, binding, command, outputs):
    validate_binding(binding)
    validate_outputs(outputs)
    if value is None:
        raise FreshnessHold("legacy_format")
    if (not isinstance(value, dict) or set(value) != {"version", "binding", "command", "outputs_sha256", "dependency_seed"}
            or type(value["version"]) is not int or value["version"] != 1
            or value["binding"] != binding or value["command"] != command or not _uuid(command)
            or value["outputs_sha256"] != hashlib.sha256(json.dumps(outputs, sort_keys=True, separators=(",", ":")).encode()).hexdigest()):
        raise FreshnessHold("generation_mismatch")
    if value["dependency_seed"] is not None and not _digest(value["dependency_seed"]):
        raise FreshnessHold("artifact_provenance_unknown")
    return value


def prepare_outputs(target, outputs, worker_uid, worker_gid):
    """Allocate declared output mounts; they confer no generated-input proof.

    Caller holds the existing exclusive image admission and verifies VM exit
    before resuming a generation. The original source filesystem stays read-only.
    """
    validate_outputs(outputs)
    target = Path(target)
    if target.name != "target" or target.resolve() != target or not target.is_dir():
        raise FreshnessHold("invalidation_unsafe")
    if any(type(value) is not int or value <= 0 for value in (worker_uid, worker_gid)):
        raise FreshnessHold("invalidation_unsafe")
    _directory(target.parent, 0o711, create=False)
    root = target.parent / "outputs"
    _directory(root, 0o711, create=True)
    mounts = {}
    for name, kind in sorted(outputs.items()):
        path = root / name
        parent = root
        for part in Path(name).parts[:-1]:
            parent = parent / part
            _directory(parent, 0o711, create=True)
        try:
            if kind == "directory":
                path.mkdir(mode=0o700)
            else:
                fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
                os.close(fd)
            os.chown(path, worker_uid, worker_gid, follow_symlinks=False)
        except FileExistsError:
            pass
        info = path.lstat()
        correct_kind = stat.S_ISDIR(info.st_mode) if kind == "directory" else stat.S_ISREG(info.st_mode)
        if (not correct_kind or info.st_uid != worker_uid or info.st_gid != worker_gid
                or info.st_mode & 0o077 or (kind == "file" and info.st_nlink != 1)):
            raise FreshnessHold("invalidation_unsafe")
        mounts[name] = path
    return mounts


_PROJECTION_DOCUMENT_BYTES=16*1024**2
_PROJECTION_MAX_FILES=110000
_PROJECTION_MAX_BYTES=8*1024**3


def _projection_git(root,arguments,*,objects=None):
    environment={'PATH':os.defpath,'GIT_CONFIG_NOSYSTEM':'1','GIT_CONFIG_GLOBAL':'/dev/null',
        'GIT_TERMINAL_PROMPT':'0','GIT_OPTIONAL_LOCKS':'0'}
    if objects is not None:environment['GIT_ALTERNATE_OBJECT_DIRECTORIES']=str(objects)
    command=['/usr/bin/git','-c','core.hooksPath=/dev/null','-c','core.fsmonitor=false',
        '-c','core.logAllRefUpdates=false','-c','safe.directory='+str(root),'-C',str(root),*arguments]
    # Set umask in a fresh interpreter, never in this multithreaded controller
    # or a Python preexec callback. Git's staging files stay controller-private.
    launcher=[sys.executable,'-I','-c',
        "import os,sys;os.umask(0o077);os.execve('/usr/bin/git',sys.argv[1:],os.environ)"]
    result=subprocess.run([*launcher,*command],env=environment,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,timeout=30)
    if result.returncode or len(result.stdout)>16*1024**2:raise FreshnessHold('input_census_unknown')
    return result.stdout


def _projection_source_config(workspace,objects,head):
    # The host exporter creates a minimal Git database. Refuse other local
    # configuration before asking privileged Git to parse the supplied repo.
    path=workspace/'.git/config'
    if path.resolve()!=path:raise FreshnessHold('input_census_unknown')
    descriptor=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK|os.O_CLOEXEC)
    with os.fdopen(descriptor,'rb') as stream:
        info=os.fstat(stream.fileno());data=stream.read(8193)
    if not stat.S_ISREG(info.st_mode) or info.st_nlink!=1 or len(data)>8192:
        raise FreshnessHold('input_census_unknown')
    config=configparser.RawConfigParser(strict=True)
    try:config.read_string(data.decode('ascii'))
    except (ValueError,UnicodeError,configparser.Error):raise FreshnessHold('input_census_unknown') from None
    allowed={'core':{'repositoryformatversion':('0','1'),'filemode':('true','false'),
        'bare':('false',),'logallrefupdates':('true','false'),'ignorecase':('true','false'),
        'precomposeunicode':('true','false')},'extensions':{'objectformat':('sha256',)}}
    if config.defaults() or 'core' not in config:raise FreshnessHold('input_census_unknown')
    for section in config.sections():
        if section not in allowed or any(key not in allowed[section] or value.lower() not in allowed[section][key]
                for key,value in config[section].items()):raise FreshnessHold('input_census_unknown')
    if config.get('extensions','objectformat',fallback='sha1')!=('sha256' if len(head)==64 else 'sha1'):
        raise FreshnessHold('input_census_unknown')
    alternate=workspace/'.git/objects/info/alternates'
    if alternate.exists() or alternate.is_symlink():
        if alternate.resolve()!=alternate:raise FreshnessHold('input_census_unknown')
        descriptor=os.open(alternate,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK|os.O_CLOEXEC)
        with os.fdopen(descriptor,'rb') as stream:
            if stream.read(8193)!=(str(objects)+'\n').encode():raise FreshnessHold('input_census_unknown')


def _projection_inventory(root,*,sealed):
    root=Path(root);entries=[];total=0
    if root.resolve()!=root:raise FreshnessHold('invalidation_unsafe')
    device=root.lstat().st_dev
    def failed(error):raise error
    for base,dirs,files in os.walk(root,followlinks=False,onerror=failed):
        directory=Path(base);info=directory.lstat();relative=directory.relative_to(root).as_posix()
        if (not stat.S_ISDIR(info.st_mode) or info.st_uid!=os.geteuid() or info.st_dev!=device
                or info.st_mode&0o022 or sealed and stat.S_IMODE(info.st_mode)!=0o555):
            raise FreshnessHold('invalidation_unsafe')
        entries.append([relative,'directory',stat.S_IMODE(info.st_mode)])
        for name in sorted(dirs):
            if (directory/name).is_symlink():raise FreshnessHold('invalidation_unsafe')
        for name in sorted(files):
            path=directory/name;info=path.lstat();relative=path.relative_to(root).as_posix()
            if (not stat.S_ISREG(info.st_mode) or info.st_uid!=os.geteuid() or info.st_nlink!=1
                    or info.st_dev!=device or info.st_mode&0o022
                    or sealed and stat.S_IMODE(info.st_mode) not in (0o444,0o555)):
                raise FreshnessHold('invalidation_unsafe')
            fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC|os.O_NONBLOCK|getattr(os,'O_NOATIME',0))
            with os.fdopen(fd,'rb') as stream:
                before=os.fstat(stream.fileno());digest=hashlib.sha256();read=0
                while block:=stream.read(1024*1024):
                    read+=len(block)
                    if read>info.st_size:raise FreshnessHold('input_census_unknown')
                    digest.update(block)
                after=os.fstat(stream.fileno())
            identity=lambda s:(s.st_dev,s.st_ino,s.st_mode,s.st_nlink,s.st_size,s.st_mtime_ns,s.st_ctime_ns)
            if identity(info)!=identity(before) or identity(before)!=identity(after) or read!=info.st_size:
                raise FreshnessHold('input_census_unknown')
            total+=read
            if total>_PROJECTION_MAX_BYTES:raise FreshnessHold('input_census_unknown')
            entries.append([relative,'file',stat.S_IMODE(info.st_mode),read,digest.hexdigest()])
        if len(entries)>_PROJECTION_MAX_FILES or any(len(row[0].encode())>4096 for row in entries[-len(files)-1:]):
            raise FreshnessHold('input_census_unknown')
    return sorted(entries)


def _seal_source_projection(root):
    # The stage is private until every file/ancestor is immutable and durable.
    for base,dirs,files in os.walk(root,topdown=False,followlinks=False):
        directory=Path(base)
        for name in files:
            path=directory/name;info=path.lstat()
            if not stat.S_ISREG(info.st_mode) or info.st_uid!=os.geteuid() or info.st_nlink!=1:
                raise FreshnessHold('invalidation_unsafe')
            path.chmod(0o555 if info.st_mode&0o111 else 0o444)
            fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC)
            try:os.fsync(fd)
            finally:os.close(fd)
        directory.chmod(0o555)
        fd=os.open(directory,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC)
        try:os.fsync(fd)
        finally:os.close(fd)
    fd=os.open(root.parent,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
    try:os.fsync(fd)
    finally:os.close(fd)


def _projection_pack(source,objects,destination,revisions,remaining):
    environment={'PATH':os.defpath,'GIT_CONFIG_NOSYSTEM':'1','GIT_CONFIG_GLOBAL':'/dev/null',
        'GIT_ALTERNATE_OBJECT_DIRECTORIES':str(objects),'GIT_TERMINAL_PROMPT':'0'}
    process=subprocess.Popen(['/usr/bin/git','-c','core.hooksPath=/dev/null','-c','core.fsmonitor=false',
        '-c','safe.directory='+str(source),'-C',str(source),'pack-objects','--revs','--stdout'],env=environment,stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,start_new_session=True)
    try:
        process.stdin.write(('\n'.join(revisions)+'\n').encode());process.stdin.close()
        deadline=time.monotonic()+30;written=0
        with destination.open('xb') as stream,selectors.DefaultSelector() as selector:
            os.chmod(destination,0o600);selector.register(process.stdout,selectors.EVENT_READ)
            while True:
                if time.monotonic()>deadline:raise FreshnessHold('input_census_unknown')
                if not selector.select(1):continue
                block=os.read(process.stdout.fileno(),1024*1024)
                if not block:break
                written+=len(block)
                if written>remaining:raise FreshnessHold('input_census_unknown')
                stream.write(block)
            stream.flush();os.fsync(stream.fileno())
        if process.wait(timeout=5):raise FreshnessHold('input_census_unknown')
    finally:
        if process.poll() is None:
            try:os.killpg(process.pid,signal.SIGKILL)
            except ProcessLookupError:pass
            process.wait(timeout=5)
        if process.stdin is not None and not process.stdin.closed:process.stdin.close()
        process.stdout.close()


def _projection_manifest(value,binding):
    keys={'version','binding','command','state','entries','shipping_authority'}
    if (not isinstance(value,dict) or set(value)!=keys or type(value['version']) is not int
            or value['version']!=FORMAT_VERSION or value['binding']!=binding or not _uuid(value['command'])
            or value['shipping_authority'] is not False or value['state'] not in ('preparing','sealed')):
        raise FreshnessHold('metadata_torn')
    entries=value['entries']
    if value['state']=='preparing':
        if entries is not None:raise FreshnessHold('metadata_torn')
        return value
    if not isinstance(entries,list) or not 0<len(entries)<=_PROJECTION_MAX_FILES:
        raise FreshnessHold('metadata_torn')
    seen=set();total=0
    for row in entries:
        if not isinstance(row,list) or len(row)<3 or not isinstance(row[0],str):raise FreshnessHold('metadata_torn')
        name=row[0]
        if (len(name.encode())>4096 or name in seen or name!='.' and
                (name.startswith('/') or any(part in ('','.','..') for part in name.split('/')))):
            raise FreshnessHold('metadata_torn')
        seen.add(name)
        if row[1]=='directory' and len(row)==3 and type(row[2]) is int and row[2]==0o555:continue
        if (row[1]!='file' or len(row)!=5 or type(row[2]) is not int or row[2] not in (0o444,0o555)
                or type(row[3]) is not int or row[3]<0 or not _digest(row[4])):raise FreshnessHold('metadata_torn')
        total+=row[3]
    if total>_PROJECTION_MAX_BYTES or ['.','directory',0o555] not in entries:raise FreshnessHold('metadata_torn')
    kinds={row[0]:row[1] for row in entries}
    if any(kinds.get(Path(name).parent.as_posix())!='directory' for name in seen if name!='.'):
        raise FreshnessHold('metadata_torn')
    return value


def prepare_source_projection(target,workspace,objects,binding,command,outputs):
    """Materialize read-only Git bytes inside the original owned generation.

    Only the admitted controller calls this, before writable output mounts.
    Its own UID owns the copy; privileged activation additionally requires root
    ownership and the protected planner protocol. This grants no CI proof.
    """
    validate_binding(binding);validate_outputs(outputs)
    target,workspace,objects=Path(target),Path(workspace),Path(objects)
    if (target.name!='target' or target.resolve()!=target or target.parent.name!=binding['generation']
            or _read_record(target.parent/'control.json')!={'version':FORMAT_VERSION,'binding':binding,'active_command':command}):
        raise FreshnessHold('generation_mismatch')
    _directory(target.parent,0o711,create=False)
    retirement=target.parent.parent.parent/'ci-generation-retirements'/(binding['generation']+'.json')
    if retirement.exists() or retirement.is_symlink():raise FreshnessHold('generation_mismatch')
    record=target.parent/'source-projection.json';projection=target.parent/'source-projection'
    if record.exists() or record.is_symlink():
        value=_projection_manifest(_read_record(record,max_bytes=_PROJECTION_DOCUMENT_BYTES),binding)
        if value['state']!='sealed':raise FreshnessHold('cleanup_uncertain')
        if _projection_inventory(projection,sealed=True)!=value['entries']:raise FreshnessHold('input_census_unknown')
        return projection
    if projection.exists() or projection.is_symlink():raise FreshnessHold('metadata_missing')
    directories=['target',*[name for name,kind in outputs.items() if kind=='directory']]
    files=[name for name,kind in outputs.items() if kind=='file']
    _projection_source_config(workspace,objects,binding['head'])
    if source_census(workspace,objects,binding['head'],generated_directories=directories,
            generated_files=files)!=binding['source_inputs_sha256']:
        raise FreshnessHold('input_census_unknown')
    if _projection_git(workspace,['rev-parse',binding['head']+'^{tree}'],objects=objects).decode().strip()!=binding['tree']:
        raise FreshnessHold('input_census_unknown')
    refs=[]
    for line in _projection_git(workspace,['for-each-ref','--format=%(refname) %(objectname)'],objects=objects).splitlines():
        fields=line.decode().split(' ')
        if len(fields)!=2 or len(fields[1])!=len(binding['head']) or any(c not in '0123456789abcdef' for c in fields[1]):
            raise FreshnessHold('input_census_unknown')
        refs.append(fields)
    if len(refs)>64:raise FreshnessHold('input_census_unknown')
    value={'version':FORMAT_VERSION,'binding':binding,'command':command,'state':'preparing',
        'entries':None,'shipping_authority':False}
    _write_record(record,value);projection.mkdir(mode=0o700)
    total=count=0
    for base,dirs,names in os.walk(workspace,followlinks=False):
        directory=Path(base);relative=directory.relative_to(workspace)
        if relative==Path('.'):dirs[:]=[name for name in dirs if name!='.git']
        destination=projection/relative;destination.mkdir(mode=0o700,exist_ok=True)
        for name in dirs:
            if (directory/name).is_symlink():raise FreshnessHold('input_census_unknown')
        for name in names:
            source=directory/name;info=source.lstat()
            if not stat.S_ISREG(info.st_mode) or info.st_nlink!=1:raise FreshnessHold('input_census_unknown')
            count+=1;total+=info.st_size
            if count>100000 or total>_PROJECTION_MAX_BYTES:raise FreshnessHold('input_census_unknown')
            fd=os.open(source,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK|os.O_CLOEXEC)
            with os.fdopen(fd,'rb') as reader,(destination/name).open('xb') as writer:
                copied=0
                while block:=reader.read(1024*1024):
                    copied+=len(block)
                    if copied>info.st_size:raise FreshnessHold('input_census_unknown')
                    writer.write(block)
                if copied!=info.st_size:raise FreshnessHold('input_census_unknown')
                writer.flush();os.fsync(writer.fileno())
            (destination/name).chmod(0o700 if info.st_mode&0o111 else 0o600)
            os.utime(destination/name,ns=(info.st_atime_ns,info.st_mtime_ns))
    (projection/'target').mkdir(mode=0o700,exist_ok=True)
    _projection_git(projection,['-c','init.templateDir=','init','-q',
        '--object-format='+('sha256' if len(binding['head'])==64 else 'sha1')])
    shallow=workspace/'.git/shallow'
    if shallow.exists():
        fd=os.open(shallow,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK|os.O_CLOEXEC)
        with os.fdopen(fd,'rb') as stream:data=stream.read(1024*1024+1)
        if len(data)>1024*1024 or any(len(row)!=len(binding['head']) or any(c not in b'0123456789abcdef' for c in row) for row in data.splitlines()):
            raise FreshnessHold('input_census_unknown')
        (projection/'.git/shallow').write_bytes(data)
    pack=projection/'.git/objects/pack/staging.pack';pack.parent.mkdir(exist_ok=True)
    _projection_pack(workspace,objects,pack,[binding['head'],*[oid for _name,oid in refs]],_PROJECTION_MAX_BYTES-total)
    index=pack.with_suffix('.idx')
    identity=_projection_git(projection,['index-pack','-o',str(index),str(pack)]).decode().strip()
    if len(identity)!=len(binding['head']) or any(c not in '0123456789abcdef' for c in identity):raise FreshnessHold('input_census_unknown')
    pack.rename(pack.parent/('pack-'+identity+'.pack'));index.rename(pack.parent/('pack-'+identity+'.idx'))
    (projection/'.git/HEAD').write_text(binding['head']+'\n')
    for name,oid in refs:_projection_git(projection,['update-ref',name,oid])
    _projection_git(projection,['read-tree',binding['head']])
    if source_census(projection,projection/'.git/objects',binding['head'],generated_directories=directories,
            generated_files=files)!=binding['source_inputs_sha256']:
        raise FreshnessHold('input_census_unknown')
    _seal_source_projection(projection)
    value.update(state='sealed',entries=_projection_inventory(projection,sealed=True))
    if len(json.dumps(value).encode())>_PROJECTION_DOCUMENT_BYTES-8192:raise FreshnessHold('input_census_unknown')
    _write_record(record,value)
    return projection


def _generation_retirement(value):
    fields={'version','state','original'}
    if (not isinstance(value,dict) or set(value) not in (fields,fields|{'source_projection'})
            or type(value['version']) is not int or value['version']!=1
            or value['state'] not in ('reclaiming','reclaimed')):raise FreshnessHold('cleanup_uncertain')
    original=value['original']
    if (not isinstance(original,dict) or set(original)!={'version','binding','active_command'}
            or type(original['version']) is not int or original['version']!=FORMAT_VERSION
            or not _uuid(original['active_command'])):raise FreshnessHold('metadata_torn')
    validate_binding(original['binding'])
    if 'source_projection' in value:_projection_manifest(value['source_projection'],original['binding'])
    return original


def source_projection_observation(target,binding,command,access):
    target=Path(target)
    if _read_record(target.parent/'control.json')!={'version':FORMAT_VERSION,'binding':binding,'active_command':command}:
        raise FreshnessHold('generation_mismatch')
    value=_projection_manifest(_read_record(target.parent/'source-projection.json',
        max_bytes=_PROJECTION_DOCUMENT_BYTES),binding)
    if value['state']!='sealed':raise FreshnessHold('cleanup_uncertain')
    return {'version':1,'kind':'ci-git-source-projection','binding':binding,'command':command,
        'manifest_sha256':hashlib.sha256(json.dumps(value,sort_keys=True,separators=(',',':')).encode()).hexdigest(),
        'owner_uid':(target.parent/'source-projection').lstat().st_uid,'access':access,
        'input_closure_complete':False,'shipping_authority':False}


def validate_source_projection_observation(value,binding,command,workers,inputs):
    """Validate transport/probe correlation, never a complete compiler proof."""
    fields={'version','kind','binding','command','manifest_sha256','owner_uid','access',
        'input_closure_complete','shipping_authority'}
    try:
        if (not isinstance(value,dict) or set(value)!=fields or type(value['version']) is not int
                or value['version']!=1 or value['kind']!='ci-git-source-projection'
                or value['binding']!=binding or value['command']!=command or not _uuid(command)
                or not _digest(value['manifest_sha256']) or type(value['owner_uid']) is not int
                or value['owner_uid']!=0 or value['input_closure_complete'] is not False
                or value['shipping_authority'] is not False or not isinstance(value['access'],list)
                or len(value['access'])!=2):raise FreshnessHold('input_census_unknown')
        seen=set()
        for item,role in zip(value['access'],(1001,2000)):
            if (not isinstance(item,dict) or set(item)!={'uid','scope'} or type(item['uid']) is not int
                    or item['uid']!=role or item['scope'] not in workers['scopes']
                    or item['scope']['scope_id'] in seen
                    or item['scope']['scope_id']==inputs['guest_worker_quiescence']['scope_id']):
                raise FreshnessHold('cleanup_uncertain')
            seen.add(item['scope']['scope_id'])
        return value
    except (TypeError,ValueError,KeyError):raise FreshnessHold('input_census_unknown') from None


def _projection_for_retirement(generation,binding,prior=None):
    record=generation/'source-projection.json';root=generation/'source-projection'
    replay=prior is not None
    present=record.exists() or record.is_symlink()
    has_root=root.exists() or root.is_symlink()
    if not present and not has_root and prior is None:return None
    original=_projection_manifest(prior,binding) if replay else None
    if present:
        current=_projection_manifest(_read_record(record,max_bytes=_PROJECTION_DOCUMENT_BYTES),binding)
        if original is not None and current!=original:raise FreshnessHold('cleanup_uncertain')
        original=current
    if original is None:raise FreshnessHold('metadata_missing')
    if has_root:
        observed=_projection_inventory(root,sealed=False)
        if original['state']=='sealed':
            expected={row[0]:row for row in original['entries']}
            for row in observed:
                previous=expected.get(row[0])
                if previous is None:raise FreshnessHold('cleanup_uncertain')
                if row[1]=='directory' and previous[1]=='directory':
                    if row[2] not in ((0o555,0o700) if replay else (0o555,)):raise FreshnessHold('cleanup_uncertain')
                elif row!=previous:raise FreshnessHold('cleanup_uncertain')
            if not replay and observed!=original['entries']:raise FreshnessHold('cleanup_uncertain')
    elif original['state']=='sealed' and not replay:raise FreshnessHold('cleanup_uncertain')
    return original


def _unseal_projection_for_retirement(root):
    if not root.exists():return
    # The original complete inventory and retirement intent were validated and
    # made durable first. Only this controller-owned source tree changes mode.
    for base,dirs,files in os.walk(root,followlinks=False):
        path=Path(base);info=path.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid!=os.geteuid():raise FreshnessHold('cleanup_uncertain')
        path.chmod(0o700)


def reclaim(cache, protected_generations, worker_uid, worker_gid, *, limit=2):
    """Bounded cleanup inside the exclusively admitted, quiescent managed image.

    Only the privileged guest controller calls this, before spawning workers.
    Host storage admission has already reconciled durable cleanup holds and
    verified original scope quiescence. Protected IDs come from the host's
    live CI session, including its current generation. Unknown allocations
    remain intact; this API cannot manufacture cleanup authority for them.
    """
    if (not isinstance(protected_generations, list) or len(protected_generations) > 4096
            or any(not _uuid(value) for value in protected_generations)
            or type(limit) is not int or not 1 <= limit <= 8
            or any(type(value) is not int or value <= 0 for value in (worker_uid, worker_gid))
            or not shutil.rmtree.avoids_symlink_attacks):
        raise FreshnessHold("cleanup_uncertain")
    cache = Path(cache)
    if not cache.is_absolute() or cache.resolve() != cache:
        raise FreshnessHold("invalidation_unsafe")
    _directory(cache, 0o700, create=False)
    root = cache / "ci-source-generations"
    if not root.exists() and not root.is_symlink():
        return []
    _directory(root, 0o711, create=False)
    retired = cache / "ci-generation-retirements"
    _directory(retired, 0o700, create=True)
    # Bound discovery as well as deletion count. Unknown names are never
    # adopted as owned generations. Each deletion preserves its original
    # binding/command record before any artifact can be removed.
    import itertools
    reclaimed = []
    # A crash can remove the directory before committing the completion
    # receipt. Reconcile that exact, already-persisted intent by observing
    # absence under the same owned parent; never infer a reusable generation.
    for receipt_path in itertools.islice(retired.iterdir(), 1024):
        name = receipt_path.stem
        if len(reclaimed) >= limit:
            break
        if receipt_path.suffix != ".json" or not _uuid(name) or name in protected_generations:
            continue
        generation = root / name
        if generation.exists() or generation.is_symlink():
            continue
        try:
            receipt = _read_record(receipt_path,max_bytes=_PROJECTION_DOCUMENT_BYTES)
            if receipt.get('state')!='reclaiming':
                continue
            original = _generation_retirement(receipt)
            if original["binding"]["generation"] != name:
                continue
        except (FreshnessHold, OSError, KeyError, TypeError, ValueError):
            continue
        _write_record(receipt_path, {**receipt,"state":"reclaimed"})
        reclaimed.append(name)
    candidates = sorted(itertools.islice(root.iterdir(), 1024))
    for generation in candidates:
        if len(reclaimed) >= limit:
            break
        if not _uuid(generation.name) or generation.name in protected_generations:
            continue
        receipt_path = retired / (generation.name + ".json")
        try:
            _directory(generation, 0o711, create=False)
            receipt = _read_record(receipt_path,max_bytes=_PROJECTION_DOCUMENT_BYTES) if receipt_path.exists() or receipt_path.is_symlink() else None
            if receipt is not None:
                _generation_retirement(receipt)
                if receipt['state']!='reclaiming':raise FreshnessHold('cleanup_uncertain')
            try:
                original = _read_record(generation / "control.json")
            except FreshnessHold as error:
                if error.reason != "metadata_missing" or receipt is None:
                    raise
                original = receipt["original"]
            if (not isinstance(original, dict) or set(original) != {"version", "binding", "active_command"}
                    or type(original["version"]) is not int or original["version"] != FORMAT_VERSION
                    or not _uuid(original["active_command"])):
                raise FreshnessHold("metadata_torn")
            validate_binding(original["binding"])
            if original["binding"]["generation"] != generation.name or (receipt is not None and receipt["original"] != original):
                raise FreshnessHold("generation_mismatch")
            children = {child.name for child in generation.iterdir()}
            layout = seed_layout(generation, original["binding"])
            if children - {"control.json", "target", "cargo-home", "outputs", "dependency-layout.json", "cargo-home-seeded.json",
                    "source-projection","source-projection.json"}:
                raise FreshnessHold("invalidation_unsafe")
            if "cargo-home-seeded.json" in children and _read_record(generation / "cargo-home-seeded.json") != {"version":1,"kind":"ci-downloads-seeded"}:
                raise FreshnessHold("metadata_torn")
            projection=_projection_for_retirement(generation,original['binding'],
                receipt.get('source_projection') if receipt is not None else None)
            if receipt is not None and projection is not None and 'source_projection' not in receipt:
                raise FreshnessHold('cleanup_uncertain')
            for name in children - {"control.json", "dependency-layout.json", "cargo-home-seeded.json",
                    "source-projection","source-projection.json"}:
                info = (generation / name).lstat()
                if layout and name in ("target", "cargo-home") and info.st_uid == os.geteuid() and stat.S_IMODE(info.st_mode) == 0o1777:
                    if info.st_dev != generation.stat().st_dev or info.st_gid != os.getegid():
                        raise FreshnessHold("invalidation_unsafe")
                    continue
                owner = os.geteuid() if name == "outputs" else worker_uid
                if (not stat.S_ISDIR(info.st_mode) or info.st_uid != owner or info.st_mode & 0o022
                        or info.st_dev != generation.stat().st_dev
                        or (name != "outputs" and info.st_gid != worker_gid)):
                    raise FreshnessHold("invalidation_unsafe")
        except (FreshnessHold, OSError, KeyError, TypeError, ValueError):
            continue
        if receipt is None:
            receipt={"version":1,"state":"reclaiming","original":original}
            if projection is not None:receipt['source_projection']=projection
            _write_record(receipt_path,receipt)
        try:
            if projection is not None:_unseal_projection_for_retirement(generation/'source-projection')
            shutil.rmtree(generation)
            fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
            try:
                os.fsync(fd)
            finally:
                os.close(fd)
            _write_record(receipt_path, {**receipt,"state":"reclaimed"})
        except OSError as error:
            if error.errno in (errno.ENOSPC,errno.EDQUOT):raise
            raise FreshnessHold("cleanup_uncertain") from None
        reclaimed.append(generation.name)
    return reclaimed


# The legacy /target root was created by the privileged cache bootstrap under
# the controller-owned mounted image. A worker can populate its derived output
# namespaces, but cannot replace that root in the controller-owned parent.
# This is cleanup ownership, never compiler provenance or reuse authority.
_LEGACY_UNIT = r'[A-Za-z_][A-Za-z0-9_]*-[a-z0-9]{1,32}'
_LEGACY_SESSION = r's-[a-z0-9]{1,32}-[a-z0-9]{1,32}-(?:[a-z0-9]{1,64}|working)'
_LEGACY_PATH = re.compile(r'target/(?:debug|release)/incremental/'+_LEGACY_UNIT+'/'+_LEGACY_SESSION+r'\Z')
_LEGACY_MAX_FILES = 4096
_LEGACY_MAX_RECORD = 1024*1024
_LEGACY_MAX_RETIREMENTS = 512


def _legacy_identity(info):
    return [info.st_dev,info.st_ino,info.st_mode,info.st_uid,info.st_gid]


def _legacy_directory(path,worker,device):
    info=path.lstat()
    if (not stat.S_ISDIR(info.st_mode) or info.st_dev!=device or [info.st_uid,info.st_gid]!=worker
            or info.st_mode&0o7022 or os.listxattr(path,follow_symlinks=False)):
        raise FreshnessHold('invalidation_unsafe')
    return _legacy_identity(info)


class _LegacyFileLimit(FreshnessHold):
    def __init__(self):
        super().__init__('invalidation_unsafe');self.legacy_bound='session_files'


def _legacy_files(path,worker,device,*,allow_links=False):
    import itertools
    rows=[]
    for child in itertools.islice(path.iterdir(),_LEGACY_MAX_FILES+1):
        if len(rows)>=_LEGACY_MAX_FILES:raise _LegacyFileLimit()
        if (child.name not in ('dep-graph.bin','query-cache.bin','work-products.bin','metadata.rmeta')
                and re.fullmatch(r'[a-z0-9]{1,64}\.o',child.name) is None):
            raise FreshnessHold('invalidation_unsafe')
        info=child.lstat()
        if (not stat.S_ISREG(info.st_mode) or info.st_dev!=device or [info.st_uid,info.st_gid]!=worker
                or info.st_mode&0o7022 or info.st_nlink<1 or not allow_links and info.st_nlink!=1
                or os.listxattr(child,follow_symlinks=False)):
            raise FreshnessHold('invalidation_unsafe')
        rows.append([child.name,*_legacy_identity(info),info.st_nlink,info.st_size,info.st_mtime_ns,info.st_ctime_ns])
    return sorted(rows)


def _legacy_directory_full(path,worker,device):
    identity=_legacy_directory(path,worker,device);info=path.lstat()
    if _legacy_identity(info)!=identity:raise FreshnessHold('cleanup_uncertain')
    return [*identity,info.st_mtime_ns,info.st_ctime_ns]


def _legacy_lock_identity(info):
    return [*_legacy_identity(info),info.st_nlink,info.st_size,info.st_mtime_ns,info.st_ctime_ns]


def _legacy_lock_path(session):
    return session.parent/('-'.join(session.name.split('-')[:3])+'.lock')


def _legacy_link_scope(cache,session,inventory,worker,device):
    """All caller-held session locks remain owned through intent and deletion."""
    endpoints={};tables={}
    for peer,rows in inventory.items():
        for row in rows:endpoints.setdefault(tuple(row[1:3]),[]).append((peer,row))
    selected=inventory[session];keys=[tuple(row[1:3]) for row in selected]
    if len(keys)!=len(set(keys)):return None
    linked={}
    for row in selected:
        peers=endpoints[tuple(row[1:3])]
        if len(peers)!=row[6] or any(other[1:]!=row[1:] for _,other in peers):return None
        if row[6]>1:
            linked[row[0]]=peers
            for peer,_ in peers:
                tables[peer]={'relative':str(peer.relative_to(cache)),
                    'directory':_legacy_directory_full(peer,worker,device),
                    'lock':_legacy_lock_identity(_legacy_lock_path(peer).lstat())}
    if not linked:return {}
    ordered=sorted(tables,key=lambda path:str(path.relative_to(cache)));indices={p:i for i,p in enumerate(ordered)}
    return {'sessions':[tables[p] for p in ordered],
        'files':{name:sorted([indices[p],row[0]] for p,row in peers) for name,peers in linked.items()}}


def _legacy_lock(path,worker,device):
    import fcntl
    descriptor=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK|os.O_CLOEXEC)
    try:
        info=os.fstat(descriptor)
        if (not stat.S_ISREG(info.st_mode) or info.st_dev!=device or [info.st_uid,info.st_gid]!=worker
                or info.st_mode&0o7022 or info.st_nlink!=1 or info.st_size!=0
                or _legacy_identity(path.lstat())!=_legacy_identity(info)):
            raise FreshnessHold('invalidation_unsafe')
        fcntl.flock(descriptor,fcntl.LOCK_EX|fcntl.LOCK_NB)
        return descriptor
    except BaseException:
        os.close(descriptor);raise


def _validate_legacy_retirement(record,cache_binding):
    common={'version','kind','state','binding','command','relative','worker','root','chain','session','bytes','shipping_authority'}
    try:
        if type(record) is not dict:raise ValueError()
        version=record.get('version');active=record.get('state')=='retiring'
        extra=({'links'} if active else {'peer_count','endpoint_count','link_scope_sha256'}) if version==2 else set()
        if (record.get('state') not in ('retiring','reclaimed')
                or set(record)!=(common|({'files'} if active else {'file_count','inventory_sha256'})|extra)
                or type(version) is not int or version not in (1,2)
                or record['kind']!='ci-legacy-incremental-retirement' or record['shipping_authority'] is not False
                or not _uuid(record['command']) or type(record['relative']) is not str
                or _LEGACY_PATH.fullmatch(record['relative']) is None):raise ValueError()
        validate_binding(record['binding'])
        if (record['state']=='retiring'
                and record['binding']['cache_identity_sha256']!=cache_binding['cache_identity_sha256']):raise ValueError()
        if (type(record['worker']) is not list or len(record['worker'])!=2
                or any(type(v) is not int or not 0<v<2**32 for v in record['worker'])
                or type(record['bytes']) is not int or record['bytes']<0):raise ValueError()
        for identity in [record['root'],record['session'],*record['chain']]:
            if type(identity) is not list or len(identity)!=5 or any(type(v) is not int or v<0 for v in identity):raise ValueError()
        if len(record['chain'])!=4:raise ValueError()
        if record['state']=='retiring':
            files=record['files']
            if type(files) is not list or len(files)>_LEGACY_MAX_FILES or files!=sorted(files):raise ValueError()
            names=set()
            for row in files:
                if (type(row) is not list or len(row)!=10 or type(row[0]) is not str or row[0] in names
                        or '/' in row[0] or row[0] in ('.','..') or any(type(v) is not int for v in row[1:])):raise ValueError()
                names.add(row[0])
            if sum(row[7] for row in files)!=record['bytes']:raise ValueError()
        elif (type(record['file_count']) is not int or not 0<=record['file_count']<=_LEGACY_MAX_FILES
                or not _digest(record['inventory_sha256'])):raise ValueError()
        if version==2:_validate_legacy_links(record)
    except (ValueError,KeyError,TypeError):raise FreshnessHold('cleanup_uncertain') from None
    return record


def _validate_legacy_links(record):
    """Validate cleanup-only v2; no path or integer coercions are accepted."""
    if record['state']=='reclaimed':
        if (type(record['peer_count']) is not int or not 1<=record['peer_count']<=256
                or type(record['endpoint_count']) is not int or not 2<=record['endpoint_count']<=65536
                or not _digest(record['link_scope_sha256'])):raise ValueError()
        return
    links=record['links'];files=record['files'];worker=record['worker'];device=record['root'][0]
    if type(links) is not dict or set(links)!={'sessions','files'}:raise ValueError()
    sessions=links['sessions'];mapping=links['files'];unit=str(Path(record['relative']).parent)
    if type(sessions) is not list or not 1<=len(sessions)<=256 or type(mapping) is not dict or not mapping:raise ValueError()
    paths=[]
    for peer in sessions:
        if (type(peer) is not dict or set(peer)!={'relative','directory','lock'}
                or type(peer['relative']) is not str or _LEGACY_PATH.fullmatch(peer['relative']) is None
                or str(Path(peer['relative']).parent)!=unit):raise ValueError()
        paths.append(peer['relative'])
        for key,size,kind in (('directory',7,stat.S_ISDIR),('lock',9,stat.S_ISREG)):
            row=peer[key]
            if (type(row) is not list or len(row)!=size or any(type(v) is not int or v<0 for v in row)
                    or row[0]!=device or row[3:5]!=worker or not kind(row[2]) or row[2]&0o7022):raise ValueError()
        if peer['lock'][5:7]!=[1,0]:raise ValueError()
        if peer['relative']==record['relative'] and peer['directory'][:5]!=record['session']:raise ValueError()
    if paths!=sorted(set(paths)) or record['relative'] not in paths:raise ValueError()
    names={};inodes=set()
    for row in files:
        if (row[0] not in ('dep-graph.bin','query-cache.bin','work-products.bin','metadata.rmeta')
                and re.fullmatch(r'[a-z0-9]{1,64}\.o',row[0]) is None):raise ValueError()
        if (any(v<0 for v in row[1:]) or row[1]!=device or row[4:6]!=worker
                or not stat.S_ISREG(row[3]) or row[3]&0o7022 or row[6]<1
                or tuple(row[1:3]) in inodes):raise ValueError()
        names[row[0]]=row;inodes.add(tuple(row[1:3]))
    if set(mapping)!={name for name,row in names.items() if row[6]>1}:raise ValueError()
    used=set();count=0
    for name,endpoints in mapping.items():
        if type(endpoints) is not list or len(endpoints)!=names[name][6]:raise ValueError()
        tuples=[]
        for endpoint in endpoints:
            if (type(endpoint) is not list or len(endpoint)!=2 or type(endpoint[0]) is not int
                    or not 0<=endpoint[0]<len(sessions) or type(endpoint[1]) is not str
                    or endpoint[1] not in ('dep-graph.bin','query-cache.bin','work-products.bin','metadata.rmeta')
                    and re.fullmatch(r'[a-z0-9]{1,64}\.o',endpoint[1]) is None):raise ValueError()
            used.add(endpoint[0]);tuples.append(tuple(endpoint))
        if tuples!=sorted(set(tuples)) or (paths.index(record['relative']),name) not in tuples:raise ValueError()
        count+=len(tuples)
    if used!=set(range(len(sessions))) or count>65536:raise ValueError()


def _finish_legacy_retirement(cache,allocation,record,*,held_locks=None):
    """Replay only a protected intent; no live worker may access this image."""
    if record['version']==2:return _finish_linked_legacy_retirement(cache,allocation,record,held_locks=held_locks)
    if _legacy_identity(cache.lstat())!=record['root']:raise FreshnessHold('cleanup_uncertain')
    source=cache/record['relative'];parent=source.parent;parts=Path(record['relative']).parts
    for ordinal,expected in enumerate(record['chain'],1):
        if _legacy_directory(cache.joinpath(*parts[:ordinal]),record['worker'],record['root'][0])!=expected:
            raise FreshnessHold('cleanup_uncertain')
    lock=parent/('-'.join(source.name.split('-')[:3])+'.lock')
    borrowed=held_locks is not None and lock in held_locks
    try:descriptor=held_locks[lock] if borrowed else _legacy_lock(lock,record['worker'],record['root'][0])
    except (OSError,FreshnessHold):raise FreshnessHold('cleanup_uncertain') from None
    try:
        data=allocation/'data'
        original=source.exists() or source.is_symlink();moved=data.exists() or data.is_symlink()
        if original and moved:raise FreshnessHold('cleanup_uncertain')
        if original:
            if (_legacy_directory(source,record['worker'],record['root'][0])!=record['session']
                    or _legacy_files(source,record['worker'],record['root'][0])!=record['files']):
                raise FreshnessHold('cleanup_uncertain')
            os.rename(source,data)
            for directory in (parent,allocation):
                fd=os.open(directory,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC)
                try:os.fsync(fd)
                finally:os.close(fd)
            moved=True
        if moved:
            if _legacy_directory(data,record['worker'],record['root'][0])!=record['session']:raise FreshnessHold('cleanup_uncertain')
            expected={row[0]:row for row in record['files']}
            if any(row!=expected.get(row[0]) for row in _legacy_files(data,record['worker'],record['root'][0])):
                raise FreshnessHold('cleanup_uncertain')
            shutil.rmtree(data)
            fd=os.open(allocation,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC)
            try:os.fsync(fd)
            finally:os.close(fd)
        completed={key:value for key,value in record.items() if key!='files'}
        completed.update(state='reclaimed',file_count=len(record['files']),
            inventory_sha256=hashlib.sha256(json.dumps(record['files'],sort_keys=True,separators=(',',':')).encode()).hexdigest())
        _write_record(allocation/'receipt.json',completed)
    except OSError as error:
        if error.errno in (errno.ENOSPC,errno.EDQUOT):raise
        raise FreshnessHold('cleanup_uncertain') from None
    finally:
        if not borrowed:os.close(descriptor)


def _finish_linked_legacy_retirement(cache,allocation,record,*,held_locks=None):
    import contextlib
    worker=record['worker'];device=record['root'][0];source=cache/record['relative'];data=allocation/'data'
    if _legacy_identity(cache.lstat())!=record['root']:raise FreshnessHold('cleanup_uncertain')
    parts=Path(record['relative']).parts
    for ordinal,expected in enumerate(record['chain'],1):
        if _legacy_directory(cache.joinpath(*parts[:ordinal]),worker,device)!=expected:raise FreshnessHold('cleanup_uncertain')
    try:
        with contextlib.ExitStack() as owned:
            original=source.exists() or source.is_symlink();moved=data.exists() or data.is_symlink()
            if original and moved:raise FreshnessHold('cleanup_uncertain')
            current=source if original else data
            rows=[]
            if original or moved:
                if _legacy_directory(current,worker,device)!=record['session']:raise FreshnessHold('cleanup_uncertain')
                rows=_legacy_files(current,worker,device,allow_links=True)
                expected={row[0]:row for row in record['files']}
                if (any(row!=expected.get(row[0]) for row in rows)
                        or original and rows!=record['files']):raise FreshnessHold('cleanup_uncertain')
            sessions=record['links']['sessions'];mapping=record['links']['files']
            needed={index for row in rows for index,_ in mapping.get(row[0],[])}
            if original or moved:needed.add(next(i for i,s in enumerate(sessions) if s['relative']==record['relative']))
            locks={}
            for index in sorted(needed,key=lambda i:str(_legacy_lock_path(cache/sessions[i]['relative']))):
                entry=sessions[index];peer=cache/entry['relative'];lock=_legacy_lock_path(peer)
                if lock not in locks:
                    if held_locks is not None and lock in held_locks:descriptor=held_locks[lock]
                    else:
                        descriptor=_legacy_lock(lock,worker,device);owned.callback(os.close,descriptor)
                    locks[lock]=descriptor
                descriptor=locks[lock]
                if (_legacy_lock_identity(os.fstat(descriptor))!=entry['lock']
                        or _legacy_lock_identity(lock.lstat())!=entry['lock']
                        or os.listxattr(lock,follow_symlinks=False)):raise FreshnessHold('cleanup_uncertain')
                selected=entry['relative']==record['relative']
                if selected:
                    identity=_legacy_directory_full(current,worker,device)
                    if identity[:5]!=entry['directory'][:5] or original and identity!=entry['directory']:
                        raise FreshnessHold('cleanup_uncertain')
                elif _legacy_directory_full(peer,worker,device)!=entry['directory']:raise FreshnessHold('cleanup_uncertain')
            inventories={}
            for row in rows:
                for index,name in mapping.get(row[0],[]):
                    entry=sessions[index];peer=cache/entry['relative']
                    if entry['relative']==record['relative']:peer=current
                    if peer not in inventories:inventories[peer]={r[0]:r for r in _legacy_files(peer,worker,device,allow_links=True)}
                    other=inventories[peer].get(name)
                    if other is None or other[1:]!=row[1:]:raise FreshnessHold('cleanup_uncertain')
            if (original or moved) and _legacy_files(current,worker,device,allow_links=True)!=rows:
                raise FreshnessHold('cleanup_uncertain')
            if original:
                os.rename(source,data)
                for directory in (source.parent,allocation):
                    fd=os.open(directory,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC)
                    try:os.fsync(fd)
                    finally:os.close(fd)
                moved=True
            if moved:
                shutil.rmtree(data)
                fd=os.open(allocation,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC)
                try:os.fsync(fd)
                finally:os.close(fd)
            completed={key:value for key,value in record.items() if key not in ('files','links')}
            canonical=lambda value:hashlib.sha256(json.dumps(value,sort_keys=True,separators=(',',':')).encode()).hexdigest()
            completed.update(state='reclaimed',file_count=len(record['files']),inventory_sha256=canonical(record['files']),
                peer_count=len(sessions),endpoint_count=sum(len(v) for v in mapping.values()),link_scope_sha256=canonical(record['links']))
            _write_record(allocation/'receipt.json',completed)
    except OSError as error:
        if error.errno in (errno.ENOSPC,errno.EDQUOT):raise
        raise FreshnessHold('cleanup_uncertain') from None


_LEGACY_DIAGNOSTIC_BOUNDS={'session_candidates','session_files','census_entries','record_bytes','attempt_retirements',
    'unit_entries','unit_directories','history_records'}
_LEGACY_RETENTION_REASONS={'unsafe_session','unclosed_links','lock_unavailable','bounded'}


def legacy_diagnostic(binding,command,*,allow_closed_links):
    validate_binding(binding)
    if not _uuid(command) or type(allow_closed_links) is not bool:raise FreshnessHold('cleanup_uncertain')
    return {'version':1,'kind':'ci-legacy-incremental-diagnostic','command':command,
        'cache_identity_sha256':binding['cache_identity_sha256'],
        'policy':'closed-links' if allow_closed_links else 'strict-single-link',
        'inspected_sessions':0,'retained_sessions':0,'retired_sessions':0,'replayed_sessions':0,
        'unrecognized_entries':0,'unsafe_namespaces':0,
        'retention':{name:0 for name in sorted(_LEGACY_RETENTION_REASONS)},'bounds':[],
        'shipping_authority':False}


def validate_legacy_diagnostic(value,binding,command,retired):
    try:
        expected=set(legacy_diagnostic(binding,command,allow_closed_links=False))
        if (type(value) is not dict or set(value)!=expected or type(value['version']) is not int or value['version']!=1
                or value['kind']!='ci-legacy-incremental-diagnostic' or value['command']!=command
                or value['cache_identity_sha256']!=binding['cache_identity_sha256']
                or value['policy'] not in ('strict-single-link','closed-links') or value['shipping_authority'] is not False):raise ValueError()
        for name,limit in (('inspected_sessions',256),('retained_sessions',256),('retired_sessions',32),('replayed_sessions',32),
                ('unrecognized_entries',2**22),('unsafe_namespaces',4096)):
            if type(value[name]) is not int or not 0<=value[name]<=limit:raise ValueError()
        if (type(value['retention']) is not dict or set(value['retention'])!=_LEGACY_RETENTION_REASONS
                or any(type(v) is not int or not 0<=v<=256 for v in value['retention'].values())
                or sum(value['retention'].values())!=value['retained_sessions']
                or value['replayed_sessions']>value['retired_sessions']
                or value['retired_sessions']!=len(retired)
                or value['inspected_sessions']!=value['retained_sessions']+value['retired_sessions']-value['replayed_sessions']
                or type(value['bounds']) is not list or any(type(v) is not str or v not in _LEGACY_DIAGNOSTIC_BOUNDS for v in value['bounds'])
                or value['bounds']!=sorted(set(value['bounds']))):raise ValueError()
    except (KeyError,TypeError,ValueError):raise FreshnessHold('cleanup_uncertain') from None
    return value


def _legacy_bound(diagnostic,name):
    diagnostic['bounds']=sorted(set(diagnostic['bounds'])|{name})


def _legacy_retained(diagnostic,reason,*,bound=None):
    diagnostic['retained_sessions']+=1;diagnostic['retention'][reason]+=1
    if bound is not None:_legacy_bound(diagnostic,bound)


def _reclaim_closed_legacy_unit(cache,unit,sessions,binding,command,worker,root_identity,chain,retired,limit,record_slots,diagnostic):
    import contextlib
    candidates=[p for p in sessions if re.fullmatch(_LEGACY_SESSION,p.name)]
    if len(candidates)>256:_legacy_bound(diagnostic,'session_candidates');return []
    reclaimed=[];device=root_identity[0]
    with contextlib.ExitStack() as owned:
        locks={}
        for lock in sorted({_legacy_lock_path(p) for p in candidates}):
            try:
                descriptor=_legacy_lock(lock,worker,device)
                if os.listxattr(lock,follow_symlinks=False):os.close(descriptor);continue
            except (OSError,FreshnessHold):continue
            owned.callback(os.close,descriptor);locks[lock]=descriptor
        for session in candidates:
            if len(reclaimed)>=limit:_legacy_bound(diagnostic,'attempt_retirements');break
            diagnostic['inspected_sessions']+=1
            inventory={};unavailable={};total=0
            for peer in candidates:
                if not peer.exists():continue
                lock=_legacy_lock_path(peer)
                if lock not in locks:continue
                try:
                    if _legacy_lock_identity(os.fstat(locks[lock]))!=_legacy_lock_identity(lock.lstat()):continue
                    _legacy_directory(peer,worker,device)
                    rows=_legacy_files(peer,worker,device,allow_links=True)
                except (OSError,FreshnessHold) as error:
                    if getattr(error,'legacy_bound',None)=='session_files':
                        _legacy_bound(diagnostic,'session_files');unavailable[peer]='bounded'
                    continue
                total+=len(rows)
                if total>65536:
                    _legacy_retained(diagnostic,'bounded',bound='census_entries');return reclaimed
                inventory[peer]=rows
            if session not in inventory:
                reason='lock_unavailable' if _legacy_lock_path(session) not in locks else unavailable.get(session,'unsafe_session')
                _legacy_retained(diagnostic,reason);continue
            try:links=_legacy_link_scope(cache,session,inventory,worker,device)
            except (OSError,FreshnessHold):_legacy_retained(diagnostic,'unsafe_session');continue
            if links is None:_legacy_retained(diagnostic,'unclosed_links');continue
            if len(reclaimed)>=record_slots:
                _legacy_retained(diagnostic,'bounded',bound='history_records');continue
            files=inventory[session]
            record={'version':2 if links else 1,'kind':'ci-legacy-incremental-retirement','state':'retiring',
                'binding':binding,'command':command,'relative':str(session.relative_to(cache)),
                'worker':worker,'root':root_identity,'chain':chain,
                'session':_legacy_directory(session,worker,device),'files':files,
                'bytes':sum(row[7] for row in files),'shipping_authority':False}
            if links:record['links']=links
            if len(json.dumps(record).encode())>_LEGACY_MAX_RECORD:
                _legacy_retained(diagnostic,'bounded',bound='record_bytes');continue
            _validate_legacy_retirement(record,binding)
            _directory(retired,0o700,create=True)
            identity=str(uuid.uuid4());allocation=retired/identity;allocation.mkdir(mode=0o700)
            for directory in (cache,retired):
                fd=os.open(directory,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC)
                try:os.fsync(fd)
                finally:os.close(fd)
            _write_record(allocation/'receipt.json',record)
            _finish_legacy_retirement(cache,allocation,record,held_locks=locks);reclaimed.append(identity)
    return reclaimed


def _legacy_settle_envelope(allocation,*,unpublished=False):
    """Only the trusted atomic writer can populate these root-private files."""
    import itertools
    _directory(allocation,0o700,create=False)
    info=allocation.lstat()
    if stat.S_IMODE(info.st_mode)!=0o700 or os.listxattr(allocation,follow_symlinks=False):
        raise FreshnessHold('cleanup_uncertain')
    children=list(itertools.islice(allocation.iterdir(),4));temporary=[]
    if len(children)>3:raise FreshnessHold('cleanup_uncertain')
    for child in children:
        if child.name in ('receipt.json','data'):
            if unpublished:raise FreshnessHold('cleanup_uncertain')
            continue
        value=child.lstat()
        if (re.fullmatch(r'\.preparing-[a-z0-9_]{8}',child.name) is None or temporary
                or not stat.S_ISREG(value.st_mode) or value.st_uid!=os.geteuid()
                or value.st_dev!=info.st_dev or value.st_nlink!=1 or stat.S_IMODE(value.st_mode)!=0o600
                or value.st_size>_LEGACY_MAX_RECORD or os.listxattr(child,follow_symlinks=False)):
            raise FreshnessHold('cleanup_uncertain')
        temporary.append(child)
    for child in temporary:child.unlink()
    if unpublished:allocation.rmdir()
    directory=allocation.parent if unpublished else allocation
    fd=os.open(directory,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC)
    try:os.fsync(fd)
    finally:os.close(fd)


def reclaim_legacy_incremental(cache,binding,command,worker_uid,worker_gid,*,limit=32,allow_closed_links=False,diagnostic=None):
    if diagnostic is not None and type(diagnostic) is not dict:raise FreshnessHold('cleanup_uncertain')
    if diagnostic is None:diagnostic={}
    diagnostic.clear();diagnostic.update(legacy_diagnostic(binding,command,allow_closed_links=allow_closed_links))
    retired=_reclaim_legacy_incremental(cache,binding,command,worker_uid,worker_gid,
        limit=limit,allow_closed_links=allow_closed_links,diagnostic=diagnostic)
    diagnostic['retired_sessions']=len(retired)
    validate_legacy_diagnostic(diagnostic,binding,command,retired)
    return retired


def _reclaim_legacy_incremental(cache,binding,command,worker_uid,worker_gid,*,limit,allow_closed_links,diagnostic):
    """Retire recognized derived sessions under original exclusive admission.

    The privileged guest caller must already hold the original registered image
    and target leases, have reconciled all prior cleanup holds, and call before
    starting ANY workers. This function cannot manufacture those capabilities.
    Compiled artifacts, fingerprints, downloads and unknown allocations stay.
    Returned IDs acknowledge cleanup only and never authorize artifact reuse.
    """
    import itertools
    validate_binding(binding)
    if (not _uuid(command) or type(limit) is not int or not 1<=limit<=32 or type(allow_closed_links) is not bool
            or any(type(v) is not int or not 0<v<2**32 for v in (worker_uid,worker_gid))
            or not shutil.rmtree.avoids_symlink_attacks):raise FreshnessHold('cleanup_uncertain')
    cache=Path(cache)
    if not cache.is_absolute() or cache.resolve()!=cache:raise FreshnessHold('invalidation_unsafe')
    _directory(cache,0o700,create=False);root_identity=_legacy_identity(cache.lstat());worker=[worker_uid,worker_gid]
    retired=cache/'ci-legacy-incremental-retirements';reclaimed=[]
    if retired.exists() or retired.is_symlink():
        _directory(retired,0o700,create=False)
        if stat.S_IMODE(retired.stat().st_mode)!=0o700:raise FreshnessHold('cleanup_uncertain')
        allocations=list(itertools.islice(retired.iterdir(),_LEGACY_MAX_RETIREMENTS+1))
        if len(allocations)>_LEGACY_MAX_RETIREMENTS:raise FreshnessHold('cleanup_uncertain')
        for ordinal,allocation in enumerate(sorted(allocations)):
            if not _uuid(allocation.name):raise FreshnessHold('cleanup_uncertain')
            _directory(allocation,0o700,create=False)
            try:record=_read_record(allocation/'receipt.json',max_bytes=_LEGACY_MAX_RECORD)
            except FreshnessHold as error:
                if error.reason!='metadata_missing':raise
                # No published intent means source mutation was never authorized.
                # Recognize only the bounded private atomic-writer crash window.
                _legacy_settle_envelope(allocation,unpublished=True)
                continue
            record=_validate_legacy_retirement(record,binding)
            _legacy_settle_envelope(allocation)
            if record['state']=='retiring':
                _finish_legacy_retirement(cache,allocation,record);reclaimed.append(allocation.name)
                diagnostic['replayed_sessions']+=1
                if len(reclaimed)>=limit:
                    for pending in sorted(allocations)[ordinal+1:]:
                        if not _uuid(pending.name):raise FreshnessHold('cleanup_uncertain')
                        _directory(pending,0o700,create=False)
                        later=_validate_legacy_retirement(_read_record(pending/'receipt.json',max_bytes=_LEGACY_MAX_RECORD),binding)
                        _legacy_settle_envelope(pending)
                        if later['state']=='retiring':raise FreshnessHold('cleanup_uncertain')
                    return reclaimed
    else:allocations=[]
    # Reconciled unpublished envelopes no longer consume persisted history.
    allocations=[allocation for allocation in allocations if allocation.exists()]
    target=cache/'target'
    if not target.exists() and not target.is_symlink():return reclaimed
    try:target_identity=_legacy_directory(target,worker,root_identity[0])
    except (OSError,FreshnessHold):diagnostic['unsafe_namespaces']+=1;return reclaimed
    inspected=0
    for profile in ('debug','release'):
        incremental=target/profile/'incremental'
        try:
            chain=[target_identity,*[_legacy_directory(path,worker,root_identity[0]) for path in (target/profile,incremental)]]
            units=sorted(itertools.islice(incremental.iterdir(),1025))
        except FileNotFoundError:continue
        except (OSError,FreshnessHold):diagnostic['unsafe_namespaces']+=1;continue
        if len(units)>1024:_legacy_bound(diagnostic,'unit_directories');units=units[:1024]
        for unit in units:
            if re.fullmatch(_LEGACY_UNIT,unit.name) is None:diagnostic['unrecognized_entries']+=1;continue
            try:
                unit_identity=_legacy_directory(unit,worker,root_identity[0])
                sessions=sorted(itertools.islice(unit.iterdir(),1025))
            except (OSError,FreshnessHold):diagnostic['unsafe_namespaces']+=1;continue
            diagnostic['unrecognized_entries']+=sum(re.fullmatch(_LEGACY_SESSION,p.name) is None
                and re.fullmatch(r's-[a-z0-9]{1,32}-[a-z0-9]{1,32}\.lock',p.name) is None for p in sessions)
            if allow_closed_links:
                candidates=sum(re.fullmatch(_LEGACY_SESSION,p.name) is not None for p in sessions)
                if len(sessions)>1024:_legacy_bound(diagnostic,'unit_entries');continue
                if candidates>256:_legacy_bound(diagnostic,'session_candidates');continue
                if inspected+candidates>256:_legacy_bound(diagnostic,'session_candidates');return reclaimed
                if len(reclaimed)>=limit:_legacy_bound(diagnostic,'attempt_retirements');return reclaimed
                inspected+=candidates
                reclaimed.extend(_reclaim_closed_legacy_unit(cache,unit,sessions,binding,command,worker,root_identity,
                    [*chain,unit_identity],retired,limit-len(reclaimed),
                    _LEGACY_MAX_RETIREMENTS-len(allocations)-(len(reclaimed)-diagnostic['replayed_sessions']),diagnostic))
                continue
            for session in sessions:
                if re.fullmatch(_LEGACY_SESSION,session.name) is None:continue
                if inspected>=256:_legacy_bound(diagnostic,'session_candidates');return reclaimed
                if len(reclaimed)>=limit:_legacy_bound(diagnostic,'attempt_retirements');return reclaimed
                inspected+=1
                diagnostic['inspected_sessions']+=1
                if len(allocations)+len(reclaimed)-diagnostic['replayed_sessions']>=_LEGACY_MAX_RETIREMENTS:
                    _legacy_retained(diagnostic,'bounded',bound='history_records');continue
                lock=_legacy_lock_path(session)
                try:descriptor=_legacy_lock(lock,worker,root_identity[0])
                except (OSError,FreshnessHold):_legacy_retained(diagnostic,'lock_unavailable');continue
                try:
                    try:
                        session_identity=_legacy_directory(session,worker,root_identity[0]);files=_legacy_files(session,worker,root_identity[0])
                    except (OSError,FreshnessHold) as error:
                        if getattr(error,'legacy_bound',None)=='session_files':
                            _legacy_retained(diagnostic,'bounded',bound='session_files')
                        else:_legacy_retained(diagnostic,'unsafe_session')
                        continue
                    record={'version':1,'kind':'ci-legacy-incremental-retirement','state':'retiring','binding':binding,
                        'command':command,'relative':str(session.relative_to(cache)),'worker':worker,'root':root_identity,
                        'chain':[*chain,unit_identity],'session':session_identity,'files':files,
                        'bytes':sum(row[7] for row in files),'shipping_authority':False}
                    if len(json.dumps(record).encode())>_LEGACY_MAX_RECORD:
                        _legacy_retained(diagnostic,'bounded',bound='record_bytes');continue
                    _directory(retired,0o700,create=True)
                    identity=str(uuid.uuid4());allocation=retired/identity;allocation.mkdir(mode=0o700)
                    # Persist both namespace ancestors before committing intent.
                    # Fsync of receipt.json's own directory alone cannot do that.
                    for directory in (cache,retired):
                        fd=os.open(directory,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC)
                        try:os.fsync(fd)
                        finally:os.close(fd)
                    _write_record(allocation/'receipt.json',record)
                    _finish_legacy_retirement(cache,allocation,record,held_locks={lock:descriptor});reclaimed.append(identity)
                finally:os.close(descriptor)
    return reclaimed
