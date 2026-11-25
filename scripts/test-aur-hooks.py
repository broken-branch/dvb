#!/usr/bin/env python3
"""AUR metadata and pacman hook regressions; hooks run in a stubbed temporary root."""
from pathlib import Path
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
PACKAGE = ROOT / 'packaging/aur/dvb-bin'
RENDER = ROOT / 'scripts/aur-render.sh'
FIXTURE_SUMS = ROOT / 'tests/support/aur-fixture.sums'
FIXTURE_VERSION = '1.2.3'
# The public copy leaves this file out, and with it the rendered PKGBUILD and .SRCINFO,
# which this repository keeps for the release they pin.
PRIVATE = (ROOT / 'scripts/public-release.sh').exists()
RENDERED = ('PKGBUILD', '.SRCINFO')
LOCAL_FILES = ('dvb-bin.install', 'dvb.sysusers', 'dvb.tmpfiles', 'dvb.slice')
SOURCE = PACKAGE / 'dvb-bin.install'
OWNER = '/var/lib/dvb/state/owner'
# A hang guard, not a performance assertion.
HOOK_TIMEOUT = 10
PROBE = ['systemctl', 'is-active', '--quiet', 'dvb.service']
RESTART = [PROBE, ['systemctl', 'daemon-reload'],
           ['systemctl', 'try-restart', 'dvb.service']]
DISABLE = [['systemctl', 'disable', '--now', 'dvb.service', 'dvb.socket', 'dvb-run.socket'],
           ['systemctl', '--global', 'disable', 'dvb-desk.service']]
STOP_SLICE = ['systemctl', 'stop', 'dvb.slice']
INSTALL_MESSAGE = """dvb is installed but not running yet. Turn it on for your account with:
  sudo /usr/lib/dvb/dvb-setup
For another owner: sudo /usr/lib/dvb/dvb-setup --owner <user>
Then open the cockpit with: dvb app
"""
REMOVE_MESSAGE = """dvb's data stays in /var/lib/dvb, and the users dvb and dvb-worker stay.
To remove them too, follow "Remove dvb" in dvb's docs/install.md.
"""


def snapshot(root):
    return {str(p.relative_to(root)): (
                p.readlink() if p.is_symlink() else p.read_bytes() if p.is_file() else None,
                p.lstat().st_mode, p.lstat().st_mtime_ns)
            for p in root.rglob('*') if p != root / 'calls.log'}


def render(version, sums, out):
    return subprocess.run(['bash', str(RENDER), version, str(sums), str(out), str(PACKAGE)],
                          env=dict(PATH=os.defpath, LC_ALL='C'), capture_output=True,
                          text=True, timeout=HOOK_TIMEOUT)


def read_pkgbuild(folder, expression):
    return subprocess.run(
        ['bash', '--noprofile', '--norc', '-ec', f'source ./PKGBUILD; {expression}'], cwd=folder,
        env=dict(PATH=os.defpath, LC_ALL='C'), capture_output=True, text=True, check=True,
        timeout=HOOK_TIMEOUT).stdout.splitlines()


