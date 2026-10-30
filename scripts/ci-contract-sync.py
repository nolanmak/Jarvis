#!/usr/bin/env python3
"""Generate CI command inventory from workflows. Requires PyYAML; no CI execution.
Run after adding/editing workflow steps; command bodies live only in ci-contract.json
once a step has been migrated. New workflows require an explicit role below.
"""
import hashlib,json,re,sys
from pathlib import Path
import yaml
ROOT=Path(__file__).resolve().parents[1]
CONTRACT=ROOT/'scripts/ci-contract.json'
EXCLUDED={'autonomous-code-scanner.yml':'scheduled autonomous scanner and issue reporting',
          'issue-to-pr.yml':'issue-driven implementation and publication, not PR validation',
          'claude-pr-review.yml':'optional comment-only external review'}

PR_CONCURRENCY = {
 'group': "${{ github.workflow }}-${{ github.event.pull_request.number || github.ref }}-${{ github.event_name == 'pull_request' && 'pr' || github.run_id }}",
 'cancel-in-progress': "${{ github.event_name == 'pull_request' }}",
}
def validate_concurrency(filename,data):
    # Platform's existing explicit policy predates this rollout and is retained.
    expected={'group':'platform-${{ github.ref }}','cancel-in-progress':True} if filename=='platform.yml' else PR_CONCURRENCY
    if data.get('concurrency')!=expected:
        raise ValueError('validation workflow requires reviewed PR concurrency policy: '+filename)

def generate(root=ROOT):
    path=root/'scripts/ci-contract.json'
    old=json.loads(path.read_text()) if path.exists() else {'commands':{}}
    commands={};workflows={}
    for source in sorted([*(root/'.github/workflows').glob('*.yml'),*(root/'.github/workflows').glob('*.yaml')]):
        raw=source.read_text();data=yaml.safe_load(raw);node=yaml.compose(raw)
        key=str(source.relative_to(root))
        events=data.get('on',data.get(True,{}))
        if source.name in EXCLUDED:
            workflows[key]={'role':'excluded','reason':EXCLUDED[source.name],'sha256':hashlib.sha256(raw.encode()).hexdigest()};continue
        if not isinstance(events,dict) or 'pull_request' not in events:raise ValueError('unclassified workflow '+key)
        validate_concurrency(source.name,data)
        jobs_node=next(value for name,value in node.value if name.value=='jobs')
        replacements=[];jobs={}
        for job_name,job_node in jobs_node.value:
            job=data['jobs'][job_name.value]
            if job.get('strategy',{}).get('matrix',{}).get('include'):raise ValueError('unsupported matrix include semantics in '+key)
            steps_node=next(value for name,value in job_node.value if name.value=='steps')
            metadata={name:value for name,value in job.items() if name!='steps'};metadata['steps']=[]
            for index,step_node in enumerate(steps_node.value):
                step=job['steps'][index];row=dict(step)
                if 'run' in step:
                    identifier=f'{source.stem}/{job_name.value}/{index}'
                    run_node=next(value for name,value in step_node.value if name.value=='run')
                    previous=re.search(r'ci-contract\.py.*?--id ([A-Za-z0-9_./-]+)',step['run'])
                    if previous:
                        body=old['commands'][previous.group(1)]['body']
                    else:body=step['run']
                    body=body.replace('${{ github.event.pull_request.base.sha }}','${CI_CONTRACT_BASE}').replace('${{ github.event.pull_request.head.sha }}','${CI_CONTRACT_HEAD}')
                    if '${{' in body:raise ValueError('unresolved expression in '+identifier)
                    if identifier in commands:raise ValueError('duplicate workflow stem '+source.stem)
                    commands[identifier]={'body':body,'shell':step.get('shell',job.get('defaults',{}).get('run',{}).get('shell',data.get('defaults',{}).get('run',{}).get('shell','bash')))}
                    if previous and 'vm_provisioning' in old['commands'][previous.group(1)]:
                        commands[identifier]['vm_provisioning']=old['commands'][previous.group(1)]['vm_provisioning']
                    wrapper=f'python3 "${{{{ github.workspace }}}}/scripts/ci-contract.py" step --hosted --id {identifier}'
                    if '${CI_CONTRACT_BASE}' in body:wrapper+=' --input-base "${{ github.event.pull_request.base.sha }}" --input-head "${{ github.event.pull_request.head.sha }}"'
                    replacement=json.dumps(wrapper)+('' if raw[run_node.end_mark.index:run_node.end_mark.index+1]=='\n' else '\n')
                    replacements.append((run_node.start_mark.index,run_node.end_mark.index,replacement))
                    row.pop('run');row['command_id']=identifier
                metadata['steps'].append(row)
            jobs[job_name.value]=metadata
        for begin,end,value in reversed(replacements):raw=raw[:begin]+value+raw[end:]
        # Add shared command inputs to every path-filtered validation trigger.
        current=yaml.compose(raw);extra=[]
        event_node=next(value for name,value in current.value if name.value=='on')
        for event,config in event_node.value:
            if not isinstance(config,yaml.MappingNode):continue
            for name,paths in config.value:
                if name.value!='paths':continue
                present=[item.value for item in paths.value]
                missing=[v for v in ['scripts/ci-contract.py','scripts/ci-contract.json','scripts/ci-contract-sync.py','scripts/tests/ci_contract_test.py'] if v not in present]
                if missing:
                    indent=' '*(paths.value[0].start_mark.column-2)
                    extra.append((raw.rfind('\n',0,paths.end_mark.index)+1,''.join(indent+'- '+json.dumps(v)+'\n' for v in missing)))
        for position,value in reversed(extra):raw=raw[:position]+value+raw[position:]
        raw=raw.rstrip()+"\n"
        parsed=yaml.safe_load(raw)
        source.write_text(raw)
        workflows[key]={'role':'validation','sha256':hashlib.sha256(raw.encode()).hexdigest(),
            'events':parsed.get('on',parsed.get(True,{})),'env':data.get('env',{}),'defaults':data.get('defaults',{}),'jobs':jobs}
    result={'version':1,'workflows':workflows,'commands':commands}
    path.write_text(json.dumps(result,indent=2,sort_keys=True)+'\n')
    return result
if __name__=='__main__':generate()
