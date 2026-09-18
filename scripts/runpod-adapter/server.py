import base64, binascii, contextlib, http.server, json, os, re, secrets, sqlite3, stat, threading, time, urllib.request, urllib.error, uuid
from pathlib import Path

API_KEY = os.environ['RUNPOD_API_KEY']
CLIENT_KEY = os.environ['ADAPTER_API_KEY']
ROUTES = Path('/app/routes.json')
JOURNAL = Path(os.environ.get('RUNPOD_ADAPTER_JOURNAL', '/app/state/jobs.sqlite3'))
MAX_IN_FLIGHT = threading.BoundedSemaphore(2)

class DuplicateRequestError(ValueError):
    pass

class JobJournal:
    """Durable, prompt-free Runpod job lifecycle. A reused request key never resubmits."""
    def __init__(self, path):
        self.path = Path(path)
        self.path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        parent = self.path.parent.lstat()
        if not stat.S_ISDIR(parent.st_mode) or parent.st_uid != os.geteuid() or parent.st_mode & 0o077:
            raise ValueError('job journal directory must be owner-private')
        try:
            fd = os.open(self.path, os.O_CREAT | os.O_EXCL | os.O_RDWR | os.O_NOFOLLOW, 0o600)
            os.close(fd)
        except FileExistsError:
            info = self.path.lstat()
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o077:
                raise ValueError('job journal must be an owner-private regular file')
        with self._connect() as db:
            db.execute('''CREATE TABLE IF NOT EXISTS jobs (
                request_id TEXT PRIMARY KEY, model TEXT NOT NULL, route TEXT NOT NULL,
                job_id TEXT, state TEXT NOT NULL, updated_at INTEGER NOT NULL)''')

    @contextlib.contextmanager
    def _connect(self):
        db = sqlite3.connect(self.path, timeout=5)
        try:
            db.execute('PRAGMA synchronous=FULL')
            with db:
                yield db
        finally:
            db.close()

    def start(self, request_id, model, route):
        with self._connect() as db:
            try:
                db.execute('INSERT INTO jobs VALUES (?, ?, ?, NULL, ?, ?)',
                           (request_id, model, route, 'SUBMITTING', int(time.time())))
            except sqlite3.IntegrityError as error:
                raise DuplicateRequestError('request key already used; inspect job journal before retry') from error

    def submitted(self, request_id, job_id):
        with self._connect() as db:
            changed = db.execute('UPDATE jobs SET job_id=?, state=?, updated_at=? WHERE request_id=? AND state=?',
                                 (job_id, 'SUBMITTED', int(time.time()), request_id, 'SUBMITTING')).rowcount
            if changed != 1:
                raise ValueError('job submission has no matching journal entry')

    def finish(self, request_id, state):
        with self._connect() as db:
            changed = db.execute('UPDATE jobs SET state=?, updated_at=? WHERE request_id=?',
                                 (state, int(time.time()), request_id)).rowcount
            if changed != 1:
                raise ValueError('job has no matching journal entry')

    def get(self, request_id):
        with self._connect() as db:
            row = db.execute('SELECT model, route, job_id, state FROM jobs WHERE request_id=?',
                             (request_id,)).fetchone()
        return dict(zip(('model', 'route', 'job_id', 'state'), row)) if row else None

def cancel_job(journal, request_id, base, rpc_call= None):
    """Only an explicit matching Runpod response confirms cancellation."""
    rpc_call = rpc if rpc_call is None else rpc_call
    job = journal.get(request_id)
    if not job or not job['job_id']:
        journal.finish(request_id, 'CANCELLATION_UNKNOWN')
        return 'CANCELLATION_UNKNOWN'
    journal.finish(request_id, 'CANCELLATION_REQUESTED')
    try:
        result = rpc_call(base + '/cancel/' + job['job_id'], {})
        if result.get('id') == job['job_id'] and result.get('status') in ('CANCELLED', 'COMPLETED', 'FAILED', 'TIMED_OUT'):
            state = result['status']
        else:
            state = 'CANCELLATION_UNKNOWN'
    except Exception:
        state = 'CANCELLATION_UNKNOWN'
    journal.finish(request_id, state)
    return state

def request(url, data=None, timeout=45):
    req = urllib.request.Request(url, data=None if data is None else json.dumps(data).encode(), headers={'Authorization': 'Bearer '+API_KEY, 'Content-Type':'application/json'})
    return urllib.request.urlopen(req, timeout=timeout)

def rpc(url,data=None):
    with request(url,data) as r:return json.load(r)

