#!/usr/bin/env python3
"""Cockpit installer regression cases; all writes go to a temporary root."""
from pathlib import Path
import os
import hashlib
import re
import shutil
import stat
import subprocess
import tempfile
import sys


source = Path(__file__).resolve().parents[1] / 'install'
# A hang guard, not a performance assertion.
INSTALL_TIMEOUT = 60


def executable(path, text):
    path.write_text('#!/bin/sh\nset -eu\n' + text)
    path.chmod(0o755)


def prepare(root, tmpfiles):
    stubs = root / 'stubs'
    stubs.mkdir()

    executable(stubs / 'id', '\n'.join([
        'case "$*" in',
        '  -u) echo 0 ;;',
        '  desktop) : ;;',
        '  "-gn desktop") echo "$DVB_TEST_OWNER_GROUP" ;;',
        '  "-nu 4242") echo desktop ;;',
        '  *) exit 1 ;;',
        'esac',
        '',
    ]))
    executable(stubs / 'chown', ':\n')
    executable(stubs / 'systemd-tmpfiles',
               'printf "%s\\n" "$*" >> "$DVB_TEST_TMPFILES_LOG"\n'
               'exec "$DVB_TEST_TMPFILES" --root="$DVB_TEST_ROOT" "$@"\n')
    # If an incompatible flag regression reaches system operations, fail safely.
    executable(stubs / 'ssh', ':\n')
    for command in ('groupadd', 'useradd', 'usermod'):
        executable(stubs / command, 'echo "unexpected system operation" >&2; exit 1\n')
    # Only the cockpit's user units: enabled for every session, started in the owner's.
    executable(stubs / 'systemctl', 'printf "%s\\n" "$*" >> "$DVB_TEST_SYSTEMCTL_LOG"\n'
               'case "$1" in --global|--user) ;; *) echo "unexpected system operation" >&2; exit 1 ;; esac\n')
    # Packages in DVB_TEST_PACMAN_MISSING are in no repository; installing one in
    # DVB_TEST_PACMAN_FAILS fails.
    executable(stubs / 'pacman', 'printf "%s\\n" "$*" >> "$DVB_TEST_PACMAN_LOG"\n'
               'for p in ${DVB_TEST_PACMAN_MISSING:-}; do [ "$*" != "-Si $p" ] || exit 1; done\n'
               'if [ "$1" = -S ]; then for p in ${DVB_TEST_PACMAN_FAILS:-}; do\n'
               '  case " $* " in *" $p "*) exit 1 ;; esac; done; fi\n')
    executable(stubs / 'gtk-update-icon-cache', 'printf "%s\\n" "$*" >> "$DVB_TEST_ICON_CACHE_LOG"\n')
    binary = root / 'dvb'
    executable(binary, 'echo "dvb 0.1.1"\n')
    script = root / 'install.sh'
    for file in ('dvb-desk.service', 'dvb.desktop'):
        shutil.copyfile(source / file, root / file)
    shutil.copytree(source / 'icons', root / 'icons')
    paths = ['/opt/dvb', '/usr/local/bin/dvb', '/etc/systemd/user',
             '/etc/systemd/system', '/etc/polkit-1/rules.d', '/var/lib', '/proc/meminfo', '/usr/local/bin/sccache',
             '/usr/share/applications', '/usr/share/icons', '/etc/tmpfiles.d', '/usr/local/bin/claude',
             '/usr/lib/polkit-1/polkitd', '/usr/libexec/polkitd']
    text = (source / 'install.sh').read_text()
    for path in paths:
        text = text.replace(path, str(root / path.lstrip('/')))
    # Runs find none of the host's programs, so every runtime want is asked of pacman.
    runs_path = 'runs_path=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin'
    assert runs_path in text
    text = text.replace(runs_path, f'runs_path={root}/runs')
    script.write_text(text)
    (root / 'usr/local/bin').mkdir(parents=True)
    (root / 'usr/lib/polkit-1').mkdir(parents=True)
    polkitd = root / 'usr/lib/polkit-1/polkitd'  # the host's polkit must not matter
    executable(polkitd, ':\n')
    etc = root / 'etc'
    etc.mkdir()
    uid, gid = os.getuid(), os.getgid()
    (etc / 'passwd').write_text(f'desktop:x:{uid}:{gid}::/nonexistent:/bin/sh\n')
    (etc / 'group').write_text(f'desktop:x:{gid}:\n')
    env = dict(os.environ, PATH=str(stubs) + ':' + os.environ['PATH'],
               DVB_TEST_TMPFILES_LOG=str(root / 'tmpfiles.log'),
               DVB_TEST_SYSTEMCTL_LOG=str(root / 'systemctl.log'),
               DVB_TEST_PACMAN_LOG=str(root / 'pacman.log'),
               DVB_TEST_ICON_CACHE_LOG=str(root / 'icon-cache.log'),
               DVB_TEST_TMPFILES=tmpfiles, DVB_TEST_ROOT=str(root),
               DVB_TEST_OWNER_GROUP='desktop')
    args = ['sh', str(script), '--binary', str(binary), '--owner', 'desktop']
    return args, env, script, binary, text


