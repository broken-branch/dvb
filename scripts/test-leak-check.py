#!/usr/bin/env python3
"""Leak check and public copy cases; every fixture tree is built here at run time,
so this repository itself holds none of the strings the rules look for."""
from pathlib import Path
import os
import secrets
import shutil
import string
import subprocess
import sys
import tempfile


scripts = Path(__file__).resolve().parent
LEAK_CHECK = scripts / 'leak-check.sh'
PUBLIC_COPY = scripts / 'public-copy.sh'
# Split so that public-copy.sh leaves this file unchanged.
PRIVATE_REPO = 'dev-bench-com' + '/dvb'
# The whole line that marks a file the public copy leaves out.
PRIVATE_LINE = '# Private: scripts/public-copy.sh leaves this file out of the public copy.'
# The private files' names, split so that no copy of this file names them.
PRIVATE_NAMES = ('launch' + '.sh', 'test-' + 'launch.py')
# Set for the copy's own run of this file inside this one.
NESTED = 'TEST_LEAK_CHECK_NESTED'
EXPRESSION = "runs-on: ${{ inputs.hosted && 'ubuntu-latest' || fromJSON('[\"self-hosted\",\"dvb-ci\"]') }}"


def write_tree(root, files):
    for name, text in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    return root


def leak_check(root, *args, gitleaks=None):
    env = dict(os.environ)
    env.pop('GITLEAKS', None)
    if gitleaks:
        env['GITLEAKS'] = str(gitleaks)
    result = subprocess.run([str(LEAK_CHECK), str(root), *args], env=env, text=True,
                            capture_output=True, timeout=120)
    return result.returncode, result.stdout + result.stderr


def tree_text(root):
    return {str(p.relative_to(root)): p.read_text() for p in sorted(root.rglob('*')) if p.is_file()}


def tree_paths(root):
    """Every file and symlink under root, without following symlinks."""
    for parent, dirs, files in os.walk(root):
        for name in files + [d for d in dirs if os.path.islink(os.path.join(parent, d))]:
            yield os.path.relpath(os.path.join(parent, name), root)


def tree_bytes(root):
    """Every file's bytes and every symlink's target, by path."""
    return {name: ('link', os.readlink(root / name)) if (root / name).is_symlink()
            else ('file', (root / name).read_bytes()) for name in sorted(tree_paths(root))}


def export_repository(out):
    """This repository's files without .git: tracked and untracked but not ignored
    ones in a checkout, every file in a tree with no checkout (a public copy)."""
    root = scripts.parent
    listed = subprocess.run(['git', '-C', str(root), 'ls-files', '-z', '--cached', '--others',
                             '--exclude-standard'], capture_output=True, timeout=60)
    if listed.returncode == 0 and (root / '.git').exists():
        names = sorted({n for n in listed.stdout.decode().split('\0') if n})
    else:
        names = sorted(tree_paths(root))
    for name in names:
        source = root / name
        if not (source.is_symlink() or source.exists()):
            continue  # deleted in the checkout
        target = out / name
        target.parent.mkdir(parents=True, exist_ok=True)
        if source.is_symlink():
            target.symlink_to(os.readlink(source))
        elif source.is_file():
            shutil.copy2(source, target)
    return out