def normalize(output, model, request_id):
    if isinstance(output,list):output=output[-1] if output else {}
    if isinstance(output,str):output=json.loads(output)
    if 'error' in output:raise RuntimeError(str(output['error']))
    message=output.get('message',{'role':'assistant','content':output.get('response','')}).copy()
    message.pop('thinking',None)
    if output.get('message',{}).get('thinking'):message['reasoning_content']=output['message']['thinking']
    calls=message.get('tool_calls',[])
    for c in calls:
        c.setdefault('id','call_'+uuid.uuid4().hex[:16]);c.setdefault('type','function')
        f=c.get('function',{})
        if not isinstance(f.get('arguments'),str):f['arguments']=json.dumps(f.get('arguments',{}))
    prompt=output.get('prompt_eval_count',0);completion=output.get('eval_count',0)
    return {'id':request_id,'object':'chat.completion','created':int(time.time()),'model':model,'choices':[{'index':0,'message':message,'finish_reason':'tool_calls' if calls else ('length' if output.get('done_reason')=='length' else 'stop')}],'usage':{'prompt_tokens':prompt,'completion_tokens':completion,'total_tokens':prompt+completion}}

def normalize_messages(messages):
    """Translate OpenAI chat message parts into Ollama's chat wire format.

    9Router converts Responses calls to Chat Completions but keeps content as
    typed parts. Ollama requires a string plus optional base64 images.
    """
    if not isinstance(messages, list):
        raise ValueError('messages must be an array')
    names = {}
    result = []
    for message in messages:
        if not isinstance(message, dict) or message.get('role') not in ('system','developer','user','assistant','tool'):
            raise ValueError('unsupported message role')
        content = message.get('content')
        images = []
        if content is None:
            content = ''
        elif isinstance(content, list):
            parts = []
            for part in content:
                if not isinstance(part, dict):
                    raise ValueError('invalid content part')
                if part.get('type') in ('text','input_text','output_text') and isinstance(part.get('text'), str):
                    parts.append(part['text'])
                elif part.get('type') == 'image_url':
                    image_url = part.get('image_url')
                    url = image_url.get('url') if isinstance(image_url, dict) else image_url
                    if not isinstance(url, str) or not url.startswith('data:image/') or ';base64,' not in url:
                        raise ValueError('only inline image data is supported')
                    encoded = url.split(';base64,',1)[1]
                    try:
                        base64.b64decode(encoded, validate=True)
                    except (binascii.Error, ValueError) as error:
                        raise ValueError('invalid inline image data') from error
                    images.append(encoded)
                else:
                    raise ValueError('unsupported content part')
            content = '\n'.join(parts)
        elif not isinstance(content, str):
            raise ValueError('invalid message content')
        role = 'system' if message['role'] == 'developer' else message['role']
        normalized = {'role':role, 'content':content}
        if images:
            normalized['images'] = images
        calls = message.get('tool_calls')
        if calls is not None:
            if not isinstance(calls, list):
                raise ValueError('tool_calls must be an array')
            normalized_calls = []
            for call in calls:
                function = call.get('function') if isinstance(call, dict) else None
                if not isinstance(function, dict) or not isinstance(function.get('name'), str):
                    raise ValueError('invalid tool call')
                arguments = function.get('arguments', {})
                if isinstance(arguments, str):
                    try:
                        arguments = json.loads(arguments)
                    except json.JSONDecodeError as error:
                        raise ValueError('invalid tool arguments') from error
                if not isinstance(arguments, dict):
                    raise ValueError('tool arguments must be an object')
                normalized_calls.append({'function':{'name':function['name'],'arguments':arguments}})
                if isinstance(call.get('id'), str):
                    names[call['id']] = function['name']
            normalized['tool_calls'] = normalized_calls
        if message['role'] == 'tool':
            name = message.get('name') or names.get(message.get('tool_call_id'))
            if not isinstance(name, str) or not name:
                raise ValueError('tool response has no matching tool call')
            normalized['tool_name'] = name
        if role == 'system':
            if images or calls:
                raise ValueError('system messages cannot contain images or tool calls')
            if result and result[-1]['role'] != 'system':
                raise ValueError('system message must precede conversation messages')
            if result:
                result[0]['content'] += '\n\n' + content
            else:
                result.append(normalized)
        else:
            result.append(normalized)
    return result

