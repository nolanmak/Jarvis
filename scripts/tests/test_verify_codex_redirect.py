"""Falsifier for the real-client redirect credential gate."""
import http.client
import importlib.util
import json
from pathlib import Path
import unittest
from urllib.parse import urlsplit


spec = importlib.util.spec_from_file_location(
    'verify_codex_redirect', Path(__file__).with_name('verify_codex_redirect.py'))
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


class RedirectCredentialGateTests(unittest.TestCase):
    def test_gate_fails_if_a_client_forwards_the_key_to_the_redirect(self):
        def leaking_client(command, **kwargs):
            base = json.loads(next(value.split('=', 1)[1] for value in command
                                   if value.startswith('model_providers.synthetic_router.base_url=')))
            source = urlsplit(base)
            key = kwargs['env']['SYNTHETIC_ROUTER_KEY']
            origin = http.client.HTTPConnection(source.hostname, source.port, timeout=2)
            origin.request('POST', '/v1/responses', '{}',
                           {'Authorization': 'Bearer ' + key})
            first = origin.getresponse()
            redirect = urlsplit(first.headers['Location'])
            first.read()
            origin.close()
            destination = http.client.HTTPConnection(redirect.hostname, redirect.port, timeout=2)
            destination.request('POST', redirect.path, '{}',
                                {'Authorization': 'Bearer ' + key})
            destination.getresponse().read()
            destination.close()

        with self.assertRaisesRegex(AssertionError, 'forwarded the router key'):
            probe.run_probe(client=leaking_client)


if __name__ == '__main__':
    unittest.main()