class AurMetadata(unittest.TestCase):
    """The checks of a PKGBUILD and its .SRCINFO run on this repository's rendered
    package and on a render of the template from fixture sums; the public copy has
    only the latter."""
    maxDiff = None

    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix='dvb-aur-render-')
        self.addCleanup(temp.cleanup)
        self.rendered = Path(temp.name) / 'dvb-bin'
        result = render(FIXTURE_VERSION, FIXTURE_SUMS, self.rendered)
        self.assertEqual(result.returncode, 0, result)
        self.packages = [self.rendered] + ([PACKAGE] if PRIVATE else [])

    def test_rendered_files_policy(self):
        self.assertTrue((PACKAGE / 'PKGBUILD.in').is_file())
        for name in RENDERED:
            with self.subTest(name=name):
                # Private: kept and checked below; public: never a stale copy.
                self.assertEqual((PACKAGE / name).exists(), PRIVATE)

    def test_render_from_fixture_sums(self):
        self.assertEqual(sorted(p.name for p in self.rendered.iterdir()), sorted(RENDERED + LOCAL_FILES))
        for name in LOCAL_FILES:
            self.assertEqual((self.rendered / name).read_bytes(), (PACKAGE / name).read_bytes(), name)
        listed = dict(reversed(line.split('  ', 1)) for line in FIXTURE_SUMS.read_text().splitlines())
        local = [hashlib.sha256((PACKAGE / name).read_bytes()).hexdigest() for name in LOCAL_FILES[1:]]
        self.assertEqual(read_pkgbuild(self.rendered, 'echo "$pkgver $pkgrel"; printf "%s\\n" "${sha256sums[@]}"'),
                         [f'{FIXTURE_VERSION} 1', listed[f'dvb-{FIXTURE_VERSION}-x86_64-linux.tar.gz'],
                          listed['SHA256SUMS'], listed['SHA256SUMS.minisig'], listed['LICENSE'], *local])
        text = (self.rendered / 'PKGBUILD').read_text()
        self.assertIn(f'# Pinned for v{FIXTURE_VERSION}.\n', text)
        self.assertNotRegex(text, '@[A-Z0-9_]*@')

    @unittest.skipUnless(PRIVATE, 'the public copy has no rendered package to compare')
    def test_committed_package_is_the_template_rendered(self):
        # The sums PKGBUILD pins for its downloads, by the name the release serves.
        sources = read_pkgbuild(PACKAGE, 'printf "%s\\n" "${source[@]}"')
        sums = read_pkgbuild(PACKAGE, 'printf "%s\\n" "${sha256sums[@]}"')
        [version] = read_pkgbuild(PACKAGE, 'echo "$pkgver"')
        with tempfile.TemporaryDirectory(prefix='dvb-aur-render-') as temp:
            listed = Path(temp) / 'sums'
            listed.write_text(''.join(f'{s}  {u.split("::")[-1].rsplit("/", 1)[-1]}\n'
                                      for u, s in zip(sources, sums) if '://' in u))
            result = render(version, listed, Path(temp) / 'out')
            self.assertEqual(result.returncode, 0, result)
            for name in RENDERED + LOCAL_FILES:
                self.assertEqual((Path(temp) / 'out' / name).read_text(), (PACKAGE / name).read_text(), name)

    def test_render_refuses(self):
        with tempfile.TemporaryDirectory(prefix='dvb-aur-render-') as temp:
            root = Path(temp)
            lines = FIXTURE_SUMS.read_text().splitlines()
            cases = {
                'no sum': ('1.2.3', [l for l in lines if not l.endswith('  LICENSE')],
                           'has no single sum for LICENSE'),
                'two sums': ('1.2.3', lines + [lines[3]], 'has no single sum for LICENSE'),
                'other version': ('1.2.4', lines, 'has no single sum for dvb-1.2.4-x86_64-linux.tar.gz'),
                'prerelease': ('1.2.3-rc.1', lines, 'the version 1.2.3-rc.1 is not X.Y.Z'),
            }
            for case, (version, sums, error) in cases.items():
                with self.subTest(case=case):
                    out = root / 'out'
                    out.mkdir()
                    (out / 'kept').write_text('a previous render\n')
                    (root / 'sums').write_text(''.join(f'{l}\n' for l in sums))
                    result = render(version, root / 'sums', out)
                    self.assertEqual(result.returncode, 1, result)
                    self.assertIn(error, result.stderr)
                    # A refused render leaves the output folder as it was.
                    self.assertEqual([p.name for p in out.iterdir()], ['kept'])
                    shutil.rmtree(out)

    def test_srcinfo_matches_pkgbuild(self):
        # This single-package recipe needs no makepkg on the CI host. Preserve
        # makepkg's field/array order and whitespace so extra lines also fail.
        script = r'''
source ./PKGBUILD
printf 'pkgbase = %s\n' "${pkgbase:-${pkgname[0]}}"
for field in pkgdesc pkgver pkgrel url install arch license makedepends depends \
             optdepends provides conflicts options source sha256sums; do
    declare -n values="$field"
    for value in "${values[@]}"; do
        printf '\t%s = %s\n' "$field" "$value"
    done
done
printf '\npkgname = %s\n' "${pkgname[0]}"
'''
        for package in self.packages:
            with self.subTest(package=package):
                result = subprocess.run(
                    ['bash', '--noprofile', '--norc', '-ec', script], cwd=package,
                    env=dict(PATH=os.defpath, LC_ALL='C'), capture_output=True,
                    text=True, timeout=HOOK_TIMEOUT)
                self.assertEqual(result.returncode, 0, result)
                self.assertEqual((package / '.SRCINFO').read_text(), result.stdout)

    @unittest.skipUnless(shutil.which('makepkg'), 'makepkg is not installed')
    def test_srcinfo_matches_makepkg(self):
        # Keep host/user makepkg settings out of the metadata check.
        with tempfile.TemporaryDirectory(prefix='dvb-aur-metadata-') as temp:
            root = Path(temp)
            config = root / 'makepkg.conf'
            config.write_text("CARCH=x86_64\nCHOST=x86_64-pc-linux-gnu\n"
                              "PKGEXT=.pkg.tar.zst\nSRCEXT=.src.tar.gz\n")
            for package in self.packages:
                with self.subTest(package=package):
                    result = subprocess.run(
                        [shutil.which('makepkg'), '--config', str(config), '--printsrcinfo'],
                        cwd=package, env=dict(PATH=os.defpath, LC_ALL='C', XDG_CONFIG_HOME=temp),
                        capture_output=True, text=True, timeout=HOOK_TIMEOUT)
                    self.assertEqual(result.returncode, 0, result)
                    self.assertEqual((package / '.SRCINFO').read_text(), result.stdout)