def check_local_cockpit(root, args, binary, env):
    cases = 0
    unit = root / 'etc/systemd/user/dvb-forward.service'
    config = root / 'etc/tmpfiles.d/dvb-cockpit.conf'
    result = subprocess.run(args + ['--cockpit'], env=env, capture_output=True, text=True, timeout=INSTALL_TIMEOUT)
    cases += 1
    assert result.returncode == 0, result.stderr
    assert not unit.exists() and not config.exists()
    assert not (root / 'tmpfiles.log').exists()
    hicolor = root / 'usr/share/icons/hicolor'
    for file, destination in (
        ('dvb-desk.service', root / 'etc/systemd/user/dvb-desk.service'),
        ('dvb.desktop', root / 'usr/share/applications/dvb.desktop'),
        ('icons/dvb.svg', hicolor / 'scalable/apps/dvb.svg'),
        *((f'icons/dvb-{size}.png', hicolor / f'{size}x{size}/apps/dvb.png')
          for size in (16, 32, 48, 64, 128, 256, 512)),
    ):
        assert destination.read_bytes() == (source / file).read_bytes()
        assert stat.S_IMODE(destination.stat().st_mode) == 0o644
    assert (root / 'icon-cache.log').read_text() == f'-f -t {hicolor}\n'
    entry = (source / 'dvb.desktop').read_text().splitlines()
    # The window `dvb app` opens: Chromium, the desk's port 7300 (app.rs).
    assert {'Icon=dvb', 'StartupWMClass=chromium-127.0.0.1__launch-dvb-127.0.0.1-7300'} <= set(entry)
    installed = root / 'opt/dvb/versions/0.1.1/dvb'
    assert installed.read_bytes() == binary.read_bytes()
    assert stat.S_IMODE(installed.stat().st_mode) == 0o755
    assert (root / 'opt/dvb/current').readlink() == Path('versions/0.1.1')
    assert (root / 'opt/dvb/role-cockpit').exists()
    owner = root / 'etc/systemd/user/dvb-desk.service.d/10-owner.conf'
    assert owner.read_text() == '[Unit]\nConditionUser=desktop\n'
    assert stat.S_IMODE(owner.stat().st_mode) == 0o644
    assert (root / 'systemctl.log').read_text().splitlines()[-3:] == [
        '--global enable dvb-desk.service',
        '--user -M desktop@ daemon-reload',
        '--user -M desktop@ restart dvb-desk.service',
    ]
    assert 'started for desktop' in result.stdout and 'systemctl' not in result.stdout
    # Through pkexec, the account that ran it is the owner.
    pkexec = [a for a in args if a not in ('--owner', 'desktop')]
    owner.unlink()
    result = subprocess.run(pkexec + ['--cockpit'], env=dict(env, SUDO_USER='', PKEXEC_UID='4242'),
                            capture_output=True, text=True, timeout=INSTALL_TIMEOUT)
    cases += 1
    assert result.returncode == 0, result.stderr
    assert owner.read_text() == '[Unit]\nConditionUser=desktop\n'
    return cases