def main():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        passing = tmp / 'gitleaks-pass'
        passing.write_text('#!/bin/sh\n[ "$1" = dir ] || exit 3\n')
        passing.chmod(0o755)
        allowed = '\n'.join([
            'Commit as login' + '@users.noreply.github.com or noreply' + '@forge.dev.',
            'owner' + '@example.com, owner' + '@example.invalid, worker' + '@host.example',
            'systemctl start dvb-run-fake' + '@01JABC.service',
            'remote: https://x:secret' + '@github.com/org/repo',
            'codexHome is /work/home/.codex; HOME is /home/someone; version 10.0.0',
            'jobs run on GitHub-hosted runners',
            '',
        ])
        clean = write_tree(tmp / 'clean', {
            'README.md': allowed,
            '.github/workflows/ci.yml': 'on:\n  pull_request:\njobs:\n  a:\n    runs-on: ubuntu-latest\n',
        })

        # A clean tree passes, with the real pinned gitleaks.
        code, out = leak_check(clean)
        assert code == 0 and 'leak-check: passed' in out, out
        assert 'FAIL' not in out, out

        # gitleaks over the files: a token fails without being printed.
        token = 'ghp_' + ''.join(secrets.choice(string.ascii_letters + string.digits) for _ in range(36))
        leaky = write_tree(tmp / 'token', {'config/app.env': f'GITHUB_TOKEN={token}\n'})
        code, out = leak_check(leaky)
        assert code == 1 and 'FAIL gitleaks' in out, out
        assert token not in out, out

        # A denylist hit names the file, its line and the denylist's line, never the term.
        term = 'Priv' + secrets.token_hex(6)
        hit = write_tree(tmp / 'hit', {
            'docs/a.md': 'first line\nsee ' + term.upper() + ' here\n',
            'b.txt': term.lower() + '\n',
            'c.txt': 'nothing\n',
        })
        denylist = tmp / 'denylist'
        denylist.write_text('# private names\n\n  other' + secrets.token_hex(6) + '\n' + term + '  \n')
        code, out = leak_check(hit, '--denylist', str(denylist), gitleaks=passing)
        assert code == 1, out
        assert 'FAIL denylist line 4: docs/a.md:2' in out, out
        assert 'FAIL denylist line 4: b.txt:1' in out, out
        assert 'c.txt' not in out and 'line 3' not in out and 'line 1:' not in out, out
        assert term.lower() not in out.lower(), out
        # The clean tree passes the same denylist.
        code, out = leak_check(clean, '--denylist', str(denylist), gitleaks=passing)
        assert code == 0 and 'leak-check: passed\n' in out, out

        # A denylist that is missing or unreadable is a refusal.
        for missing in (tmp / 'nonexistent', tmp):
            code, out = leak_check(clean, '--denylist', str(missing), gitleaks=passing)
            assert code == 2 and 'not a readable file' in out, out

        # The generic rules: file:line only.
        name = 'alice' + secrets.token_hex(2)
        found = [
            ('home-directory path', 'path /' + 'home/' + name + '/src'),
            ('home-directory path', 'file:///' + 'home/' + name + '/notes.txt'),
            ('home-directory path', '/srv/' + 'home/' + name + '/src'),
            ('home-directory path', 'C:/' + 'Users/' + name + '/src'),
            ('e-mail address', 'mail ' + name + '@' + 'gmail.com'),
            ('private IPv4 address', 'host ' + '.'.join(['192', '168', '1', '20'])),
            ('dvb run work directory', 'in /var/lib/dvb/work/' + '01H' + 'Z' * 23 + '/ws'),
        ]
        for index, (label, text) in enumerate(found):
            root = write_tree(tmp / f'generic-{index}', {'x/notes.txt': allowed + text + '\n'})
            code, out = leak_check(root, gitleaks=passing)
            assert code == 1, (label, out)
            assert f'FAIL {label}: x/notes.txt:{allowed.count(chr(10)) + 1}\n' in out, (label, out)
            assert out.count('FAIL ') == 1 and text.split()[-1] not in out, (label, out)

        # The workflow rules: any self-hosted string under .github, and the two triggers.
        for workflow, label, line in [
            ('on: push\njobs:\n  a:\n    runs-on: [self-hosted, linux]\n', 'self-hosted runner', 4),
            ('on: push\njobs:\n  a:\n    ' + EXPRESSION + '\n', 'self-hosted runner', 4),
            ('# runs on self-hosted\non: push\n', 'self-hosted runner', 1),
            ('on:\n  pull_request_target:\njobs: {}\n', 'pull_request_target or workflow_run trigger', 2),
            ('on:\n  workflow_run:\n    workflows: [ci]\n', 'pull_request_target or workflow_run trigger', 2),
        ]:
            root = write_tree(tmp / f'workflow-{secrets.token_hex(4)}', {'.github/workflows/x.yml': workflow})
            code, out = leak_check(root, gitleaks=passing)
            assert code == 1 and f'FAIL {label}: .github/workflows/x.yml:{line}\n' in out, out

        # public-copy.sh: both runs-on forms, the hosted input, the name and a private file.
        ci = ('on:\n  pull_request:\n  workflow_dispatch:\n    inputs:\n      hosted:\n'
              '        description: Run on GitHub-hosted runners\n        type: boolean\n        default: false\n'
              'permissions:\n  contents: read\njobs:\n  rust:\n    ' + EXPRESSION + '\n    steps:\n      - run: true\n')
        other = ('on:\n  workflow_dispatch:\n    inputs:\n      hosted:\n        type: boolean\n'
                 '      level:\n        type: string\njobs:\n  a:\n    runs-on: [self-hosted, dvb-ci]\n')
        src = write_tree(tmp / 'src', {
            '.github/workflows/ci.yml': ci,
            '.github/workflows/release.yml': 'on: push\njobs:\n  build:\n    runs-on: [self-hosted, dvb-ci]\n'
                                             f'    steps:\n      - run: gh release create v1 --repo {PRIVATE_REPO}\n',
            '.github/workflows/other.yaml': other,
            'Cargo.toml': f'repository = "https://github.com/{PRIVATE_REPO}"\n',
            'scripts/private.sh': '#!/bin/sh\n' + PRIVATE_LINE + '\necho private\n',
            'scripts/keep.sh': '#!/bin/sh\n# quotes it: ' + PRIVATE_LINE + '\n',
        })
        public = tmp / 'public'
        subprocess.run([str(PUBLIC_COPY), str(src), str(public)], check=True, timeout=60)
        files = tree_text(public)
        assert files['.github/workflows/ci.yml'] == (
            'on:\n  pull_request:\n  workflow_dispatch:\npermissions:\n  contents: read\njobs:\n'
            '  rust:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n'), files['.github/workflows/ci.yml']
        assert files['.github/workflows/release.yml'] == (
            'on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n'
            '    steps:\n      - run: gh release create v1 --repo broken-branch/dvb\n')
        assert files['.github/workflows/other.yaml'] == (
            'on:\n  workflow_dispatch:\n    inputs:\n      level:\n        type: string\n'
            'jobs:\n  a:\n    runs-on: ubuntu-latest\n')
        assert files['Cargo.toml'] == 'repository = "https://github.com/broken-branch/dvb"\n'
        assert 'scripts/private.sh' not in files and 'scripts/keep.sh' in files, files
        assert tree_text(src)['.github/workflows/ci.yml'] == ci, 'the source changed'
        code, out = leak_check(public, gitleaks=passing)
        assert code == 0, out
        # On a tree that is already public it changes nothing.
        again = tmp / 'again'
        subprocess.run([str(PUBLIC_COPY), str(public), str(again)], check=True, timeout=60)
        assert tree_text(again) == files
        # It refuses a checkout and a non-empty output directory.
        (src / '.git').mkdir()
        result = subprocess.run([str(PUBLIC_COPY), str(src), str(tmp / 'out')], capture_output=True, text=True, timeout=60)
        assert result.returncode == 2 and 'git archive' in result.stderr, result.stderr
        (src / '.git').rmdir()
        result = subprocess.run([str(PUBLIC_COPY), str(src), str(public)], capture_output=True, text=True, timeout=60)
        assert result.returncode == 2 and 'not empty' in result.stderr, result.stderr

        # This repository's own public copy: no private file, no private name and no
        # reference to a private file. A second run on it changes nothing, so the
        # copied public-copy.sh still works, and the copy's own cases pass.
        real = export_repository(tmp / 'real')
        real_public = tmp / 'real-public'
        subprocess.run([str(PUBLIC_COPY), str(real), str(real_public)], check=True, timeout=120)
        copied = tree_bytes(real_public)
        for name, (kind, content) in copied.items():
            if kind == 'file':
                assert PRIVATE_LINE.encode() not in content.splitlines(), f'{name} is private'
                assert PRIVATE_REPO.encode() not in content, f'{name} names the private repository'
                for private in PRIVATE_NAMES:
                    assert private.encode() not in content, f'{name} names a private file'
            assert not any(name.endswith(private) for private in PRIVATE_NAMES), name
        real_again = tmp / 'real-again'
        subprocess.run([str(real_public / 'scripts/public-copy.sh'), str(real_public), str(real_again)],
                       check=True, timeout=120)
        again_copied = tree_bytes(real_again)
        changed = sorted(n for n in copied.keys() | again_copied.keys() if copied.get(n) != again_copied.get(n))
        assert not changed, f'the second public copy changed {changed}'
        if not os.environ.get(NESTED):
            result = subprocess.run([sys.executable, str(real_public / 'scripts/test-leak-check.py')],
                                    env=dict(os.environ, **{NESTED: '1'}), capture_output=True,
                                    text=True, timeout=600)
            assert result.returncode == 0, result.stdout + result.stderr
    print('leak check and public copy: all cases passed')


if __name__ == '__main__':
    sys.exit(main())
