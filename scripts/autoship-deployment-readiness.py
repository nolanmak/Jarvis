#!/usr/bin/env python3
"""Process-bound effective configuration; never print environment or credentials."""
import argparse
import fcntl
import hashlib
import json
import os
import re
from pathlib import Path
import shlex
import shutil
import stat
import subprocess
import tempfile

ROLES={'daemon':'augmentagent.service','worker':'augmentagent-autopr.service'}
KEYS={'HOME','PATH','XDG_CONFIG_HOME','XDG_STATE_HOME','CLAUDE_CLI','CODEX_CLI',
      'AUGMENTAGENT_NATIVE_VM_POLICY','AUGMENTAGENT_MODEL_SELECTION_CONFIG','AUGMENTAGENT_MODEL_ROUTER_CONFIG',
      'AUGMENTAGENT_MODEL_ROUTER_ALLOWED_HOSTS','AUGMENTAGENT_REASONER_CHAIN','AUGMENTAGENT_AUTOPR_REVIEWERS',
      'AUGMENTAGENT_AUTOPR','AUGMENTAGENT_AUTOPR_WORKFLOW_DB','AUGMENTAGENT_AUTOPR_OPENED_FILE',
      'AUGMENTAGENT_AUTOPR_ATTEMPTS_FILE','AUGMENTAGENT_COOLDOWN_FILE','AUGMENTAGENT_AUTOPR_DAILY_CAP',
      'AUGMENTAGENT_AUTOPR_AUTOMERGE','AUGMENTAGENT_REASONER_TIMEOUT_SECS','AUGMENTAGENT_RUN_RESOURCES','AUGMENTAGENT_RUN_MEMORY_BYTES',
      'AUGMENTAGENT_RUN_TEXT_MEMORY_BYTES','AUGMENTAGENT_RUN_HOST_OVERHEAD_BYTES','AUGMENTAGENT_RUN_PIDS',
      'AUGMENTAGENT_RUN_CPU_PERCENT','AUGMENTAGENT_OPUS_MODEL','AUGMENTAGENT_MODEL_QWEN_ENABLED','AUGMENTAGENT_MODEL_GLM_ENABLED'}
KEYS.update('AUGMENTAGENT_MODEL_'+provider+'_'+tier for provider in ('CLAUDE','CODEX','QWEN','GLM','GEMINI','CEREBRAS') for tier in ('QUALITY','FAST'))
# Resolution inputs have their own effective evidence below; cosmetic PATH
# differences alone do not imply different provider executables.
DECLARATION_KEYS=KEYS-{'HOME','PATH','XDG_CONFIG_HOME','XDG_STATE_HOME','CLAUDE_CLI','CODEX_CLI',
                     'AUGMENTAGENT_NATIVE_VM_POLICY','AUGMENTAGENT_MODEL_SELECTION_CONFIG','AUGMENTAGENT_MODEL_ROUTER_CONFIG','AUGMENTAGENT_AUTOPR'}

class Refused(Exception):pass

def private(path,limit=65536):
    descriptor=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK|os.O_CLOEXEC)
    with os.fdopen(descriptor,'rb') as stream:
        before=os.fstat(stream.fileno())
        if not stat.S_ISREG(before.st_mode) or before.st_uid!=os.getuid() or before.st_mode&0o077 or before.st_nlink!=1 or before.st_size>limit:raise Refused('private-file-invalid')
        data=stream.read(limit+1);after=os.fstat(stream.fileno())
        if len(data)>limit or (before.st_ino,before.st_size,before.st_mtime_ns,before.st_ctime_ns)!=(after.st_ino,after.st_size,after.st_mtime_ns,after.st_ctime_ns):raise Refused('private-file-changed')
        return data

def digest(data):return hashlib.sha256(data).hexdigest()
def owner_identity(environment):
    # Credentials never enter the generic configuration allowlist or output.
    try:
        owner=environment.get('DISCORD_ALLOWED_USER_ID','');token=environment.get('DISCORD_BOT_TOKEN','')
        if not isinstance(owner,str) or not re.fullmatch(r'\+?[0-9]{1,128}',owner) or not 0<int(owner)<2**64:return None
        if not isinstance(token,str) or not 0<len(token)<=4096:return None
        return {'owner':str(int(owner)),'credential_sha256':digest(token.encode('utf-8'))}
    except Exception:return None