def check_remote_cockpit(root, args, env):
    cases = 0
    unit = root / 'etc/systemd/user/dvb-forward.service'
    config = root / 'etc/tmpfiles.d/dvb-cockpit.conf'
    etc = root / 'etc'
    uid, gid = os.getuid(), os.getgid()
    for host in ('engine-host', 'worker@engine-host.example', 'worker@2001:db8::1', 'group-engine-host'):
        # The last case has no group named desktop: only its primary group users.
        group = 'users' if host == 'group-engine-host' else 'desktop'
        env['DVB_TEST_OWNER_GROUP'] = group
        (etc / 'group').write_text(f'{group}:x:{gid}:\n')
        shutil.rmtree(root / 'run', ignore_errors=True)
        result = subprocess.run(args + ['--cockpit', '--engine-host', host], env=env, capture_output=True, text=True, timeout=INSTALL_TIMEOUT)
        cases += 1
        assert result.returncode == 0, result.stderr
        lines = unit.read_text().splitlines()
        command = next(line for line in lines
                       if line.startswith('ExecStart='))
        assert command.startswith('ExecStart=/usr/bin/ssh -N ')
        for option in ('BatchMode=yes', 'ExitOnForwardFailure=yes',
                       'ServerAliveInterval=30', 'StreamLocalBindUnlink=yes'):
            assert f'-o {option}' in command
        assert command.endswith(f'-L /run/dvb/engine.sock:/run/dvb/engine.sock {host}')
        assert {f'Description=dvb engine socket from {host}', 'ConditionUser=desktop',
                'After=network-online.target', 'Restart=always', 'RestartSec=5',
                'WantedBy=default.target'} <= set(lines)
        assert config.read_text() == f'd /run/dvb 0755 desktop {group} -\n'
        runtime = root / 'run/dvb'
        assert runtime.is_dir()
        assert stat.S_IMODE(runtime.stat().st_mode) == 0o755
        assert (runtime.stat().st_uid, runtime.stat().st_gid) == (uid, gid)
        assert stat.S_IMODE(unit.stat().st_mode) == 0o644
        assert stat.S_IMODE(config.stat().st_mode) == 0o644
        assert (root / 'systemctl.log').read_text().splitlines()[-3:] == [
            '--global enable dvb-forward.service dvb-desk.service',
            '--user -M desktop@ daemon-reload',
            '--user -M desktop@ restart dvb-forward.service dvb-desk.service',
        ]
        assert 'systemctl' not in result.stdout
        assert (root / 'tmpfiles.log').read_text().splitlines()[-1] == '--create ' + str(config)
    before = unit.read_text(), config.read_text(), (root / 'tmpfiles.log').read_text()
    invalid = ['', '-engine-host', 'two hosts', 'engine-host\nRestart=no', 'engine-host\tother', 'engine-host;echo', 'engine-host%h', 'engine-host"']
    for host in invalid:
        result = subprocess.run(args + ['--cockpit', '--engine-host', host], env=env, capture_output=True, text=True, timeout=INSTALL_TIMEOUT)
        cases += 1
        assert result.returncode != 0 and '--engine-host <ssh-host>' in result.stderr, result
    for options, message in [
        (['--engine', '--cockpit', '--engine-host', 'engine-host'], 'cannot be used with --engine'),
        (['--engine-host', 'engine-host', '--engine', '--cockpit'], 'cannot be used with --engine'),
        (['--engine-host', 'engine-host'], 'requires --cockpit'),
        (['--cockpit', '--engine-host'], '--engine-host <ssh-host>'),
    ]:
        result = subprocess.run(args + options, env=env, capture_output=True, text=True, timeout=INSTALL_TIMEOUT)
        cases += 1
        assert result.returncode != 0 and message in result.stderr, result
    assert before == (unit.read_text(), config.read_text(), (root / 'tmpfiles.log').read_text())
    return cases


