#!/usr/bin/env python3
"""Shared declared CI commands. Local execution requires controller resource/storage admission.
No host setup mutation is performed by plan; foreign-platform/setup jobs are
explicit remote requirements. GitHub and the local controller execute identical bodies.
"""
import argparse,hashlib,itertools,json,os,platform,re,shlex,subprocess,sys
from pathlib import Path
RUNNER=Path(globals().get('__file__','scripts/ci-contract.py')).resolve()
DEFAULT=RUNNER.with_name('ci-contract.json')


def digest(value):return hashlib.sha256(json.dumps(value,sort_keys=True,separators=(',',':')).encode()).hexdigest()
def load(args):
    raw=args.contract_json if args.contract_json else Path(args.contract or DEFAULT).read_text()
    args.contract_sha256=hashlib.sha256(raw.encode()).hexdigest()
    value=json.loads(raw)
    if value.get('version')!=1 or not isinstance(value.get('commands'),dict) or not isinstance(value.get('workflows'),dict):raise ValueError('unsupported CI contract')
    return value

def verify(contract,root):
    actual={str(p.relative_to(root)) for p in [*(root/'.github/workflows').glob('*.yml'),*(root/'.github/workflows').glob('*.yaml')]}
    expected=set(contract['workflows'])
    return sorted(actual^expected | {name for name,row in contract['workflows'].items() if name in actual and hashlib.sha256((root/name).read_bytes()).hexdigest()!=row['sha256']})

def match_path(path,pattern):
    expression=re.escape(pattern).replace(r'\*\*/','(?:.*/)?').replace(r'\*\*','.*').replace(r'\*','[^/]*').replace(r'\?','[^/]')
    return re.fullmatch(expression,path) is not None

def applicable(events,changed):
    if not changed:return True
    trigger=events.get('pull_request') or {}
    patterns=trigger.get('paths')
    if patterns:
        for path in changed:
            matched=False
            for pattern in patterns:
                if match_path(path,pattern.lstrip('!')):matched=not pattern.startswith('!')
            if matched:return True
        return False
    ignored=trigger.get('paths-ignore',[])
    return not ignored or any(not any(match_path(path,p) for p in ignored) for path in changed)

def matrices(job):
    matrix=job.get('strategy',{}).get('matrix',{})
    if matrix.get('include'):raise ValueError('shared CI prerequisite unavailable: unsupported matrix include semantics')
    keys=[k for k in matrix if k not in ('include','exclude')]
    rows=[dict(zip(keys,v)) for v in itertools.product(*(matrix[k] for k in keys))] if keys else [{}]
    rows=[row for row in rows if not any(all(row.get(k)==v for k,v in e.items()) for e in matrix.get('exclude',[]))]
    rows.extend(matrix.get('include',[]))
    return rows

def resolve(value,context):
    if not isinstance(value,str):return str(value)
    def replace(match):
        key=match.group(1).strip()
        if key not in context:raise ValueError('unresolved GitHub input '+key)
        return str(context[key])
    return re.sub(r'\$\{\{\s*(.*?)\s*\}\}',replace,value)

def condition(value,osname):
    if value is None or value in ('always()','success()'):return True
    if value=="runner.os == 'Linux'":return osname=='linux'
    if value=="runner.os == 'macOS'":return osname=='darwin'
    raise ValueError('unresolved job/step condition '+str(value))

def host_setup(body):return bool(re.search(r'\b(sudo|apt-get|brew install)\b|playwright\s+install(?:-deps)?\b|--with-deps',body))

def tool_prerequisites(steps,target):
    result=[]
    for step in steps:
        try:
            if not condition(step.get('if'),target):continue
        except ValueError:
            result.append({'kind':'unsupported_action','action':'unresolved prerequisite condition'});continue
        action=step.get('uses','').split('@')[0];options=step.get('with',{})
        if action in ('actions/checkout','Swatinem/rust-cache','actions/cache','actions/upload-artifact'):continue
        mappings={'actions/setup-node':('node','node-version'),'actions/setup-python':('python3','python-version'),
                  'actions/setup-go':('go','go-version'),'dtolnay/rust-toolchain':('rustc','toolchain'),
                  'denoland/setup-deno':('deno','deno-version')}
        if action in mappings:
            tool,key=mappings[action];version=str(options.get(key,''))
            if action=='dtolnay/rust-toolchain' and not version:version='rust-toolchain.toml'
            result.append({'kind':'tool','tool':tool,'version':version,'action':step['uses']})
            for component in str(options.get('components','')).replace(' ','').split(','):
                if component:result.append({'kind':'rust_component','component':component})
        elif action:result.append({'kind':'unsupported_action','action':step['uses']})
        body=step.get('_body','')
        if host_setup(body):result.append({'kind':'system_setup','command_sha256':hashlib.sha256(body.encode()).hexdigest(),'body':body})
    return result