def checked_owner(value):
    if value is None:return None
    if not isinstance(value,dict) or set(value)!={'owner','credential_sha256'}:raise Refused('owner-identity-invalid')
    owner=value['owner'];credential=value['credential_sha256']
    if not isinstance(owner,str) or not re.fullmatch(r'[1-9][0-9]{0,19}',owner) or not 0<int(owner)<2**64:raise Refused('owner-identity-invalid')
    if not isinstance(credential,str) or not re.fullmatch(r'[0-9a-f]{64}',credential):raise Refused('owner-identity-invalid')
    return value

def birth(pid):return Path(f'/proc/{pid}/stat').read_text().rsplit(')',1)[1].split()[19]
def boot():return Path('/proc/sys/kernel/random/boot_id').read_text().strip()

def file_hash(path):
    with os.fdopen(os.open(path,os.O_RDONLY|os.O_NONBLOCK|os.O_CLOEXEC),'rb') as stream:
        before=os.fstat(stream.fileno())
        if not stat.S_ISREG(before.st_mode) or before.st_uid not in (0,os.getuid()) or before.st_mode&0o022 or not before.st_mode&0o111 or before.st_size>1024**3:raise Refused('executable-untrusted')
        result=hashlib.sha256()
        for chunk in iter(lambda:stream.read(1024*1024),b''):result.update(chunk)
        after=os.fstat(stream.fileno())
        if (before.st_ino,before.st_size,before.st_mtime_ns,before.st_ctime_ns)!=(after.st_ino,after.st_size,after.st_mtime_ns,after.st_ctime_ns):raise Refused('executable-changed')
        return result.hexdigest()

def selected(selection):
    data=private(selection)
    entries=[]
    for line in data.decode().splitlines():entries.extend(shlex.split(line,comments=True))
    if len(entries)!=1 or not entries[0].startswith('AUGMENTAGENT_NATIVE_VM_POLICY='):raise Refused('selection-invalid')
    path=Path(entries[0].split('=',1)[1])
    if not path.is_absolute() or '\n' in str(path) or '\r' in str(path):raise Refused('selection-invalid')
    policy=private(path,2*1024**2)
    value=json.loads(policy)
    if value.get('version')!=1 or not isinstance(value.get('providers'),dict) or not value['providers']:raise Refused('policy-invalid')
    return path,digest(data),digest(policy),value

def provider_hash(config,key,default,cwd):
    program=config.get(key) or default
    if Path(program).is_absolute():path=Path(program)
    elif '/' in program:path=cwd/program
    else:
        paths=[str(Path(part) if Path(part).is_absolute() else cwd/part) for part in config.get('PATH','/usr/bin:/bin').split(os.pathsep)]
        path=shutil.which(program,path=os.pathsep.join(paths))
    if not path:raise Refused('provider-executable-missing')
    return file_hash(Path(path).resolve(strict=True))