def check_release_layout(root, script, binary, env):
    cases = 0
    # Release layout: install.sh beside the assets, the units in dvb-x/install/.
    shutil.rmtree(root / 'opt/dvb')
    top = root / 'top'
    (top / 'dvb-0.1.1/install').mkdir(parents=True)
    shutil.copyfile(script, top / 'install.sh')
    shutil.copyfile(binary, top / 'dvb-0.1.1/dvb')
    (top / 'dvb-0.1.1/dvb').chmod(0o755)
    for file in ('dvb-desk.service', 'dvb.desktop'):
        shutil.copyfile(source / file, top / 'dvb-0.1.1/install' / file)
    shutil.copytree(source / 'icons', top / 'dvb-0.1.1/install/icons')
    release = ['sh', str(top / 'install.sh'), '--binary', str(top / 'dvb-0.1.1/dvb'), '--owner', 'desktop']
    result = subprocess.run(release + ['--cockpit'], env=env, capture_output=True, text=True, timeout=INSTALL_TIMEOUT)
    cases += 1
    assert result.returncode == 0, result.stderr
    assert (root / 'opt/dvb/current').is_symlink()
    # A missing file stops the install before /opt/dvb or the engine is touched.
    shutil.rmtree(root / 'opt/dvb')
    (top / 'dvb-0.1.1/install/icons/dvb-16.png').unlink()
    result = subprocess.run(release + ['--cockpit'], env=env, capture_output=True, text=True, timeout=INSTALL_TIMEOUT)
    cases += 1
    assert result.returncode != 0 and 'icons/dvb-16.png' in result.stderr, result
    assert not (root / 'opt/dvb').exists()
    (top / 'dvb-0.1.1/install/dvb.desktop').unlink()
    result = subprocess.run(release + ['--cockpit'], env=env, capture_output=True, text=True, timeout=INSTALL_TIMEOUT)
    cases += 1
    assert result.returncode != 0 and 'dvb.desktop' in result.stderr, result
    result = subprocess.run(release + ['--engine'], env=env, capture_output=True, text=True, timeout=INSTALL_TIMEOUT)
    cases += 1
    assert result.returncode != 0 and 'dvb.service' in result.stderr, result
    assert not (root / 'opt/dvb').exists()
    assert not (root / 'opt/dvb/role-engine').exists()
    return release, cases