def prerequisite_check(requirements,root):
    missing=[]
    for requirement in requirements:
        if requirement['kind']=='system_setup':
            # Verify declared apt packages read-only. Never sudo/install locally.
            bodies=requirement['body'].replace('\\\n',' ')
            match=re.search(r'apt-get\s+(?:-[\w=]+\s+)*install\s+([^\n]+)',bodies)
            if not match:
                if re.search(r'playwright\s+install(?:-deps)?\s+(?:--with-deps\s+)?chrome\b',bodies):
                    import shutil
                    if not any(shutil.which(name) for name in ['google-chrome','google-chrome-stable','chromium']):missing.append('controlled Chrome provisioning')
                else:missing.append('system setup requires controlled provisioning')
                continue
            packages=[x for x in shlex.split(match.group(1)) if not x.startswith('-')]
            for package in packages:
                if not re.fullmatch(r'[A-Za-z0-9.+:-]+',package):missing.append('unresolved package declaration');continue
                try:
                    done=subprocess.run(['dpkg-query','-W','-f=${Status}',package],capture_output=True,text=True,timeout=10)
                    if done.returncode or done.stdout.strip()!='install ok installed':missing.append('package:'+package)
                except (OSError,subprocess.TimeoutExpired):missing.append('package:'+package)
            continue
        if requirement['kind']=='rust_component':
            try:
                done=subprocess.run(['rustup','component','list','--installed'],capture_output=True,text=True,timeout=10,env={**os.environ,'RUSTUP_AUTO_INSTALL':'0'})
                if done.returncode or not any(line.startswith(requirement['component']+'-') for line in done.stdout.splitlines()):missing.append('rust component:'+requirement['component'])
            except (OSError,subprocess.TimeoutExpired):missing.append('rust component:'+requirement['component'])
            continue
        if requirement['kind']!='tool':missing.append(requirement['action']);continue
        tool= requirement['tool'];version=requirement['version']
        if version=='rust-toolchain.toml':
            try:
                import tomllib
                version=tomllib.loads((root/'rust-toolchain.toml').read_text())['toolchain']['channel']
            except (OSError,KeyError,ValueError):missing.append('rust toolchain declaration');continue
        try:
            args=[tool,'version'] if tool=='go' else [tool,'--version']
            done=subprocess.run(args,capture_output=True,text=True,timeout=10,env={**os.environ,'RUSTUP_AUTO_INSTALL':'0'})
            observed=re.search(r'\b(?:v|go)?(\d+(?:\.\d+){1,2})',done.stdout)
            expected=version.lstrip('v').removesuffix('.x')
            if done.returncode or not observed or not re.fullmatch(r'\d+(?:\.\d+)*',expected) or not (observed[1]==expected or observed[1].startswith(expected+'.')):missing.append(tool+':'+version)
        except (OSError,subprocess.TimeoutExpired):missing.append(tool+':'+version)
    return missing

