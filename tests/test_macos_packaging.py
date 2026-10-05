"""Packaging regressions run with stdlib-only unittest on any CI host."""
import importlib.util
from unittest.mock import patch
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class DmgPackaging(unittest.TestCase):
    def test_direct_compressed_creation_and_stage_cleanup(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            app = root / 'My App.app'
            (app / 'Contents').mkdir(parents=True)
            (app / 'Contents' / 'marker').write_text('app payload')
            icon = root / 'icon.icns'; icon.write_text('icon')
            installer = root / 'installer.sh'; installer.write_text('#!/bin/bash\n')
            output = root / 'output.dmg'
            bin_dir = root / 'bin'; bin_dir.mkdir()
            log = root / 'calls'
            stage_log = root / 'stage'
            hdiutil = bin_dir / 'hdiutil'
            hdiutil.write_text('''#!/bin/bash
set -eu
printf '%s\\n' "$*" >> "$CALL_LOG"
if [ "$1" = create ]; then
    stage=""; previous=""
    for argument in "$@"; do
        [ "$previous" != -srcfolder ] || stage="$argument"
        previous="$argument"
    done
    test -f "$stage/My App.app/Contents/marker"
    test -x "$stage/Install CLI Tools.command"
    test -L "$stage/Applications"
    printf '%s' "$stage" > "$STAGE_LOG"
    touch "${@: -1}"
elif [ "$1" = verify ]; then
    test -f "$2"
else
    echo 'Writable mount cycle must not be used' >&2
    exit 16
fi
''')
            hdiutil.chmod(0o755)
            env = {**os.environ, 'PATH': str(bin_dir) + ':' + os.environ['PATH'],
                   'CALL_LOG': str(log), 'STAGE_LOG': str(stage_log), 'TMPDIR': str(root)}
            result = subprocess.run(['bash', str(ROOT / 'scripts/package_macos_dmg.sh'),
                                     str(app), str(icon), str(installer), str(output)],
                                    env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn('-format UDZO', log.read_text())
            self.assertNotIn('attach', log.read_text())
            self.assertFalse(Path(stage_log.read_text()).exists())

    def test_input_failure_does_not_create_image(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / 'must-not-exist.dmg'
            result = subprocess.run(['bash', str(ROOT / 'scripts/package_macos_dmg.sh'),
                                     '/missing/app', '/missing/icon', '/missing/installer', str(output)],
                                    capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(output.exists())

    def test_residual_rpath_is_rewritten_and_install_id_not_a_dependency(self):
        spec = importlib.util.spec_from_file_location('dependency_fix', ROOT / 'scripts/fix_macos_dependencies.py')
        module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
        with tempfile.TemporaryDirectory() as tmp:
            app = Path(tmp) / 'Test.app'
            libs = app / 'Contents/Frameworks'; libs.mkdir(parents=True)
            a = libs / 'a.dylib'; a.touch(); a = a.resolve()
            b = libs / 'b.dylib'; b.touch(); b = b.resolve()
            def output(*args):
                if args[0] == 'file': return 'Mach-O 64-bit dynamically linked shared library'
                path = Path(args[-1]); identity = '/opt/homebrew/lib/' + path.name
                if args[1] == '-D': return str(path) + ':\n' + identity + '\n'
                dep = '@rpath/b.dylib' if path.name == 'a.dylib' else '/usr/lib/libSystem.B.dylib'
                return str(path) + ':\n\t' + identity + ' (compatibility version 1.0.0)\n\t' + dep + ' (compatibility version 1.0.0)\n'
            with patch.object(module, 'output', side_effect=output), patch.object(module.subprocess, 'run') as run:
                module.repair(app)
            calls = [c.args[0] for c in run.call_args_list]
            self.assertIn(['install_name_tool', '-change', '@rpath/b.dylib', '@loader_path/b.dylib', str(a)], calls)
            self.assertIn(['install_name_tool', '-id', '@rpath/a.dylib', str(a)], calls)

    def test_unresolved_dependency_is_not_silently_accepted(self):
        spec = importlib.util.spec_from_file_location('dependency_fix', ROOT / 'scripts/fix_macos_dependencies.py')
        module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
        with tempfile.TemporaryDirectory() as tmp:
            app = Path(tmp) / 'Test.app'; binary = app / 'Contents/MacOS/test'; binary.parent.mkdir(parents=True); binary.touch()
            def output(*args):
                if args[0] == 'file': return 'Mach-O 64-bit executable'
                if args[1] == '-D': return str(binary) + ':\n'
                return str(binary) + ':\n\t@rpath/missing.dylib (compatibility version 1.0.0)\n'
            with patch.object(module, 'output', side_effect=output):
                with self.assertRaisesRegex(RuntimeError, 'Unresolved/nonportable'):
                    module.repair(app)

    def test_bundle_verifies_signatures_and_isolated_launches(self):
        script = (ROOT / 'build_macos.sh').read_text()
        self.assertIn('-no-plugins -no-codesign', script)
        self.assertIn('codesign --verify --deep --strict', script)
        self.assertIn('env -i PATH=/usr/bin:/bin', script)
        self.assertIn('platforms/libqcocoa.dylib', script)
        self.assertIn('imageformats/libqsvg.dylib', script)
        self.assertNotIn('SetFile', script)


if __name__ == '__main__':
    unittest.main()
