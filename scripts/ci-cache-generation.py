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
        '-C',str(source),'pack-objects','--revs','--stdout'],env=environment,stdin=subprocess.PIPE,
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