class AurHooks(unittest.TestCase):
    def run_hook(self, hook, owner='4242\n', *, active=True, fail_systemctl=False,
                 version='1.2.3-4'):
        with tempfile.TemporaryDirectory(prefix='dvb-aur-hooks-') as temp:
            root = Path(temp)
            owner_file = root / OWNER.lstrip('/')
            owner_file.parent.mkdir(parents=True)
            (owner_file.parent / 'keep').write_text('existing engine data\n')
            if owner is not None:
                owner_file.write_text(owner)
                owner_file.chmod(0o600)
            script = root / 'dvb-bin.install'
            text = SOURCE.read_text()
            self.assertIn(OWNER, text)
            script.write_text(text.replace(OWNER, '"${DVB_TEST_ROOT}' + OWNER + '"'))
            stubs = root / 'stubs'
            stubs.mkdir()
            stub = f'#!{sys.executable} -I\n' + '''from pathlib import Path
import json, os, sys
command, args = Path(sys.argv[0]).name, sys.argv[1:]
with open(os.environ['DVB_TEST_LOG'], 'a') as log:
    log.write(json.dumps([command, *args]) + '\\n')
if command == 'systemctl':
    if os.environ['DVB_TEST_SYSTEMCTL_FAILS'] == '1':
        sys.exit(1)
    if args == ['is-active', '--quiet', 'dvb.service']:
        sys.exit(0 if os.environ['DVB_TEST_ACTIVE'] == '1' else 3)
    sys.exit(0)
if command == 'getent':
    if args in (['passwd', '4242'], ['passwd', 'desktop']):
        print('desktop:x:4242:4242::' + os.environ['DVB_TEST_ROOT'] + '/home/desktop:/bin/sh')
        sys.exit(0)
    # Real getent passwd "" on the test host: status 2, no output.
    if args in (['passwd', ''], ['passwd', 'missing-user']):
        sys.exit(2)
sys.exit(99)
'''
            for command in ('systemctl', 'getent'):
                path = stubs / command
                path.write_text(stub)
                path.chmod(0o755)
            # A closed PATH prevents falling through to host service/account tools.
            for command in ('cat', 'cut'):
                path = shutil.which(command, path=os.defpath)
                self.assertIsNotNone(path, f'{command} is required')
                (stubs / command).symlink_to(path)
            bash = shutil.which('bash', path=os.defpath)
            self.assertIsNotNone(bash, 'bash is required')
            log = root / 'calls.log'
            log.touch()
            env = dict(PATH=str(stubs), LC_ALL='C', DVB_TEST_ROOT=str(root),
                       DVB_TEST_LOG=str(log), DVB_TEST_ACTIVE=str(int(active)),
                       DVB_TEST_SYSTEMCTL_FAILS=str(int(fail_systemctl)))
            before = snapshot(root)
            result = subprocess.run(
                [bash, '--noprofile', '--norc', '-c', '. "$1"; "$2" "$3" "$4"',
                 'aur-hook-test', str(script), hook, version, '1.2.2-1'],
                cwd=root, env=env, capture_output=True, text=True, timeout=HOOK_TIMEOUT)
            self.assertEqual(result.returncode, 0, result)
            self.assertEqual(result.stderr, '')
            self.assertEqual(snapshot(root), before, 'hook changed fixture files')
            return result.stdout, [json.loads(line) for line in log.read_text().splitlines()]

    def test_upgrade_active_uid(self):
        output, calls = self.run_hook('post_upgrade')
        self.assertEqual(calls, RESTART + [['getent', 'passwd', '4242'],
                         ['systemctl', '--user', '-M', 'desktop@', 'try-restart', 'dvb-desk.service']])
        self.assertEqual(output, 'dvb: engine restarted on 1.2.3.\n')

    def test_upgrade_active_username_and_prerelease(self):
        output, calls = self.run_hook('post_upgrade', 'desktop\n', version='1.2.3-rc1-12')
        self.assertEqual(calls, RESTART + [['getent', 'passwd', 'desktop'],
                         ['systemctl', '--user', '-M', 'desktop@', 'try-restart', 'dvb-desk.service']])
        self.assertEqual(output, 'dvb: engine restarted on 1.2.3-rc1.\n')

    def test_upgrade_inactive(self):
        for owner in ('4242\n', None, '', 'missing-user\n'):
            with self.subTest(owner=owner):
                output, calls = self.run_hook('post_upgrade', owner, active=False)
                self.assertEqual(calls, [PROBE])
                self.assertEqual(output, '')

    def test_upgrade_without_known_owner(self):
        for owner in (None, '', 'missing-user\n'):
            with self.subTest(owner=owner):
                output, calls = self.run_hook('post_upgrade', owner)
                self.assertEqual(calls, RESTART + [['getent', 'passwd', (owner or '').strip()]])
                self.assertEqual(output, 'dvb: engine restarted on 1.2.3.\n')

    def test_remove_known_owner(self):
        for owner in ('4242\n', 'desktop\n'):
            with self.subTest(owner=owner):
                output, calls = self.run_hook('pre_remove', owner)
                self.assertEqual(calls, DISABLE + [['getent', 'passwd', owner.strip()],
                                 ['systemctl', '--user', '-M', 'desktop@', 'stop', 'dvb-desk.service'],
                                 STOP_SLICE])
                self.assertEqual(output, '')

    def test_remove_without_known_owner(self):
        for owner in (None, '', 'missing-user\n'):
            with self.subTest(owner=owner):
                output, calls = self.run_hook('pre_remove', owner)
                self.assertEqual(calls, DISABLE + [['getent', 'passwd', (owner or '').strip()],
                                 STOP_SLICE])
                self.assertEqual(output, '')

    def test_remove_systemctl_failures(self):
        for owner in ('4242\n', 'desktop\n', None, '', 'missing-user\n'):
            with self.subTest(owner=owner):
                output, calls = self.run_hook('pre_remove', owner, fail_systemctl=True)
                desk = ([['systemctl', '--user', '-M', 'desktop@', 'stop', 'dvb-desk.service']]
                        if owner in ('4242\n', 'desktop\n') else [])
                self.assertEqual(calls, DISABLE + [['getent', 'passwd', (owner or '').strip()]]
                                 + desk + [STOP_SLICE])
                self.assertEqual(output, '')

    def test_post_install_message_only(self):
        for owner in ('4242\n', None):
            with self.subTest(owner=owner):
                output, calls = self.run_hook('post_install', owner)
                self.assertEqual(calls, [])
                self.assertEqual(output, INSTALL_MESSAGE)

    def test_post_remove_message_only(self):
        for owner in ('4242\n', None):
            with self.subTest(owner=owner):
                output, calls = self.run_hook('post_remove', owner)
                self.assertEqual(calls, [])
                self.assertEqual(output, REMOVE_MESSAGE)


if __name__ == '__main__':
    unittest.main(verbosity=2)