def snapshot(role,pid,binary,binary_sha,policy,policy_sha,declaration,receipts):
    try:
        if pid<=0:raise Refused('service-not-running')
        proc=Path('/proc')/str(pid)
        if proc.stat().st_uid!=os.getuid():raise Refused('process-owner-mismatch')
        identity=birth(pid);cwd=(proc/'cwd').resolve(strict=True)
        raw=private(receipts/(role+'.json'));value=json.loads(raw)
        if value.get('version') not in (1,2) or value.get('role')!=role:raise Refused('process-receipt-invalid')
        if value['version']==2:
            if 'owner_identity' not in value:raise Refused('owner-identity-missing')
            checked_owner(value['owner_identity'])
            if value.get('executable_sha256')!=binary_sha:raise Refused('receipt-executable-mismatch')
        if value.get('pid')!=pid or value.get('birth')!=identity or value.get('boot_id')!=boot():raise Refused('process-receipt-stale')
        config=value.get('config')
        if not isinstance(config,dict) or not set(config)<=KEYS or any(not isinstance(v,str) for v in config.values()):raise Refused('process-receipt-invalid')
        if role=='daemon' and value.get('external') is not True:raise Refused('daemon-worker-mode-unverified')
        if value.get('dry_run') is not False:raise Refused('service-dry-run')
        if config.get('AUGMENTAGENT_AUTOPR','').strip().lower() not in ('1','true'):raise Refused('autoship-disabled')
        actual=config.get('AUGMENTAGENT_NATIVE_VM_POLICY')
        if not actual:raise Refused('policy-not-selected')
        if actual!=str(policy):raise Refused('policy-selection-mismatch')
        if value.get('policy_sha256')!=policy_sha:raise Refused('policy-content-mismatch')
        if digest(private(Path(actual),2*1024**2))!=policy_sha:raise Refused('policy-selection-mismatch')
        if os.readlink(proc/'exe')!=str(binary) or file_hash(proc/'exe')!=binary_sha:raise Refused('executable-mismatch')
        providers={}
        for provider in declaration['providers']:
            if provider in ('claude','codex','qwen','glm'):
                providers[provider]=provider_hash(config,'CLAUDE_CLI' if provider=='claude' else 'CODEX_CLI','claude' if provider=='claude' else 'codex',cwd)
        config_home=Path(config.get('XDG_CONFIG_HOME') or str(Path(config.get('HOME',''))/'.config'))/'augmentagent'
        files={}
        for key,name in [('AUGMENTAGENT_MODEL_SELECTION_CONFIG','model-selection.json'),('AUGMENTAGENT_MODEL_ROUTER_CONFIG','model-router.json')]:
            path=Path(config.get(key) or str(config_home/name))
            if not path.is_absolute():path=cwd/path
            try:files[key]=digest(private(path))
            except FileNotFoundError:files[key]=None
        state_home=Path(config.get('XDG_STATE_HOME',''))
        if not state_home.is_absolute():state_home=Path(config.get('HOME',''))/'.local/state'
        if not state_home.is_absolute():state_home=cwd/state_home
        fingerprints={'state_root':str((state_home/'augmentagent').resolve()),'declarations':{key:config.get(key) for key in sorted(DECLARATION_KEYS)},'files':files,'providers':providers}
        if birth(pid)!=identity or (proc/'cwd').resolve(strict=True)!=cwd or private(receipts/(role+'.json'))!=raw:raise Refused('process-changed')
        return {'status':'configuration-observed','pid':pid,'birth':identity,'policy_sha256':policy_sha,
                'executable_sha256':binary_sha,'configuration_sha256':digest(json.dumps(fingerprints,sort_keys=True).encode()),
                'provider_executable_sha256':providers,'receipt_sha256':digest(raw)}
    except Refused as error:return {'status':str(error),'pid':pid}
    except FileNotFoundError:return {'status':'process-evidence-missing','pid':pid}
    except Exception:return {'status':'process-evidence-invalid','pid':pid}

def service_pid(role):
    output=subprocess.run(['/usr/bin/systemctl','--user','show',ROLES[role],'-p','MainPID','--value'],capture_output=True,text=True,timeout=3)
    return int(output.stdout.strip()) if output.returncode==0 and output.stdout.strip().isdigit() else 0

def workflow_store_configuration(receipts,pid_for=service_pid):
    """Observe the live worker's post-bootstrap receipt, never initial environ.

    Return only store-resolution inputs; this grants no workflow authority.
    Missing/stale worker evidence refuses instead of selecting another store.
    """
    try:
        pid=pid_for('worker')
        if type(pid) is not int or pid<=0:raise Refused('worker-not-running')
        proc=Path('/proc')/str(pid)
        if proc.stat().st_uid!=os.getuid():raise Refused('process-owner-mismatch')
        identity=birth(pid);cwd=(proc/'cwd').resolve(strict=True)
        raw=private(receipts/'worker.json');value=json.loads(raw)
        if (type(value.get('version')) is not int or value['version'] not in (1,2) or value.get('role')!='worker' or value.get('pid')!=pid
                or value.get('birth')!=identity or value.get('boot_id')!=boot()
                or value.get('dry_run') is not False):raise Refused('worker-receipt-unverified')
        config=value.get('config')
        if not isinstance(config,dict) or not set(config)<=KEYS or any(not isinstance(v,str) for v in config.values()):
            raise Refused('worker-receipt-invalid')
        # Version-1 collectors did not all capture HOME. Without an explicit
        # database/absolute state path, absence is ambiguous rather than proof
        # that the running worker chose the observer's fallback directory.
        if (value['version']==1 and 'HOME' not in config
                and not config.get('AUGMENTAGENT_AUTOPR_WORKFLOW_DB')
                and not Path(config.get('XDG_STATE_HOME','')).is_absolute()):
            raise Refused('worker-store-resolution-unverified')
        if (pid_for('worker')!=pid or birth(pid)!=identity or (proc/'cwd').resolve(strict=True)!=cwd
                or private(receipts/'worker.json')!=raw):raise Refused('process-changed')
        return {'cwd':str(cwd),'workflow_db':config.get('AUGMENTAGENT_AUTOPR_WORKFLOW_DB'),
                'state_home':config.get('XDG_STATE_HOME'),'home':config.get('HOME'),
                'daily_cap':config.get('AUGMENTAGENT_AUTOPR_DAILY_CAP')}
    except (OSError,ValueError,subprocess.SubprocessError):
        raise Refused('worker-configuration-unavailable') from None

