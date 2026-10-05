#!/usr/bin/env python3
"""Close macdeployqt's residual @rpath dependencies inside a bundle.

Never copy arbitrary missing libraries or suppress errors: an unresolved
non-system dependency fails before signing. LC_ID_DYLIB is not a dependency.
"""
import os
from pathlib import Path
import subprocess
import sys


def output(*args):
    return subprocess.check_output(args, text=True)


def bundle_target(app, binary, dependency):
    if dependency.startswith('@loader_path/'):
        return binary.parent / dependency[len('@loader_path/'):]
    if dependency.startswith('@executable_path/'):
        return app / 'Contents/MacOS' / dependency[len('@executable_path/'):]
    if dependency.startswith('@rpath/'):
        return app / 'Contents/Frameworks' / dependency[len('@rpath/'):]
    return Path(dependency)


def repair(app):
    app = Path(app).resolve()
    frameworks = app / 'Contents/Frameworks'
    for binary in sorted((app / 'Contents').rglob('*')):
        if not binary.is_file() or binary.is_symlink():
            continue
        if 'Mach-O' not in output('file', '-b', str(binary)):
            continue
        ids = output('otool', '-D', str(binary)).splitlines()[1:]
        identity = ids[0].strip() if ids else None
        new_identity = None
        if identity and binary.is_relative_to(frameworks):
            new_identity = '@rpath/' + str(binary.relative_to(frameworks))
            subprocess.run(['install_name_tool', '-id', new_identity, str(binary)], check=True)
        for line in output('otool', '-L', str(binary)).splitlines()[1:]:
            dependency = line.strip().split(' (compatibility version', 1)[0]
            # Skip the library's own install ID, whether old or just normalized.
            if dependency == identity or dependency == new_identity:
                continue
            if dependency.startswith(('/System/Library/', '/usr/lib/')):
                continue
            target = bundle_target(app, binary, dependency).resolve()
            if not target.is_file() or not target.is_relative_to(app):
                raise RuntimeError(f'Unresolved/nonportable dependency: {binary}: {dependency}')
            if dependency.startswith('@rpath/'):
                relative = os.path.relpath(target, binary.parent)
                subprocess.run(['install_name_tool', '-change', dependency, '@loader_path/' + relative, str(binary)], check=True)
    print('Verified and repaired all non-system dependencies inside', app)


if __name__ == '__main__':
    if len(sys.argv) != 2:
        sys.exit('Usage: fix_macos_dependencies.py app-bundle')
    repair(sys.argv[1])
