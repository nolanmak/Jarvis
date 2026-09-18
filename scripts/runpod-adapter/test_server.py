import importlib.util
import pathlib
import sys
import unittest
import tempfile
import http.client
import json
import threading
from unittest import mock
import sqlite3
import urllib.error

ROOT = pathlib.Path(__file__).parent
spec = importlib.util.spec_from_file_location('runpod_adapter', ROOT / 'server.py')
module = importlib.util.module_from_spec(spec)
import os
os.environ.setdefault('RUNPOD_API_KEY', 'test-runpod-key')
os.environ.setdefault('ADAPTER_API_KEY', 'test-client-key')
spec.loader.exec_module(module)


class NormalizeMessagesTests(unittest.TestCase):
    def test_responses_conversion_text_parts_become_ollama_strings(self):
        messages = [
            {'role': 'system', 'content': [{'type': 'text', 'text': 'Use approved tools.'}]},
            {'role': 'user', 'content': [{'type': 'input_text', 'text': 'Say READY.'}]},
        ]
        normalized = module.normalize_messages(messages)
        self.assertEqual([m['content'] for m in normalized], ['Use approved tools.', 'Say READY.'])

    def test_consecutive_system_messages_are_one_initial_ollama_system_message(self):
        messages = [
            {'role': 'system', 'content': 'Jarvis policy'},
            {'role': 'system', 'content': [{'type': 'text', 'text': 'Tool rules'}]},
            {'role': 'user', 'content': 'Say READY'},
        ]
        normalized = module.normalize_messages(messages)
        self.assertEqual([m['role'] for m in normalized], ['system', 'user'])
        self.assertEqual(normalized[0]['content'], 'Jarvis policy\n\nTool rules')

    def test_tool_round_trip_keeps_name_and_decodes_arguments(self):
        messages = [
            {'role': 'assistant', 'content': None, 'tool_calls': [
                {'id': 'call_1', 'type': 'function', 'function': {'name': 'Read', 'arguments': '{"path":"a.md"}'}}
            ]},
            {'role': 'tool', 'tool_call_id': 'call_1', 'content': [{'type': 'text', 'text': 'contents'}]},
        ]
        normalized = module.normalize_messages(messages)
        self.assertEqual(normalized[0]['tool_calls'][0]['function']['arguments'], {'path': 'a.md'})
        self.assertEqual(normalized[1]['tool_name'], 'Read')
        self.assertEqual(normalized[1]['content'], 'contents')

    def test_inline_image_part_becomes_ollama_image(self):
        messages = [{'role': 'user', 'content': [
            {'type': 'text', 'text': 'Describe this'},
            {'type': 'image_url', 'image_url': {'url': 'data:image/png;base64,aGVsbG8='}},
        ]}]
        normalized = module.normalize_messages(messages)
        self.assertEqual(normalized[0]['content'], 'Describe this')
        self.assertEqual(normalized[0]['images'], ['aGVsbG8='])

    def test_output_limit_caps_large_codex_metadata_request(self):
        self.assertEqual(module.predict_limit({'max_completion_tokens': 64000}, {'max_output_tokens': 2048}), 2048)
        self.assertEqual(module.predict_limit({'max_tokens': 512}, {'max_output_tokens': 2048}), 512)
        with self.assertRaises(ValueError):
            module.predict_limit({'max_tokens': -1}, {'max_output_tokens': 2048})

    def test_stream_emits_role_then_content_then_finish(self):
        response = {'id': 'id-1', 'object': 'chat.completion', 'model': 'qwen38-27b',
                    'choices': [{'index': 0, 'message': {'role': 'assistant', 'content': 'READY'}, 'finish_reason': 'stop'}],
                    'usage': {'prompt_tokens': 1, 'completion_tokens': 1, 'total_tokens': 2}}
        chunks = list(module.stream_chunks(response))
        self.assertEqual([chunk['choices'][0]['delta'] for chunk in chunks],
                         [{'role': 'assistant'}, {'content': 'READY'}, {}])
        self.assertEqual(chunks[-1]['choices'][0]['finish_reason'], 'stop')

    def test_malformed_or_unknown_content_fails_before_runpod_submission(self):
        for content in ([{'type': 'image_url', 'image_url': {'url': 'https://example.test/a.png'}}],
                        [{'type': 'text', 'text': 8}], 42):
            with self.subTest(content=content), self.assertRaises(ValueError):
                module.normalize_messages([{'role': 'user', 'content': content}])

    def test_upstream_error_does_not_echo_secret_bearing_worker_text(self):
        message = module.public_error(RuntimeError('worker failed: token=private-secret'))
        self.assertEqual(message, 'Runpod inference failed')
        self.assertNotIn('private-secret', message)