def check_prerequisites(root, release, env):
    cases = 0
    stubs = root / 'stubs'
    polkitd = root / 'usr/lib/polkit-1/polkitd'
    top = root / 'top'
    # A missing prerequisite is reported, and nothing is written.
    bare = root / 'bare'
    bare.mkdir()
    for command in ('id', 'groupadd', 'useradd', 'usermod'):
        (bare / command).symlink_to(stubs / command)
    (bare / 'dirname').symlink_to(shutil.which('dirname'))
    before = sorted(p for p in root.rglob('*') if 'bare' not in p.parts)
    result = subprocess.run([shutil.which('sh'), str(top / 'install.sh'), '--binary', str(top / 'dvb-0.1.1/dvb'),
                             '--owner', 'desktop', '--engine'],
                            env=dict(env, PATH=str(bare)), capture_output=True, text=True, timeout=INSTALL_TIMEOUT)
    cases += 1
    assert result.returncode != 0, result
    # Without pacman nothing is installed: what is missing is listed.
    for name in ('systemd', 'git', 'nothing was changed'):
        assert name in result.stderr, result
    assert 'polkit' not in result.stderr, result
    assert before == sorted(p for p in root.rglob('*') if 'bare' not in p.parts)
    assert not (root / 'opt/dvb').exists() and not (root / 'var').exists()
    assert not (root / 'usr/local/bin/dvb').exists() and not (root / 'etc/systemd/system').exists()
    # With pacman, missing polkit is installed in one transaction before the
    # release's files are checked; dvb itself is not installed without them.
    polkitd.unlink()
    (root / 'pacman.log').unlink()
    result = subprocess.run(release + ['--engine'], env=env, capture_output=True, text=True, timeout=INSTALL_TIMEOUT)
    cases += 1
    assert result.returncode != 0 and 'dvb.service' in result.stderr, result
    installs = [line for line in (root / 'pacman.log').read_text().splitlines() if line.startswith('-S ')]
    for line in ('openai-codex the Codex CLI (codex)',
                 'claude-code the Claude Code CLI (claude)',
                 'rustup rustup (Rust toolchains)',
                 'mise mise (Node versions a project pins)',
                 'chromium Chromium'):
        assert line in result.stdout, result
        assert result.stdout.index(line) < result.stdout.index('installing with pacman'), result
    assert installs[0] == '-S --needed --noconfirm polkit', installs
    # What runs want is installed one package at a time after the tools.
    assert installs[1:] == ['-S --needed --noconfirm openai-codex',
                            '-S --needed --noconfirm claude-code',
                            '-S --needed --noconfirm --ask=4 rustup',
                            '-S --needed --noconfirm mise',
                            '-S --needed --noconfirm chromium'], installs
    assert not (root / 'opt/dvb').exists()
    # A want pacman lacks or cannot install is a warning; the others still install.
    (root / 'pacman.log').unlink()
    result = subprocess.run(release + ['--engine'], capture_output=True, text=True, timeout=INSTALL_TIMEOUT,
                            env=dict(env, DVB_TEST_PACMAN_MISSING='openai-codex', DVB_TEST_PACMAN_FAILS='mise'))
    cases += 1
    assert result.returncode != 0 and 'dvb.service' in result.stderr, result
    assert "repositories have no openai-codex; runs and the app need the Codex CLI" in result.stderr, result
    assert 'pacman could not install mise' in result.stderr, result
    installs = [line for line in (root / 'pacman.log').read_text().splitlines() if line.startswith('-S ')]
    assert installs[1:] == ['-S --needed --noconfirm claude-code',
                            '-S --needed --noconfirm --ask=4 rustup',
                            '-S --needed --noconfirm mise',
                            '-S --needed --noconfirm chromium'], installs
    # Without a claude-code package, Claude Code's native build is installed only
    # if it matches its recorded sha512.
    builds = root / 'claude'
    builds.mkdir()
    with tempfile.TemporaryDirectory() as staging:
        (Path(staging) / 'package').mkdir()
        executable(Path(staging) / 'package/claude', 'echo tampered\n')
        subprocess.run(['tar', '-czf', str(builds / 'claude-code-linux-x64-2.1.286.tgz'), '-C', staging, 'package'],
                       check=True)
    result = subprocess.run(release + ['--engine'], capture_output=True, text=True, timeout=INSTALL_TIMEOUT,
                            env=dict(env, DVB_TEST_PACMAN_MISSING='claude-code', DVB_CLAUDE_BASE=builds.as_uri()))
    cases += 1
    assert result.returncode != 0 and 'the checksum of Claude Code 2.1.286 does not match' in result.stderr, result
    assert not (root / 'usr/local/bin/claude').exists()
    # Failed downloads and hosts without a package/tarball route give one useful
    # line; Codex readiness depends on what workers can actually find.
    runs = root / 'runs'
    runs.mkdir()
    no_pacman = root / 'no-pacman'
    no_pacman.mkdir()
    for stub in stubs.iterdir():
        if stub.name != 'pacman':
            (no_pacman / stub.name).symlink_to(stub)
    for command in ('sh', 'dirname', 'git', 'curl', 'tar', 'sha256sum', 'awk'):
        (no_pacman / command).symlink_to(shutil.which(command))
    executable(polkitd, ':\n')
    message = ("install.sh: Claude Code isn't installed; install it system-wide using "
               'https://code.claude.com/docs/en/setup#install-claude-code')
    for path in (env['PATH'], str(no_pacman)):
        for codex_ready in (False, True):
            if codex_ready:
                executable(runs / 'codex', ':\n')
            result = subprocess.run(release + ['--engine'], capture_output=True, text=True,
                                    timeout=INSTALL_TIMEOUT,
                                    env=dict(env, PATH=path, DVB_TEST_PACMAN_MISSING='claude-code',
                                             DVB_CLAUDE_BASE=(root / 'no-claude-download').as_uri()))
            cases += 1
            assert result.returncode != 0 and 'dvb.service' in result.stderr, result
            lines = [line for line in result.stderr.splitlines() if "Claude Code isn't installed" in line]
            assert lines == [message + ('; Codex is ready.' if codex_ready else '.')], result
            assert ('Codex is ready' in result.stderr) == codex_ready, result
        (runs / 'codex').unlink()
    executable(runs / 'claude', ':\n')
    result = subprocess.run(release + ['--engine'], env=env, capture_output=True, text=True,
                            timeout=INSTALL_TIMEOUT)
    cases += 1
    assert result.returncode != 0 and 'dvb.service' in result.stderr, result
    assert "Claude Code isn't installed" not in result.stderr, result
    return cases