def plan(contract,args):
    checks=[];root=Path(args.root).resolve();shared=any(p in ('scripts/ci-contract.py','scripts/ci-contract.json','scripts/ci-contract-sync.py') for p in args.changed_file)
    drift=verify(contract,root)
    if drift:checks.append({'id':'contract-authority-change','status':'remote_required','reason':'workflow inventory differs from controller contract','paths':drift,'workflow':'controller-contract','job':'authority-change','matrix':{},'platform':args.platform,'working_directory':'.','environment':{},'shell':'bash','body':'','argv':[],'prerequisites':[]})
    for filename,workflow in contract['workflows'].items():
        if workflow['role']!='validation':continue
        applies=shared or applicable(workflow['events'],args.changed_file)
        for job_id,job in workflow['jobs'].items():
            for matrix in matrices(job):
                context={'matrix.'+k:v for k,v in matrix.items()};context.update({'github.workspace':str(root),'github.event.pull_request.base.sha':args.base,'github.event.pull_request.head.sha':args.head})
                try:runner=resolve(job['runs-on'],context)
                except ValueError:runner='unresolved'
                target='darwin' if runner.startswith('macos-') else 'linux' if runner.startswith('ubuntu-') else 'unsupported'
                requirements=tool_prerequisites([{**s,'_body':contract['commands'].get(s.get('command_id'),{}).get('body','')} for s in job['steps']],target)
                missing=prerequisite_check(requirements,root) if applies and target==args.platform and args.probe_prerequisites else []
                for index,step in enumerate(job['steps']):
                    identifier=step.get('command_id')
                    if not identifier:continue
                    command=contract['commands'][identifier];status='ready';reason='';environment={}
                    working=step.get('working-directory',job.get('defaults',{}).get('run',{}).get('working-directory',workflow.get('defaults',{}).get('run',{}).get('working-directory','.')))
                    try:
                        environment={key:resolve(value,context) for key,value in {**workflow.get('env',{}),**job.get('env',{}),**step.get('env',{})}.items()}
                        working=resolve(working,context)
                        if not applies or not condition(job.get('if'),target) or not condition(step.get('if'),target):status='not_applicable';reason='workflow paths or declared condition'
                        elif target!=args.platform:status='remote_required';reason='requires '+runner
                        elif host_setup(command['body']):status='remote_required';reason='host provisioning is never executed from candidate CI'
                        elif missing:status='prerequisite_hold';reason='; '.join(missing)
                        elif requirements and not args.probe_prerequisites:status='prerequisite_hold';reason='prerequisites not measured'
                    except ValueError as error:status='remote_required';reason=str(error)
                    variant=identifier+(' @'+json.dumps(matrix,sort_keys=True,separators=(',',':')) if matrix else '')
                    checks.append({'id':variant,'command_id':identifier,'workflow':filename,'job':job_id,'matrix':matrix,'platform':target,'status':status,'reason':reason,
                        'body':command['body'],'shell':command['shell'],'working_directory':working,'environment':environment,'prerequisites':requirements,
                        'argv':[sys.executable,'-I',str(RUNNER),'step','--id',variant,'--input-base',args.base,'--input-head',args.head]})
    return {'version':1,'contract_sha256':getattr(args,'contract_sha256',digest(contract)),'head':args.head,'base':args.base,'checks':checks,'complete':not any(c['status']=='prerequisite_hold' for c in checks),'remote_pending':any(c['status']=='remote_required' for c in checks)}

