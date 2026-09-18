import importlib.util
import pathlib
import sys
import unittest

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


if __name__ == '__main__':
    unittest.main()