def inspect(binary, selection, receipts, pid_for=service_pid):
    report={'status':'deployment-unverified','matches':False,'live_verified':False,'services':{},'worker_running':False,'expected_active':False}
    try:
        pids={role:pid_for(role) for role in ROLES};report['worker_running']=pids['worker']>0
        report['expected_active']=report['worker_running']
        try:
            host=json.loads(private(receipts/'daemon.json'))
            if host.get('pid')==pids['daemon'] and host.get('birth')==birth(pids['daemon']) and host.get('boot_id')==boot():
                report['expected_active'] |= host.get('config',{}).get('AUGMENTAGENT_AUTOPR','').strip().lower() in ('1','true')
        except Exception:pass
        try:policy,selection_sha,policy_sha,declaration=selected(selection)
        except FileNotFoundError:report['status']='selection-missing';return report
        except Exception:report['status']='selection-invalid';return report
        binary=Path(binary).resolve(strict=True);binary_sha=file_hash(binary)
        report['expected_executable_sha256']=binary_sha;report['selected_policy_sha256']=policy_sha
        report['services']={role:snapshot(role,pids[role],binary,binary_sha,policy,policy_sha,declaration,receipts) for role in ROLES}
        for role in ROLES:
            if pid_for(role)!=pids[role] or (pids[role]>0 and birth(pids[role])!=report['services'][role].get('birth',birth(pids[role]))):
                report['status']='process-changed';return report
        if selected(selection)[:3]!=(policy,selection_sha,policy_sha):report['status']='selection-changed';return report
        if file_hash(binary)!=binary_sha:report['status']='executable-changed';return report
        if any(value['status']!='configuration-observed' for value in report['services'].values()):
            report['status']='service-configuration-unverified';return report
        hashes={value['configuration_sha256'] for value in report['services'].values()}
        report['matches']=len(hashes)==1
        report['status']='configuration-matched' if report['matches'] else 'effective-configuration-mismatch'
    except Exception:report['status']='deployment-inspection-unavailable'
    return report

def inspect_owner(binary,selection,receipts,pid_for=service_pid,configured=None,caller_pid=None):
    report=inspect(binary,selection,receipts,pid_for)
    report.update(owner_matches=False,owner_status='owner-configuration-unverified')
    if not report['matches']:return report
    try:
        expected=checked_owner(owner_identity(os.environ) if configured is None else configured)
        if expected is None:raise Refused('owner-configuration-unavailable')
        caller=os.getppid() if caller_pid is None else caller_pid
        if report['services']['worker']['pid']!=caller:raise Refused('owner-worker-caller-mismatch')
        directory=receipts.lstat()
        if not stat.S_ISDIR(directory.st_mode) or directory.st_uid!=os.getuid() or directory.st_mode&0o077:raise Refused('receipt-directory-invalid')
        captured={}
        for role in ROLES:
            raw=private(receipts/(role+'.json'));value=json.loads(raw);service=report['services'][role]
            if digest(raw)!=service['receipt_sha256']:raise Refused('process-receipt-changed')
            if value.get('version')!=2:raise Refused('owner-receipt-version-unverified')
            if checked_owner(value.get('owner_identity'))!=expected:raise Refused('owner-configuration-mismatch')
            captured[role]=raw
        for role in ROLES:
            service=report['services'][role];pid=service['pid']
            if pid_for(role)!=pid or birth(pid)!=service['birth']:raise Refused('process-changed')
            if file_hash(Path('/proc')/str(pid)/'exe')!=report['expected_executable_sha256']:raise Refused('executable-changed')
            if private(receipts/(role+'.json'))!=captured[role]:raise Refused('process-receipt-changed')
        current=receipts.lstat()
        if (directory.st_dev,directory.st_ino)!=(current.st_dev,current.st_ino):raise Refused('receipt-directory-changed')
        report['owner_matches']=True;report['owner_status']='owner-configuration-matched'
        report['owner_configuration_sha256']=digest(json.dumps(expected,sort_keys=True,separators=(',',':')).encode())
    except Refused as error:report['owner_status']=str(error)
    except Exception:report['owner_status']='owner-configuration-unavailable'
    return report

