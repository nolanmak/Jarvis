import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path.cwd()))
root = Path.cwd()
worker = (root / 'crates/augmentagent-cli/src/autoship_worker.rs').read_text()
pressure = (root / 'crates/augmentagent-cli/src/disk_pressure.rs').read_text()

content_identity = ('Sha256' in worker or 'sha2::' in worker) and ('std::fs::read' in worker or 'std::fs::read_to_end' in worker)
ancestor_resolution = ('parent()' in pressure and 'exists()' in pressure and 'device_id' in pressure)
unrelated_preserved = 'devices.contains(&device)' in pressure
live_binaries_preserved = 'Never remove the profile itself or its deployed executables' in pressure and 'remove_dir_all(&path)' in pressure
named_regressions_retained = all(name in worker + pressure for name in [
    'policy_selection_and_in_place_updates_request_boundary_reload',
    'admission_pressure_preserves_an_unrelated_filesystem_even_when_both_are_low',
    'admission_pressure_reclaims_each_explicitly_used_filesystem',
])
print(json.dumps([content_identity, ancestor_resolution, unrelated_preserved, live_binaries_preserved, named_regressions_retained]))
