#!/usr/bin/python3
"""Private scripted GitHub boundary backed by real disposable Git objects."""
import base64, datetime, json, os, pathlib, runpy, subprocess, sys, urllib.parse
root = pathlib.Path(os.environ['AUTOSHIP_PARENT_FIXTURE'])
repo, remote = root/'repo', root/'remote.git'
p = root/'scenario.json'
s = json.loads(p.read_text())
a = sys.argv[1:]
repository, number, branch = 'owner/repo', 1607, 'agent-fix/issue-2'
def git(*args):
    return subprocess.check_output(['/usr/bin/git','-C',str(remote),*args],stderr=subprocess.PIPE,text=True).strip()
def save(): p.write_text(json.dumps(s))
def emit(value): print(json.dumps(value)); sys.exit(0)
def field(name, default=None):
    return next((v.split('=',1)[1] for v in a if v.startswith(name+'=')), default)
head, main = git('rev-parse','refs/heads/'+branch), git('rev-parse','refs/heads/main')
row = dict(url='https://github.com/owner/repo/pull/1607',number=number,id='PR_child',state=s.get('state','OPEN'),headRefName=branch,headRefOid=head,
    baseRefName=s['base'],baseRefOid=git('rev-parse','refs/heads/'+s['base']),isDraft=s.get('draft',True),
    isCrossRepository=False,headRepository=dict(name='repo',nameWithOwner=repository,owner=dict(login='owner')),headRepositoryOwner=dict(login='owner'),author=dict(login='owner'),
    title='Readiness becomes true',body='- complexity (scoping pass): simple\nOwner request: readiness becomes true.',
    mergeCommit=dict(oid=s.get('merge_commit')),labels=[],mergeable='MERGEABLE',closingIssuesReferences=[dict(number=2,url='https://github.com/owner/repo/issues/2')])
with (root/'calls.jsonl').open('a') as log:
    log.write(json.dumps({'command':a[:3], 'mutation':any(v.startswith('query=mutation ') for v in a)})+'\n')
if a[:2]==['repo','view']:
    if '.viewerPermission' in a: print('ADMIN')
    elif '.owner.login' in a: print('owner')
    else: print(repository)
    sys.exit(0)
if a[:2]==['issue','list']: emit([])
if a[:2]==['issue','view']:
    issue=dict(number=2,state='OPEN',title='Readiness becomes true',body='Readiness becomes true.',author=dict(login='owner'),labels=[])
    if '--jq' in a: print(issue.get(a[a.index('--jq')+1].lstrip('.'),''));sys.exit(0)
    emit(issue)
if a[:2]==['pr','list']:
    if '--head' in a and a[a.index('--head')+1]=='parent':
        emit([dict(number=1606,state='MERGED',isCrossRepository=False,mergeCommit=dict(oid=s['parent_merge']))])
    emit([row] if row['state']=='OPEN' else [])
if a[:2]==['pr','view']:
    if '--jq' in a: print(row.get(a[a.index('--jq')+1].lstrip('.'),''));sys.exit(0)
    emit(row)
if a[:2]==['pr','diff']: print('behavior.py');sys.exit(0)
if a[:2]==['pr','checks']:
    emit([dict(name=n,state='SUCCESS' if s['ci_head']==head else 'PENDING') for n in ['privacy','node-release']])
if a[:2]==['pr','edit']:
    assert a[-2:]==['--base','main'];s['base']='main';s['retargets']=s.get('retargets',0)+1;save();sys.exit(0)
if a[:2]==['pr','ready']:
    s['draft']='--undo' in a;save();sys.exit(0)