def stream_chunks(response):
    """Emit OpenAI-compatible deltas in the order Responses translators expect."""
    choice = response['choices'][0]
    message = choice['message']
    base = {k:v for k,v in response.items() if k not in ('choices','usage')}
    base['object'] = 'chat.completion.chunk'
    def chunk(delta, finish=None):
        return {**base, 'choices':[{'index':0,'delta':delta,'finish_reason':finish}]}
    yield chunk({'role':'assistant'})
    if message.get('reasoning_content'):
        yield chunk({'reasoning_content':message['reasoning_content']})
    if message.get('content'):
        yield chunk({'content':message['content']})
    for index, call in enumerate(message.get('tool_calls',[])):
        yield chunk({'tool_calls':[{'index':index, **call}]})
    final = chunk({}, choice['finish_reason'])
    final['usage'] = response['usage']
    yield final

def predict_limit(body, route):
    requested = body.get('max_completion_tokens', body.get('max_tokens', 2048))
    maximum = route.get('max_output_tokens', 2048)
    if type(requested) is not int or requested <= 0 or type(maximum) is not int or maximum <= 0:
        raise ValueError('invalid output token limit')
    return min(requested, maximum)

def public_error(error):
    # Worker/provider errors may echo prompts, tool results or credentials.
    # Only local validation messages are safe to return to the gateway.
    return str(error) if isinstance(error, ValueError) else 'Runpod inference failed'

