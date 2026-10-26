"""Tests for tune-machine.sh (ec-tune): RAM is the container's cgroup limit, not the host's MemTotal.

Run: python3 scripts/fork/test_tune_machine.py
"""
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name('tune-machine.sh')
GIB = 1 << 30
HOST_KB = 528_280_000  # 503.8 GiB, the 3090's /proc/meminfo
LIMIT_3090 = 367_005_794_304  # its cgroup v2 memory.max (341.8 GiB)


def run(files, *args, env_extra=None, tune=None):
    """Run the tuner with a fake / (EC_TUNE_SYSROOT) holding `files`; returns (stdout, prefs written). `tune`: a dict
    that receives machine-tune.json."""
    with tempfile.TemporaryDirectory() as folder:
        root = Path(folder) / 'sys'
        for path, text in files.items():
            f = root / path.lstrip('/')
            f.parent.mkdir(parents=True, exist_ok=True)
            f.write_text(text)
        conf = Path(folder) / 'home' / 'user' / '.config' / 'effectcraft'
        conf.mkdir(parents=True)
        (conf / 'prefs.json').write_text(json.dumps({'memory': {}, 'disk': {}, 'recentProjects': []}))
        env = dict(os.environ, EC_TUNE_SYSROOT=str(root))
        env.pop('EFFECTCRAFT_RAM_GB', None)
        env.update(env_extra or {})
        out = subprocess.run(['bash', str(SCRIPT), '--conf', str(conf), *args], capture_output=True, text=True, env=env, check=True).stdout
        if tune is not None:
            tune.update(json.loads((conf / 'machine-tune.json').read_text()))
        return out, json.loads((conf / 'prefs.json').read_text())


def meminfo(kb=HOST_KB):
    return {'/proc/meminfo': f'MemTotal:       {kb} kB\nMemFree:         1000 kB\n'}


class Ram(unittest.TestCase):
    def test_the_rentals_allotment_caps_every_cache_and_serve_does_not_double_count(self):
        tune = {}
        out, prefs = run({**meminfo(), '/sys/fs/cgroup/memory.max': f'{LIMIT_3090}\n'}, env_extra={'EFFECTCRAFT_RAM_GB': '256.8'}, tune=tune)
        self.assertIn('allotted', out)
        m = prefs['memory']
        app = (m['layerCacheMb'] + m['mediaCacheMb'] + m['previewCacheMb']) << 20
        serve = tune['serve']['framesMb'] << 20
        self.assertLessEqual(app + serve + GIB, 0.85 * 256.8e9 + GIB, 'app caches + server frame cache fit in 85 % of the allotment')
        self.assertLessEqual(HOST_KB * 1024 - m['ramReservedGb'] * GIB, 0.85 * 256.8e9 + GIB)

    def test_cgroup_v2_limit_is_the_ram(self):
        out, prefs = run({**meminfo(), '/sys/fs/cgroup/memory.max': f'{LIMIT_3090}\n'})
        self.assertIn('342 GB RAM (container limit; host has 504 GB)', out)
        m = prefs['memory']
        caches = (m['layerCacheMb'] + m['mediaCacheMb'] + m['previewCacheMb']) << 20
        self.assertLessEqual(caches, 0.85 * LIMIT_3090 + GIB, 'caches fit in 85 % of the container, not of the host')
        # The app caps caches at host MemTotal - reserve: that cap must stay inside the container too.
        self.assertLessEqual(HOST_KB * 1024 - m['ramReservedGb'] * GIB, 0.85 * LIMIT_3090 + GIB)

    def test_cgroup_v2_max_means_no_limit(self):
        out, _ = run({**meminfo(), '/sys/fs/cgroup/memory.max': 'max\n'})
        self.assertIn('504 GB RAM,', out)
        self.assertNotIn('container limit', out)

    def test_cgroup_v1_unlimited_sentinel_is_ignored(self):
        out, _ = run({**meminfo(), '/sys/fs/cgroup/memory/memory.limit_in_bytes': '9223372036854771712\n'})
        self.assertIn('504 GB RAM,', out)

    def test_cgroup_v1_limit(self):
        out, prefs = run({**meminfo(), '/sys/fs/cgroup/memory/memory.limit_in_bytes': f'{64 * GIB}\n'})
        self.assertIn('64 GB RAM (container limit; host has 504 GB)', out)
        self.assertLessEqual(HOST_KB * 1024 - prefs['memory']['ramReservedGb'] * GIB, 0.85 * 64 * GIB + GIB)

    def test_limit_above_host_is_ignored(self):
        out, _ = run({**meminfo(16 * 1024 * 1024), '/sys/fs/cgroup/memory.max': f'{64 * GIB}\n'})
        self.assertIn('16 GB RAM,', out)

    def test_dry_run_writes_nothing(self):
        out, prefs = run({**meminfo(), '/sys/fs/cgroup/memory.max': f'{LIMIT_3090}\n'}, '--dry-run')
        self.assertIn('nothing written', out)
        self.assertEqual(prefs['memory'], {})


if __name__ == '__main__':
    unittest.main()