if a[0]!='api': raise AssertionError('unexpected command '+repr(a[:3]))
endpoint=a[1]
if endpoint=='graphql':
    query=field('query','')
    if 'AutoshipViewer' in query: emit(dict(data=dict(viewer=dict(login='owner'))))
    if 'AutoshipDirectMergePolicy' in query: emit(dict(data=dict(repository=dict(mergeQueue=None))))
    if query.startswith('mutation AutoshipMerge'):
        assert field('id')=='PR_child' and field('head')==head and s['base']=='main' and s['ci_head']==head
        subprocess.run(['/usr/bin/git','-C',str(remote),'merge-base','--is-ancestor',main,head],check=True)
        tree=git('rev-parse',head+'^{tree}')
        commit=git('-c','user.name=Fixture','-c','user.email=fixture@example.invalid','commit-tree',tree,'-p',main,'-m',field('body'))
        git('update-ref','refs/heads/main',commit,main)
        s['state']='MERGED';s['merges']=s.get('merges',0)+1;s['merged_head']=head;s['merge_commit']=commit;save()
        emit(dict(data=dict(mergePullRequest=dict(clientMutationId=field('request'),pullRequest=dict(id='PR_child',number=number,state='MERGED',headRefOid=head,mergedAt=datetime.datetime.now(datetime.timezone.utc).isoformat(),mergeCommit=dict(oid=commit,message=field('body')),mergedBy=dict(login='controller'))))))
    raise AssertionError('unexpected GraphQL operation')
if endpoint.endswith('/rules/branches/main'):
    emit([dict(type='required_status_checks',ruleset_id=1,parameters=dict(strict_required_status_checks_policy=True,required_status_checks=[dict(context='privacy')]))])
if '/rulesets/' in endpoint: emit(dict(enforcement='active',bypass_actors=[]))
if '/issues/' in endpoint: emit(dict(number=int(endpoint.rsplit('/',1)[1]),state='open',body='Readiness becomes true.',user=dict(login='owner'),labels=[]))
if endpoint.endswith('/git/ref/heads/main') or endpoint.endswith('/branches/main'):
    if '--jq' in a: print(main);sys.exit(0)
    emit(dict(object=dict(sha=main),commit=dict(sha=main)))
if endpoint.endswith('/pulls/1607'):
    emit(dict(number=number,state=s.get('state','OPEN').lower(),draft=row['isDraft'],changed_files=1,
        head=dict(sha=head,ref=branch),base=dict(sha=main,ref=s['base'],repo=dict(full_name=repository)),merged=s.get('state')=='MERGED',
        merge_commit_sha=s.get('merge_commit'),merged_by=dict(login='controller')))
if '/pulls/1607/files' in endpoint: emit([dict(filename='behavior.py')])
if '/contents/' in endpoint:
    path, _, query=endpoint.split('/contents/',1)[1].partition('?')
    ref=urllib.parse.parse_qs(query)['ref'][0]
    data=subprocess.check_output(['/usr/bin/git','-C',str(remote),'show',ref+':'+path])
    emit(dict(type='file',encoding='base64',content=base64.b64encode(data).decode()))
policy=json.loads((root/'policy-root/scripts/ci-contract.json').read_text())
reader=runpy.run_path(str(root/'policy-root/scripts/autoship-hosted-ci.py'))
expected=reader['expected_jobs'](policy,['behavior.py'],'pull_request')
paths=sorted(expected)
if '/actions/runs?' in endpoint:
    runs=[dict(id=100+i,run_attempt=1,head_sha=s['ci_head'],event='pull_request',path=path,repository=dict(full_name=repository),pull_requests=[dict(number=number)]) for i,path in enumerate(paths)]
    emit(dict(total_count=len(runs),workflow_runs=runs))
if '/actions/runs/' in endpoint and '/jobs?' in endpoint:
    run=int(endpoint.split('/actions/runs/')[1].split('/')[0]);path=paths[run-100]
    emit(dict(jobs=[dict(id=run*100+i,run_id=run,run_attempt=1,head_sha=s['ci_head'],name=name,status='completed',conclusion='success') for i,name in enumerate(sorted(expected[path]))]))
if endpoint.endswith('/pulls/1607/commits'): emit([dict(sha=head)])
raise AssertionError('unexpected API '+endpoint.split('?',1)[0])