class UpstreamCredentialTests(unittest.TestCase):
    def test_runpod_queue_and_load_balancer_hosts_are_allowed(self):
        for url in ('https://api.runpod.ai/v2/endpoint/run',
                    'https://g1eary963m1aym.api.runpod.ai/openai/v1/chat/completions'):
            with self.subTest(url=url):
                self.assertIsNone(module.validate_upstream_url(url))

    def test_redirect_cannot_send_runpod_key_to_another_host(self):
        redirected = []

        class Destination(module.http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                redirected.append(self.headers.get('Authorization'))
                self.send_response(200)
                self.end_headers()

            def log_message(self, *args):
                pass

        destination = module.http.server.ThreadingHTTPServer(('127.0.0.1', 0), Destination)

        class Source(module.http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(302)
                self.send_header('Location', f'http://localhost:{destination.server_port}/stolen')
                self.end_headers()

            def log_message(self, *args):
                pass

        source = module.http.server.ThreadingHTTPServer(('127.0.0.1', 0), Source)
        threads = [threading.Thread(target=server.serve_forever, daemon=True)
                   for server in (source, destination)]
        for thread in threads:
            thread.start()
        try:
            with mock.patch.object(module, 'validate_upstream_url', return_value=None):
                with self.assertRaises(urllib.error.HTTPError) as failure:
                    module.request(f'http://127.0.0.1:{source.server_port}/start')
            self.assertEqual(failure.exception.code, 302)
            failure.exception.close()
            self.assertEqual(redirected, [])
        finally:
            for server in (source, destination):
                server.shutdown()
                server.server_close()
            for thread in threads:
                thread.join(timeout=2)

    def test_non_runpod_or_credential_bearing_upstream_never_receives_key(self):
        for url in ('http://127.0.0.1:8000/run',
                    'https://user:pass@api.runpod.ai/v2/endpoint/run',  # pii-ok synthetic URL credentials
                    'https://other.example/v2/endpoint/run',
                    'https://api.runpod.ai:444/v2/endpoint/run',
                    'https://api.runpod.ai/v2/endpoint/run?token=bad'):
            with self.subTest(url=url), mock.patch.object(
                    module.urllib.request, 'build_opener',
                    side_effect=AssertionError('request reached network')):
                with self.assertRaises(ValueError):
                    module.request(url)


class JobLifecycleTests(unittest.TestCase):
    def test_existing_journal_rows_survive_endpoint_column_migration(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp) / 'jobs.sqlite3'
            with sqlite3.connect(path) as db:
                db.execute('''CREATE TABLE jobs (request_id TEXT PRIMARY KEY, model TEXT NOT NULL,
                    route TEXT NOT NULL, job_id TEXT, state TEXT NOT NULL, updated_at INTEGER NOT NULL)''')
                db.execute("INSERT INTO jobs VALUES ('old-request','qwen38-27b','queue','old-job','POLL_UNKNOWN',1)")
            path.chmod(0o600)
            journal = module.JobJournal(path)
            self.assertEqual(journal.get('old-request')['job_id'], 'old-job')
            self.assertIsNone(journal.get('old-request')['endpoint_url'])

    def test_reconcile_uses_original_endpoint_and_requires_matching_job_id(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal = module.JobJournal(pathlib.Path(tmp) / 'jobs.sqlite3')
            journal.start('request-1', 'qwen38-27b', 'queue', 'https://old-endpoint.test')
            journal.submitted('request-1', 'job-1')
            journal.finish('request-1', 'CANCELLATION_UNKNOWN')
            calls = []
            def completed(url, payload=None):
                calls.append(url)
                return {'id': 'job-1', 'status': 'COMPLETED'}
            state = module.reconcile_job(journal, 'request-1', completed)
            self.assertEqual(state, 'COMPLETED')
            self.assertEqual(calls, ['https://old-endpoint.test/status/job-1'])
            self.assertEqual(module.JobJournal(journal.path).get('request-1')['state'], 'COMPLETED')
            journal.start('request-2', 'qwen38-27b', 'queue', 'https://old-endpoint.test')
            journal.submitted('request-2', 'job-2')
            with self.assertRaises(ValueError):
                module.reconcile_job(journal, 'request-2',
                    lambda url, payload=None: {'id': 'different-job', 'status': 'COMPLETED'})
            self.assertEqual(journal.get('request-2')['state'], 'SUBMITTED')

    def test_authenticated_http_reconcile_reports_confirmed_terminal_state(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal_path = pathlib.Path(tmp) / 'jobs.sqlite3'
            journal = module.JobJournal(journal_path)
            journal.start('request-1', 'qwen38-27b', 'queue', 'https://old-endpoint.test')
            journal.submitted('request-1', 'job-1')
            server = module.http.server.ThreadingHTTPServer(('127.0.0.1', 0), module.Handler)
            worker = threading.Thread(target=server.serve_forever, daemon=True)
            worker.start()
            try:
                def completed(url, payload=None):
                    self.assertEqual(url, 'https://old-endpoint.test/status/job-1')
                    return {'id': 'job-1', 'status': 'COMPLETED'}
                with mock.patch.object(module, 'JOURNAL', journal_path), \
                     mock.patch.object(module, 'rpc', completed):
                    conn = http.client.HTTPConnection('127.0.0.1', server.server_port, timeout=2)
                    conn.request('POST', '/v1/jobs/request-1/reconcile', '',
                                 {'Authorization': 'Bearer test-client-key'})
                    response = conn.getresponse()
                    self.assertEqual(response.status, 200)
                    self.assertEqual(json.load(response)['state'], 'COMPLETED')
                    conn.close()
                self.assertEqual(module.JobJournal(journal_path).get('request-1')['state'], 'COMPLETED')
            finally:
                server.shutdown()
                server.server_close()
                worker.join(timeout=2)

    def test_authenticated_cancel_confirms_queue_job_and_marks_load_balancer_unsupported(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal_path = pathlib.Path(tmp) / 'jobs.sqlite3'
            journal = module.JobJournal(journal_path)
            journal.start('queue-request', 'qwen38-27b', 'queue', 'https://api.runpod.ai/v2/old-endpoint')
            journal.submitted('queue-request', 'job-1')
            journal.start('lb-request', 'glm-5.3-flash', 'load_balancer',
                          'https://g1eary963m1aym.api.runpod.ai/openai/v1')
            journal.start('unknown-request', 'qwen38-27b', 'queue',
                          'https://api.runpod.ai/v2/old-endpoint')
            upstream = []
            def confirmed(url, payload=None):
                upstream.append((url, payload))
                return {'id': 'job-1', 'status': 'CANCELLED'}
            server = module.http.server.ThreadingHTTPServer(('127.0.0.1', 0), module.Handler)
            worker = threading.Thread(target=server.serve_forever, daemon=True)
            worker.start()
            try:
                with mock.patch.object(module, 'JOURNAL', journal_path), \
                     mock.patch.object(module, 'rpc', confirmed):
                    for request_id, expected_status, expected_state in (
                            ('queue-request', 200, 'CANCELLED'),
                            ('lb-request', 202, 'CANCELLATION_UNSUPPORTED'),
                            ('unknown-request', 202, 'CANCELLATION_UNKNOWN')):
                        conn = http.client.HTTPConnection('127.0.0.1', server.server_port, timeout=2)
                        conn.request('POST', f'/v1/jobs/{request_id}/cancel', '',
                                     {'Authorization': 'Bearer test-client-key'})
                        response = conn.getresponse()
                        self.assertEqual(response.status, expected_status)
                        self.assertEqual(json.load(response)['state'], expected_state)
                        conn.close()
                self.assertEqual(upstream,
                                 [('https://api.runpod.ai/v2/old-endpoint/cancel/job-1', {})])
                self.assertEqual(module.JobJournal(journal_path).get('queue-request')['state'], 'CANCELLED')
                self.assertEqual(module.JobJournal(journal_path).get('lb-request')['state'],
                                 'CANCELLATION_UNSUPPORTED')
                self.assertEqual(module.JobJournal(journal_path).get('unknown-request')['state'],
                                 'CANCELLATION_UNKNOWN')
            finally:
                server.shutdown()
                server.server_close()
                worker.join(timeout=2)

    def test_cancel_during_submission_captures_job_id_and_cancels_it_once(self):
        with tempfile.TemporaryDirectory() as tmp:
            routes = pathlib.Path(tmp) / 'routes.json'
            routes.write_text(json.dumps({'qwen38-27b': {'type': 'ollama-queue',
                'base_url': 'https://api.runpod.ai/v2/endpoint', 'max_output_tokens': 128}}))
            journal_path = pathlib.Path(tmp) / 'jobs.sqlite3'
            entered = threading.Event()
            release = threading.Event()
            calls = []
            def upstream(url, payload=None):
                calls.append(url)
                if url.endswith('/run'):
                    entered.set()
                    release.wait(timeout=3)
                    return {'id': 'job-1'}
                if url.endswith('/cancel/job-1'):
                    return {'id': 'job-1', 'status': 'CANCELLED'}
                raise AssertionError('cancelled job must not be polled')
            server = module.http.server.ThreadingHTTPServer(('127.0.0.1', 0), module.Handler)
            worker = threading.Thread(target=server.serve_forever, daemon=True)
            worker.start()
            result = []
            def submit():
                conn = http.client.HTTPConnection('127.0.0.1', server.server_port, timeout=5)
                conn.request('POST', '/v1/chat/completions', json.dumps({
                    'model': 'qwen38-27b', 'messages': [{'role': 'user', 'content': 'hello'}]}),
                    {'Authorization': 'Bearer test-client-key', 'Content-Type': 'application/json',
                     'Idempotency-Key': 'turn-1'})
                response = conn.getresponse()
                result.append(response.status)
                response.read()
                conn.close()
            sender = threading.Thread(target=submit)
            try:
                with mock.patch.object(module, 'ROUTES', routes), \
                     mock.patch.object(module, 'JOURNAL', journal_path), \
                     mock.patch.object(module, 'rpc', upstream):
                    sender.start()
                    self.assertTrue(entered.wait(timeout=2))
                    conn = http.client.HTTPConnection('127.0.0.1', server.server_port, timeout=2)
                    conn.request('POST', '/v1/jobs/turn-1/cancel', '',
                                 {'Authorization': 'Bearer test-client-key'})
                    response = conn.getresponse()
                    self.assertEqual(response.status, 202)
                    response.read()
                    conn.close()
                    release.set()
                    sender.join(timeout=5)
                self.assertEqual(result, [409])
                self.assertEqual(calls, ['https://api.runpod.ai/v2/endpoint/run',
                                         'https://api.runpod.ai/v2/endpoint/cancel/job-1'])
                job = module.JobJournal(journal_path).get('turn-1')
                self.assertEqual(job['job_id'], 'job-1')
                self.assertEqual(job['state'], 'CANCELLED')
            finally:
                release.set()
                sender.join(timeout=5)
                server.shutdown()
                server.server_close()
                worker.join(timeout=2)

    def test_journal_refuses_a_shared_directory(self):
        with tempfile.TemporaryDirectory() as tmp:
            state = pathlib.Path(tmp) / 'state'
            state.mkdir()
            state.chmod(0o755)
            with self.assertRaises(ValueError):
                module.JobJournal(state / 'jobs.sqlite3')
            self.assertFalse((state / 'jobs.sqlite3').exists())

    def test_submission_is_recorded_before_network_call_and_survives_restart(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp) / 'jobs.sqlite3'
            journal = module.JobJournal(path)
            journal.start('request-1', 'qwen38-27b', 'queue')
            self.assertEqual(module.JobJournal(path).get('request-1')['state'], 'SUBMITTING')
            journal.submitted('request-1', 'job-1')
            self.assertEqual(module.JobJournal(path).get('request-1')['job_id'], 'job-1')
            journal.finish('request-1', 'COMPLETED')
            self.assertEqual(module.JobJournal(path).get('request-1')['state'], 'COMPLETED')

    def test_uncertain_submission_stays_unresolved_without_a_second_paid_job(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal = module.JobJournal(pathlib.Path(tmp) / 'jobs.sqlite3')
            journal.start('request-1', 'qwen38-27b', 'queue')
            journal.finish('request-1', 'SUBMISSION_UNKNOWN')
            with self.assertRaises(ValueError):
                journal.start('request-1', 'qwen38-27b', 'queue')
            self.assertEqual(journal.get('request-1')['state'], 'SUBMISSION_UNKNOWN')

    def test_cancel_acknowledgement_must_name_job_and_confirm_cancelled(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal = module.JobJournal(pathlib.Path(tmp) / 'jobs.sqlite3')
            journal.start('request-1', 'qwen38-27b', 'queue')
            journal.submitted('request-1', 'job-1')
            calls = []
            def confirmed(url, payload):
                calls.append((url, payload))
                return {'id': 'job-1', 'status': 'CANCELLED'}
            state = module.cancel_job(journal, 'request-1', 'https://example.test', confirmed)
            self.assertEqual(state, 'CANCELLED')
            self.assertEqual(calls, [('https://example.test/cancel/job-1', {})])
            self.assertEqual(module.JobJournal(journal.path).get('request-1')['state'], 'CANCELLED')

    def test_cancel_failure_or_late_completion_is_not_reported_as_cancelled(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal = module.JobJournal(pathlib.Path(tmp) / 'jobs.sqlite3')
            journal.start('request-1', 'qwen38-27b', 'queue')
            journal.submitted('request-1', 'job-1')
            state = module.cancel_job(journal, 'request-1', 'https://example.test',
                                      lambda url, payload: {'id': 'job-1', 'status': 'COMPLETED'})
            self.assertEqual(state, 'COMPLETED')
            self.assertEqual(journal.get('request-1')['state'], 'COMPLETED')
            journal.start('request-2', 'qwen38-27b', 'queue')
            journal.submitted('request-2', 'job-2')
            def unavailable(url, payload):
                raise TimeoutError('cancel response lost')
            state = module.cancel_job(journal, 'request-2', 'https://example.test', unavailable)
            self.assertEqual(state, 'CANCELLATION_UNKNOWN')
            self.assertEqual(journal.get('request-2')['state'], 'CANCELLATION_UNKNOWN')

    def test_http_retry_after_lost_submission_response_does_not_submit_again(self):
        with tempfile.TemporaryDirectory() as tmp:
            routes = pathlib.Path(tmp) / 'routes.json'
            routes.write_text(json.dumps({'qwen38-27b': {'type': 'ollama',
                'base_url': 'https://example.test', 'max_output_tokens': 128}}))
            journal_path = pathlib.Path(tmp) / 'jobs.sqlite3'
            calls = []
            def lost_response(url, payload=None):
                calls.append(url)
                raise TimeoutError('Runpod accepted the job but response was lost')
            server = module.http.server.ThreadingHTTPServer(('127.0.0.1', 0), module.Handler)
            worker = threading.Thread(target=server.serve_forever, daemon=True)
            worker.start()
            try:
                with mock.patch.object(module, 'ROUTES', routes), \
                     mock.patch.object(module, 'JOURNAL', journal_path), \
                     mock.patch.object(module, 'rpc', lost_response):
                    for expected in (502, 409):
                        conn = http.client.HTTPConnection('127.0.0.1', server.server_port, timeout=2)
                        conn.request('POST', '/v1/chat/completions',
                                     json.dumps({'model': 'qwen38-27b', 'messages': [
                                         {'role': 'user', 'content': 'hello'}]}),
                                     {'Authorization': 'Bearer test-client-key',
                                      'Content-Type': 'application/json',
                                      'Idempotency-Key': 'one-logical-turn'})
                        response = conn.getresponse()
                        self.assertEqual(response.status, expected)
                        self.assertEqual(response.getheader('X-Adapter-Request-Id'), 'one-logical-turn')
                        response.read()
                        conn.close()
                self.assertEqual(calls, ['https://example.test/run'])
                self.assertEqual(module.JobJournal(journal_path).get('one-logical-turn')['state'],
                                 'SUBMISSION_UNKNOWN')
                with mock.patch.object(module, 'JOURNAL', journal_path):
                    conn = http.client.HTTPConnection('127.0.0.1', server.server_port, timeout=2)
                    conn.request('GET', '/v1/jobs/one-logical-turn', headers={
                        'Authorization': 'Bearer test-client-key'})
                    response = conn.getresponse()
                    self.assertEqual(response.status, 200)
                    self.assertEqual(json.load(response)['state'], 'SUBMISSION_UNKNOWN')
                    conn.close()
            finally:
                server.shutdown()
                server.server_close()
                worker.join(timeout=2)

    def test_load_balancer_retry_uses_the_same_journal_gate(self):
        with tempfile.TemporaryDirectory() as tmp:
            routes = pathlib.Path(tmp) / 'routes.json'
            routes.write_text(json.dumps({'glm-5.3-flash': {'type': 'openai',
                'base_url': 'https://example.test/openai/v1'}}))
            journal_path = pathlib.Path(tmp) / 'jobs.sqlite3'
            calls = []
            def lost_response(url, data=None, timeout=45):
                calls.append(url)
                raise TimeoutError('load balancer may have accepted request')
            server = module.http.server.ThreadingHTTPServer(('127.0.0.1', 0), module.Handler)
            worker = threading.Thread(target=server.serve_forever, daemon=True)
            worker.start()
            try:
                with mock.patch.object(module, 'ROUTES', routes), \
                     mock.patch.object(module, 'JOURNAL', journal_path), \
                     mock.patch.object(module, 'request', lost_response):
                    for expected in (502, 409):
                        conn = http.client.HTTPConnection('127.0.0.1', server.server_port, timeout=2)
                        conn.request('POST', '/v1/chat/completions',
                                     json.dumps({'model': 'glm-5.3-flash', 'messages': [
                                         {'role': 'user', 'content': 'hello'}]}),
                                     {'Authorization': 'Bearer test-client-key',
                                      'Content-Type': 'application/json',
                                      'Idempotency-Key': 'glm-logical-turn'})
                        response = conn.getresponse()
                        self.assertEqual(response.status, expected)
                        response.read()
                        conn.close()
                self.assertEqual(calls, ['https://example.test/openai/v1/chat/completions'])
                self.assertEqual(module.JobJournal(journal_path).get('glm-logical-turn')['state'],
                                 'SUBMISSION_UNKNOWN')
            finally:
                server.shutdown()
                server.server_close()
                worker.join(timeout=2)

    def test_busy_adapter_rejects_an_extra_paid_request_before_submission(self):
        with tempfile.TemporaryDirectory() as tmp:
            routes = pathlib.Path(tmp) / 'routes.json'
            routes.write_text(json.dumps({'glm-5.3-flash': {'type': 'openai',
                'base_url': 'https://example.test/openai/v1'}}))
            journal_path = pathlib.Path(tmp) / 'jobs.sqlite3'
            entered = threading.Event()
            release = threading.Event()
            calls = []
            def held_request(url, data=None, timeout=45):
                calls.append(url)
                entered.set()
                release.wait(timeout=2)
                raise TimeoutError('upstream uncertain')
            server = module.http.server.ThreadingHTTPServer(('127.0.0.1', 0), module.Handler)
            worker = threading.Thread(target=server.serve_forever, daemon=True)
            worker.start()
            first_status = []
            def post(key, result):
                conn = http.client.HTTPConnection('127.0.0.1', server.server_port, timeout=3)
                conn.request('POST', '/v1/chat/completions',
                             json.dumps({'model': 'glm-5.3-flash', 'messages': [
                                 {'role': 'user', 'content': 'hello'}]}),
                             {'Authorization': 'Bearer test-client-key',
                              'Content-Type': 'application/json', 'Idempotency-Key': key})
                response = conn.getresponse()
                result.append(response.status)
                response.read()
                conn.close()
            try:
                with mock.patch.object(module, 'ROUTES', routes), \
                     mock.patch.object(module, 'JOURNAL', journal_path), \
                     mock.patch.object(module, 'MAX_IN_FLIGHT', threading.BoundedSemaphore(1), create=True), \
                     mock.patch.object(module, 'request', held_request):
                    first = threading.Thread(target=post, args=('first', first_status))
                    first.start()
                    self.assertTrue(entered.wait(timeout=2))
                    second_status = []
                    post('second', second_status)
                    self.assertEqual(second_status, [429])
                    self.assertEqual(len(calls), 1)
                    self.assertIsNone(module.JobJournal(journal_path).get('second'))
                    release.set()
                    first.join(timeout=3)
                    self.assertEqual(first_status, [502])
            finally:
                release.set()
                server.shutdown()
                server.server_close()
                worker.join(timeout=2)


if __name__ == '__main__':
    unittest.main()
