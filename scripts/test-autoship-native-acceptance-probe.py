#!/usr/bin/env python3
"""Pure probe preflight arithmetic: never starts a service, VM, build or model."""
import importlib.util
from pathlib import Path
import unittest

spec=importlib.util.spec_from_file_location('native_probe',Path(__file__).with_name('autoship-native-acceptance-probe.py'))
probe=importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)
GIB=1024**3
MIB=1024**2


class Tests(unittest.TestCase):
    def test_full_guests_fit_at_six_and_half_gib_without_reserving_unit_ceiling(self):
        budget=probe.resource_budget(1024,4096,6*GIB+512*MIB)
        self.assertEqual(budget['run_memory_bytes'],6*GIB)
        self.assertEqual(budget['aggregate_ceiling_bytes'],8*GIB)
        self.assertEqual(budget['preflight_required_bytes'],6*GIB+512*MIB)
        self.assertEqual(budget['final_admission'],'RunScope')
        with self.assertRaisesRegex(ValueError,'insufficient host headroom'):
            probe.resource_budget(1024,4096,6*GIB+512*MIB-1)
        with self.assertRaisesRegex(ValueError,'insufficient host headroom'):
            probe.resource_budget(1024,4096,4400*MIB)

    def test_controller_allowance_is_explicit_and_cannot_reduce_minimum(self):
        budget=probe.resource_budget(1024,4096,7*GIB,512)
        self.assertEqual(budget['controller_allowance_bytes'],512*MIB)
        self.assertEqual(budget['preflight_required_bytes'],6*GIB+768*MIB)
        for value in [0,255,2049,True,'256',256.0]:
            with self.assertRaises(ValueError):
                probe.resource_budget(1024,4096,10*GIB,value)

    def test_review_reservation_and_malformed_inputs(self):
        budget=probe.resource_budget(1024,0,3*GIB)
        self.assertEqual(budget['run_memory_bytes'],2*GIB)
        self.assertEqual(budget['aggregate_ceiling_bytes'],4*GIB)
        for native,build,available in [(True,4096,10*GIB),(511,4096,10*GIB),
                (1024,True,10*GIB),(1024,-1,10*GIB),(1024,4097,10*GIB),
                (1024,4096,-1),(1024,4096,'available'),(1024,4096,True)]:
            with self.assertRaises(ValueError):probe.resource_budget(native,build,available)
        with self.assertRaises(ValueError):probe.resource_budget(4096,4096,20*GIB)


if __name__=='__main__':unittest.main()