def record(role,pid,receipts,dry_run,external):
    if role not in ROLES or pid!=os.getppid():raise Refused('record-parent-mismatch')
    identity=birth(pid)
    config={key:value for key,value in os.environ.items() if key in KEYS}
    value={'version':2,'role':role,'pid':pid,'birth':identity,'boot_id':boot(),'dry_run':dry_run,'external':external,'config':config,
           'owner_identity':owner_identity(os.environ),'executable_sha256':file_hash(Path('/proc')/str(pid)/'exe')}
    policy=config.get('AUGMENTAGENT_NATIVE_VM_POLICY')
    value['policy_sha256']=digest(private(Path(policy),2*1024**2)) if policy else None
    data=json.dumps(value,sort_keys=True).encode()
    if len(data)>65536:raise Refused('configuration-exceeds-bound')
    receipts.parent.mkdir(parents=True,exist_ok=True)
    try:receipts.mkdir(mode=0o700)
    except FileExistsError:pass
    info=receipts.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid!=os.getuid() or info.st_mode&0o077:raise Refused('receipt-directory-invalid')
    lock=os.open(receipts,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
    try:
        fcntl.flock(lock,fcntl.LOCK_EX)
        path=receipts/(role+'.json')
        if path.exists() or path.is_symlink():
            previous=json.loads(private(path))
            if previous.get('version') not in (1,2) or previous.get('role')!=role:raise Refused('preserving-unmanaged-receipt')
            prior_pid=previous.get('pid')
            if isinstance(prior_pid,int) and prior_pid>0 and prior_pid!=pid:
                try:live=previous.get('boot_id')==boot() and previous.get('birth')==birth(prior_pid)
                except FileNotFoundError:live=False
                if live:raise Refused('preserving-live-receipt')
        temporary=None
        try:
            with tempfile.NamedTemporaryFile(dir=receipts,prefix='.effective-',delete=False) as stream:
                temporary=Path(stream.name);stream.write(data);stream.flush();os.fsync(stream.fileno())
            if birth(pid)!=identity:raise Refused('process-changed')
            os.replace(temporary,path)
            descriptor=os.open(receipts,os.O_RDONLY|os.O_DIRECTORY|os.O_CLOEXEC)
            try:os.fsync(descriptor)
            finally:os.close(descriptor)
        finally:
            if temporary is not None:temporary.unlink(missing_ok=True)
    finally:os.close(lock)

def activation_needed(selection,receipts,pid_for=service_pid):
    # Read-only signal for the updater. The worker reloads independently at a
    # completed tick/sweep boundary, so the updater never interrupts its work.
    try:policy,_,policy_sha,_=selected(selection)
    except FileNotFoundError:return False
    pid=pid_for('daemon')
    if pid<=0:raise Refused('daemon-not-running')
    value=json.loads(private(receipts/'daemon.json'))
    if value.get('version') not in (1,2) or value.get('role')!='daemon' or value.get('pid')!=pid or value.get('birth')!=birth(pid) or value.get('boot_id')!=boot():
        raise Refused('daemon-receipt-unverified')
    config=value.get('config',{})
    if config.get('AUGMENTAGENT_AUTOPR','').strip().lower() not in ('1','true'):return False
    if value.get('external') is not True:raise Refused('daemon-worker-mode-unverified')
    pending=config.get('AUGMENTAGENT_NATIVE_VM_POLICY')!=str(policy) or value.get('policy_sha256')!=policy_sha
    if pid_for('daemon')!=pid or birth(pid)!=value['birth']:raise Refused('process-changed')
    return pending

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary',type=Path);parser.add_argument('--selection',type=Path)
    parser.add_argument('--receipts',type=Path,required=True)
    parser.add_argument('--record',choices=ROLES);parser.add_argument('--pid',type=int)
    parser.add_argument('--activation-needed',action='store_true')
    parser.add_argument('--owner-identity',action='store_true')
    parser.add_argument('--workflow-store',action='store_true')
    parser.add_argument('--dry-run',action='store_true');parser.add_argument('--external',action='store_true')
    args=parser.parse_args()
    if args.workflow_store:
        try:print(json.dumps(workflow_store_configuration(args.receipts),sort_keys=True));return 0
        except Exception:
            print('worker-workflow-store-unavailable');return 1
    if args.activation_needed:
        try:return 10 if activation_needed(args.selection,args.receipts) else 0
        except Exception:return 1
    if args.record:
        try:record(args.record,args.pid,args.receipts,args.dry_run,args.external)
        except Exception:
            print('effective-configuration-record-failed');return 1
        return 0
    report=inspect_owner(args.binary,args.selection,args.receipts) if args.owner_identity else inspect(args.binary,args.selection,args.receipts)
    print(json.dumps(report,sort_keys=True))
    return 0

if __name__=='__main__':raise SystemExit(main())