def check_signing_key(root, script, text, env):
    cases = 0
    # A stamped tag without a pinned key must never take a checksum-only path.
    assets = root / 'releases/v0.1.1'
    assets.mkdir(parents=True)
    tarball = assets / 'dvb-0.1.1-x86_64-linux.tar.gz'
    tarball.write_bytes(b'not a release archive')
    (assets / 'SHA256SUMS').write_text(
        f'{hashlib.sha256(tarball.read_bytes()).hexdigest()}  {tarball.name}\n')
    for key in ('@RELEASE_PUBKEY@', ''):
        script.write_text(text.replace('@RELEASE_TAG@', 'v0.1.1').replace('@RELEASE_PUBKEY@', key))
        result = subprocess.run(['sh', str(script), '--cockpit', '--owner', 'desktop'],
                                env=dict(env, DVB_RELEASE_BASE=(root / 'releases').as_uri(),
                                         DVB_GITHUB_TOKEN='', DVB_GITHUB_TOKEN_FILE=''),
                                capture_output=True, text=True, timeout=INSTALL_TIMEOUT)
        cases += 1
        assert result.returncode != 0 and 'names no signing key; refusing to install it' in result.stderr, result
        assert not (root / 'opt/dvb').exists()
    return cases


def check_rustup(tmpfiles):
    cases = 0
    for cargo, rustup, role in ((False, False, '--engine'), (True, False, '--engine'),
                                (True, True, '--engine'), (True, False, '--cockpit')):
        with tempfile.TemporaryDirectory(prefix='dvb-rustup-test-') as temp:
            root = Path(temp)
            args, env, script, _, text = prepare(root, tmpfiles)
            # Exercise only prerequisites; no service or release operations.
            script.write_text(text.split('\nif [ -z "$binary" ]; then\n  command -v curl')[0])
            runs = root / 'runs'
            runs.mkdir()
            if cargo:
                executable(runs / 'cargo', ':\n')
            if rustup:
                executable(runs / 'rustup', ':\n')
            # Like Arch's packages, system Rust conflicts with rustup. Pacman
            # refuses the removal by default under --noconfirm (question bit 4).
            executable(root / 'stubs/pacman',
                       'printf "%s\\n" "$*" >> "$DVB_TEST_PACMAN_LOG"\n'
                       'case " $* " in\n'
                       '  " -Q rust "|" -Q rustup ")\n'
                       '    if [ -x "$DVB_TEST_ROOT/runs/rustup" ]; then\n'
                       '      echo "rustup 1.28.2-1"\n'
                       '    elif [ "$2" = rust ] && [ -x "$DVB_TEST_ROOT/runs/cargo" ]; then\n'
                       '      echo "rust 1.90.0-1"\n'
                       '    else exit 1; fi ;;\n'
                       '  *" -Q "*) exit 1 ;;\n'
                       '  *" -S "*" rustup "*)\n'
                       '    if [ -x "$DVB_TEST_ROOT/runs/cargo" ]; then\n'
                       '      case " $* " in *" --ask=4 "*) ;; *) exit 1 ;; esac\n'
                       '    fi\n'
                       '    printf "#!/bin/sh\\nexit 0\\n" > "$DVB_TEST_ROOT/runs/rustup"\n'
                       '    chmod +x "$DVB_TEST_ROOT/runs/rustup" ;;\n'
                       'esac\n')
            result = subprocess.run(args + [role], env=env, capture_output=True, text=True,
                                    timeout=INSTALL_TIMEOUT)
            cases += 1
            assert result.returncode == 0, result
            replacement = "install.sh: replacing pacman's"
            if cargo and not rustup and role == '--engine':
                assert (replacement + ' rust with rustup (provides cargo; '
                        'runs need the toolchain a project pins)') in result.stderr, result
            else:
                assert replacement not in result.stderr, result
            installs = [line for line in (root / 'pacman.log').read_text().splitlines()
                        if line.startswith('-S ')]
            if role == '--engine':
                assert (runs / 'rustup').is_file(), (cargo, rustup, installs, result.stderr)
                assert 'replace the rust package' not in result.stderr, result
                assert sum(line.endswith(' rustup') for line in installs) == int(not rustup), installs
            else:
                assert not (runs / 'rustup').exists(), installs
            assert all('--ask' not in line for line in installs if not line.endswith(' rustup')), installs
    return cases