def main():
    parser=argparse.ArgumentParser();parser.add_argument('--contract');parser.add_argument('--contract-json')
    sub=parser.add_subparsers(dest='operation',required=True)
    p=sub.add_parser('plan');p.add_argument('--head',required=True);p.add_argument('--base',required=True);p.add_argument('--platform',choices=['linux','darwin'],default=sys.platform);p.add_argument('--root',default='.');p.add_argument('--changed-file',action='append',default=[]);p.add_argument('--probe-prerequisites',action='store_true')
    p=sub.add_parser('step');p.add_argument('--id',required=True);p.add_argument('--input-base');p.add_argument('--input-head');p.add_argument('--hosted',action='store_true');p.add_argument('--state-dir',type=Path)
    p=sub.add_parser('verify');p.add_argument('--root',default='.')
    args=parser.parse_args();contract=load(args)
    if args.operation=='plan':print(json.dumps(plan(contract,args),sort_keys=True));return 0
    if args.operation=='verify':
        mismatch=verify(contract,Path(args.root).resolve());print(json.dumps({'valid':not mismatch,'changed':mismatch}));return bool(mismatch)
    identifier,_,matrix_json=args.id.partition(' @')
    command=contract['commands'][identifier];environment=os.environ.copy();root=Path.cwd()
    if not args.hosted:
        matrix=json.loads(matrix_json) if matrix_json else {}
        workflow_name,job_name,step_index=identifier.split('/')
        matches=[row for path,row in contract['workflows'].items() if Path(path).stem==workflow_name]
        if len(matches)!=1:raise ValueError('ambiguous workflow identity')
        workflow=matches[0];job=workflow['jobs'][job_name];step=job['steps'][int(step_index)]
        if step['command_id']!=identifier or matrix not in matrices(job):raise ValueError('shared CI prerequisite unavailable: invalid matrix identity')
        context={'matrix.'+k:v for k,v in matrix.items()};context.update({'github.workspace':str(root),'github.event.pull_request.base.sha':args.input_base or '', 'github.event.pull_request.head.sha':args.input_head or ''})
        runner=resolve(job['runs-on'],context);host='darwin' if runner.startswith('macos-') else 'linux' if runner.startswith('ubuntu-') else 'unsupported'
        if host!=sys.platform or not condition(job.get('if'),sys.platform) or not condition(step.get('if'),sys.platform):raise ValueError('shared CI prerequisite unavailable: platform or condition')
        requirements=tool_prerequisites([{**s,'_body':contract['commands'].get(s.get('command_id'),{}).get('body','')} for s in job['steps']],host)
        missing=prerequisite_check(requirements,root)
        if missing:raise ValueError('shared CI prerequisite unavailable: '+'; '.join(missing))
        if host_setup(command['body']):raise ValueError('shared CI prerequisite unavailable: host setup cannot execute locally')
        environment.update({key:resolve(value,context) for key,value in {**workflow.get('env',{}),**job.get('env',{}),**step.get('env',{})}.items()})
        environment.update({'CI':'true','GITHUB_ACTIONS':'true','GITHUB_WORKSPACE':str(root)})
        if True:  # Every hosted job has private RUNNER_TEMP, including scanner/install consumers.
            if args.state_dir is None:raise ValueError('shared CI prerequisite unavailable: cross-step state directory')
            state=args.state_dir.resolve();info=state.stat()
            if args.state_dir.absolute()!=state or state.is_relative_to(root) or info.st_uid!=os.getuid() or info.st_mode&0o077:raise ValueError('shared CI prerequisite unavailable: state must be private outside checkout')
            runtime=state/('runtime-'+digest({'workflow':workflow_name,'job':job_name,'matrix':matrix,'head':args.input_head,'base':args.input_base,'root':str(root)})[:16]);
            if runtime.is_symlink():raise ValueError('invalid shared CI runtime directory')
            runtime.mkdir(mode=0o700,exist_ok=True)
            envfile=runtime/'github-env';environment.update({'RUNNER_TEMP':str(runtime),'GITHUB_ENV':str(envfile)})
            if envfile.exists():
                if envfile.is_symlink() or envfile.stat().st_size>4096:raise ValueError('invalid shared CI state')
                for line in envfile.read_text().splitlines():
                    key,_,value=line.partition('=')
                    if key!='AUGMENTAGENT_WA_SIDECAR_TEST_BIN' or value!=str(runtime/'wa-sidecar'):raise ValueError('unexpected shared CI environment output')
                    environment[key]=value

        working=resolve(step.get('working-directory',job.get('defaults',{}).get('run',{}).get('working-directory',workflow.get('defaults',{}).get('run',{}).get('working-directory','.'))),context)
        target=(root/working).resolve()
        if not target.is_relative_to(root):raise ValueError('shared CI prerequisite unavailable: work directory outside checkout')
    else:target=root
    if args.input_base:environment['CI_CONTRACT_BASE']=args.input_base
    if args.input_head:environment['CI_CONTRACT_HEAD']=args.input_head
    for key in ['CI_CONTRACT_BASE','CI_CONTRACT_HEAD']:
        if '${'+key+'}' in command['body'] and not environment.get(key):raise ValueError('shared CI prerequisite unavailable: missing '+key)
    if command['shell']!='bash':raise ValueError('unsupported declared shell')
    return subprocess.call(['/bin/bash','--noprofile','--norc','-e','-o','pipefail','-c',command['body']],env=environment,cwd=target)
if __name__=='__main__':
    try:raise SystemExit(main())
    except (ValueError,KeyError,OSError) as error:
        print(json.dumps({'status':'prerequisite_hold','reason':str(error)}),file=sys.stderr);raise SystemExit(2)