class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version='HTTP/1.1'
    def log_message(self,fmt,*args):print(fmt%args,flush=True)
    def reply(self,status,data):
        raw=json.dumps(data).encode();self.send_response(status);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(raw)));self.send_header('Connection','close');self.close_connection=True
        if getattr(self, 'request_id', None):self.send_header('X-Adapter-Request-Id', self.request_id)
        self.end_headers();self.wfile.write(raw)
    def authorized(self):
        if secrets.compare_digest(self.headers.get('Authorization',''),'Bearer '+CLIENT_KEY):return True
        self.reply(401,{'error':{'message':'Invalid API key','type':'authentication_error'}});return False
    def do_GET(self):
        if self.path=='/health':return self.reply(200,{'status':'ok'})
        if not self.authorized():return
        if self.path=='/v1/models':
            routes=json.loads(ROUTES.read_text());return self.reply(200,{'object':'list','data':[{'id':k,'object':'model','created':0,'owned_by':'runpod'} for k in routes]})
        if self.path.startswith('/v1/jobs/'):
            request_id=self.path.removeprefix('/v1/jobs/')
            if not re.fullmatch(r'[A-Za-z0-9._-]{1,128}',request_id):
                return self.reply(400,{'error':{'message':'Invalid request id'}})
            job=JobJournal(JOURNAL).get(request_id)
            return self.reply(200,{'request_id':request_id,**job}) if job else self.reply(404,{'error':{'message':'Unknown request id'}})
        self.reply(404,{'error':{'message':'Unknown path'}})
    def do_POST(self):
        if not self.authorized():return
        if self.path!='/v1/chat/completions':return self.reply(404,{'error':{'message':'Use /v1/chat/completions'}})
        if not MAX_IN_FLIGHT.acquire(blocking=False):
            return self.reply(429,{'error':{'message':'Adapter is busy; retry later','type':'capacity_error'}})
        streaming=False;job=None;base=None;journal=None;submitted=False
        try:
            length=int(self.headers.get('Content-Length','0'))
            if length>4*1024*1024:return self.reply(413,{'error':{'message':'Request too large'}})
            body=json.loads(self.rfile.read(length));model=body.get('model');routes=json.loads(ROUTES.read_text())
            print('chat request', json.dumps({'model':model,'stream':body.get('stream'),
                'messages':[{'role':m.get('role'),'content_type':type(m.get('content')).__name__,
                    'part_types':[p.get('type') for p in m.get('content',[]) if isinstance(p,dict)] if isinstance(m.get('content'),list) else []}
                    for m in body.get('messages',[]) if isinstance(m,dict)],
                'tool_count':len(body.get('tools',[]))}), flush=True)
            if model not in routes:return self.reply(404,{'error':{'message':'Unknown model: '+str(model)}})
            route=routes[model]
            if route.get('enabled') is False:return self.reply(503,{'error':{'message':route.get('disabled_reason','Model is paused'),'type':'model_unavailable'}})
            if route['type'] != 'openai':
                options={k:body[k] for k in ['temperature','top_p','seed'] if k in body}
                options['num_predict']=predict_limit(body, route)
                if 'stop' in body:options['stop']=body['stop']
                payload={'messages':normalize_messages(body.get('messages',[])),'stream':False,'options':options}
                for k in ['tools','think']:
                    if k in body:payload[k]=body[k]
                fmt=body.get('response_format',{})
                if fmt.get('type')=='json_object':payload['format']='json'
                if fmt.get('type')=='json_schema':payload['format']=fmt.get('json_schema',{}).get('schema')
            key=self.headers.get('Idempotency-Key')
            if key is not None and not re.fullmatch(r'[A-Za-z0-9._-]{1,128}', key):
                raise ValueError('invalid idempotency key')
            self.request_id=key or uuid.uuid4().hex
            journal=JobJournal(JOURNAL)
            journal.start(self.request_id, model, 'load_balancer' if route['type']=='openai' else 'queue')
            if route['type']=='openai':
                try:
                    with request(route['base_url']+'/chat/completions',body,timeout=340) as upstream:
                        journal.finish(self.request_id, 'IN_PROGRESS')
                        self.send_response(upstream.status);self.send_header('Content-Type',upstream.headers.get('Content-Type','application/json'));self.send_header('Connection','close');self.send_header('X-Adapter-Request-Id',self.request_id);self.end_headers();self.close_connection=True
                        while True:
                            chunk=upstream.read1(65536)
                            if not chunk:break
                            self.wfile.write(chunk);self.wfile.flush()
                    journal.finish(self.request_id, 'COMPLETED')
                except (BrokenPipeError,ConnectionResetError):
                    journal.finish(self.request_id, 'CANCELLATION_UNSUPPORTED')
                except Exception:
                    journal.finish(self.request_id, 'SUBMISSION_UNKNOWN' if journal.get(self.request_id)['state']=='SUBMITTING' else 'RESULT_UNKNOWN')
                    raise
                return
            base=route['base_url']
            try:
                submission=rpc(base+'/run',{'input':payload,'policy':{'executionTimeout':600000,'ttl':3600000}})
                job=submission['id']
            except Exception:
                journal.finish(self.request_id, 'SUBMISSION_UNKNOWN')
                raise
            journal.submitted(self.request_id, job);submitted=True
            streaming=bool(body.get('stream'))
            if streaming:
                self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Cache-Control','no-cache');self.send_header('Connection','close');self.send_header('X-Adapter-Request-Id',self.request_id);self.end_headers();self.close_connection=True
            deadline=time.monotonic()+1800;heartbeat=0
            while time.monotonic()<deadline:
                result=rpc(base+'/status/'+job)
                if result['status']=='COMPLETED':
                    journal.finish(self.request_id, 'COMPLETED');break
                if result['status'] in ['FAILED','CANCELLED','TIMED_OUT']:
                    journal.finish(self.request_id, result['status'])
                    raise RuntimeError(str(result.get('error',result['status'])))
                if streaming and time.monotonic()-heartbeat>10:
                    self.wfile.write(b': waiting for Runpod\n\n');self.wfile.flush();heartbeat=time.monotonic()
                time.sleep(2)
            else:
                cancel_job(journal,self.request_id,base)
                raise RuntimeError('Runpod cold start/request exceeded 30 minutes')
            response=normalize(result['output'],model,'chatcmpl-'+uuid.uuid4().hex)
            if not response['choices'][0]['message'].get('content') and not response['choices'][0]['message'].get('tool_calls'):
                raise RuntimeError('model returned no visible answer')
            if not streaming:return self.reply(200,response)
            for chunk in stream_chunks(response):
                self.wfile.write(('data: '+json.dumps(chunk)+'\n\n').encode())
            self.wfile.write(b'data: [DONE]\n\n');self.wfile.flush()
        except (BrokenPipeError,ConnectionResetError):
            if job and base and journal and journal.get(self.request_id)['state'] not in ('COMPLETED','FAILED','CANCELLED','TIMED_OUT'):
                cancel_job(journal,self.request_id,base)
        except DuplicateRequestError as e:
            self.reply(409,{'error':{'message':str(e),'type':'duplicate_request'}})
        except Exception as e:
            if submitted and journal and journal.get(self.request_id)['state']=='SUBMITTED':
                journal.finish(self.request_id, 'POLL_UNKNOWN')
            print('chat failure', type(e).__name__, 'job', job or '-', flush=True)
            error={'error':{'message':public_error(e),'type':'upstream_error'}}
            if streaming:
                self.wfile.write(('data: '+json.dumps(error)+'\n\ndata: [DONE]\n\n').encode());self.wfile.flush()
            else:self.reply(502,error)
        finally:
            MAX_IN_FLIGHT.release()

if __name__=='__main__':http.server.ThreadingHTTPServer(('0.0.0.0',8000),Handler).serve_forever()