def check_engine_paths(tmpfiles):
    cases = 0
    for filesystem, existing, btrfs, ram_kb in [
            ('btrfs', False, True, 33423360), ('btrfs', True, True, 33423360),
            ('ext2/ext3', False, True, 33423360), ('btrfs', False, False, 33423360),
            ('ext2/ext3', False, False, 3 * 1024 * 1024),
            ('ext2/ext3', False, False, 1024 * 1024)]:
        with tempfile.TemporaryDirectory(prefix='dvb-install-engine-') as temp:
            root = Path(temp)
            args, env, script, _, text = prepare(root, tmpfiles)
            stubs = root / 'stubs'
            for file in source.glob('dvb*'):
                if file.is_file():
                    shutil.copyfile(file, root / file.name)
            shutil.copytree(source / 'dvb-run-.service.d', root / 'dvb-run-.service.d')
            shutil.copyfile(source / '50-dvb.rules', root / '50-dvb.rules')
            executable(stubs / 'id', 'echo 0\n')
            executable(stubs / 'getent', ':\n')
            executable(stubs / 'usermod', ':\n')
            executable(stubs / 'systemctl', 'printf "%s\\n" "$*" >> "$DVB_TEST_SYSTEMCTL_LOG"\n')
            real_install = shutil.which('install')
            # Ownership is the only root operation bypassed; file modes and paths are real.
            install = stubs / 'install'
            install.write_text(f'#!{sys.executable} -I\nimport os, sys\n'
                               'args = iter(sys.argv[1:]); kept = []\n'
                               'for arg in args:\n'
                               '    if arg in ("-o", "-g"): next(args)\n'
                               '    else: kept.append(arg)\n'
                               f'os.execv({real_install!r}, ["install", *kept])\n')
            install.chmod(0o755)
            executable(stubs / 'stat', f'echo {filesystem}\n')
            if btrfs:
                executable(stubs / 'btrfs',
                           'printf "%s\\n" "$*" >> "$DVB_TEST_ROOT/btrfs.log"\n'
                           '[ "$1 $2" = "subvolume create" ]; mkdir "$3"\n')
            else:
                text = text.replace('command -v btrfs', 'command -v dvb-test-missing-btrfs')
            (root / 'var/lib').mkdir(parents=True)
            data = root / 'var/lib/dvb'
            if existing:
                data.mkdir(mode=0o711)
                (data / 'keep').write_text('existing data')
            (root / 'proc').mkdir()
            (root / 'proc/meminfo').write_text(f'MemTotal:       {ram_kb} kB\n')
            # Exercise the real download/hash/extract path with a local sccache archive.
            archive_dir = root / 'sccache-v0.12.0-x86_64-unknown-linux-musl'
            archive_dir.mkdir()
            executable(archive_dir / 'sccache', ':\n')
            archive = root / 'sccache.tar.gz'
            subprocess.run(['tar', '-czf', str(archive), '-C', str(root), archive_dir.name], check=True)
            text = text.replace('b0e89ead6899224a4ba2b90e9073bf1ce036d95bab30f3dc33c1e1468bc4ad44',
                                hashlib.sha256(archive.read_bytes()).hexdigest())
            executable(stubs / 'curl', f'cp "{archive}" "$3"\n')
            script.write_text(text)
            slice_file = root / 'etc/systemd/system/dvb.slice'
            slice_file.parent.mkdir(parents=True)
            if existing:
                slice_file.write_text('[Slice]\nMemoryMax=8G\nMemoryHigh=6G\n')
            result = subprocess.run(args + ['--engine'], env=env, capture_output=True,
                                    text=True, timeout=INSTALL_TIMEOUT)
            cases += 1
            assert result.returncode == 0, result.stderr
            assert data.is_dir()
            made_subvolume = filesystem == 'btrfs' and not existing and btrfs
            log = root / 'btrfs.log'
            assert log.exists() == made_subvolume
            if made_subvolume:
                assert log.read_text() == f'subvolume create {data}\n'
                assert 'keeps dvb' in result.stdout and 'system snapshots' in result.stdout
            if existing:
                assert stat.S_IMODE(data.stat().st_mode) == 0o711
                assert (data / 'keep').read_text() == 'existing data'
                assert slice_file.read_text() == '[Slice]\nMemoryMax=8G\nMemoryHigh=6G\n'
            else:
                limit = max(2, ram_kb // (1024 * 1024 * 2))
                assert {f'MemoryMax={limit}G', f'MemoryHigh={limit * 896}M'} <= set(
                    slice_file.read_text().splitlines())
                assert stat.S_IMODE(slice_file.stat().st_mode) == 0o644
            drop_in = (root / 'etc/systemd/system/dvb-run-.service.d/10-common.conf').read_text()
            assert 'Slice=dvb.slice' in drop_in and 'MemoryMax=8G' in drop_in
    return cases


def check_release_workflow():
    workflow = (source.parent / '.github/workflows/release.yml').read_text()
    # Top-level job boundaries keep the key's environment and event guard in
    # the same job; a guarded step alone cannot declare an Actions environment.
    jobs = dict(re.findall(r'^  ([a-z_]+):\n(.*?)(?=^  [a-z_]+:|\Z)',
                           workflow.split('jobs:\n', 1)[1], re.M | re.S))
    key = '${{ secrets.MINISIGN_SECRET_KEY }}'
    assert workflow.count(key) == 1
    signing = [job for job, body in jobs.items() if key in body]
    assert len(signing) == 1
    sign = jobs[signing[0]]
    gate = "    if: github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')"
    assert gate in sign.splitlines(), 'production signing must require a v* tag push'
    assert '    environment: release' in sign.splitlines()
    assert '    needs: build' in sign.splitlines()
    build = jobs['build']
    assert 'environment:' not in build
    assert key not in build
    assert 'minisign -G -W -f -p "$RUNNER_TEMP/throwaway.pub"' in build
    for step in re.split(r'^      - ', build, flags=re.M)[1:]:
        if ('throwaway' in step or 'scripts/release.sh' in step
                or '          name: release' in step.splitlines()):
            assert "        if: github.event_name == 'pull_request'" in step.splitlines()
    assert '          name: release-binary' in build.splitlines()
    assert '          name: release-binary' in sign.splitlines()
    assert '          name: release' in sign.splitlines()
    publish = jobs['publish']
    assert gate in publish.splitlines()
    assert f'    needs: {signing[0]}' in publish.splitlines()
    assert '          name: release' in publish.splitlines()
    assert '--repo broken-branch/dvb' in publish
    return 1


def main():
    tmpfiles = shutil.which('systemd-tmpfiles')
    assert tmpfiles, 'systemd-tmpfiles is required'
    with tempfile.TemporaryDirectory(prefix='dvb-install-test-') as temp:
        root = Path(temp)
        args, env, script, binary, text = prepare(root, tmpfiles)
        cases = check_local_cockpit(root, args, binary, env)
        cases += check_remote_cockpit(root, args, env)
        release, count = check_release_layout(root, script, binary, env)
        cases += count
        cases += check_prerequisites(root, release, env)
        cases += check_signing_key(root, script, text, env)
    cases += check_rustup(tmpfiles)
    cases += check_engine_paths(tmpfiles)
    cases += check_release_workflow()
    print(f'Installer: {cases} cases passed; all writes stayed in the temporary tree.')


if __name__ == '__main__':
    main()
