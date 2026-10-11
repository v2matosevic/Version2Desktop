"""Build reproducible offline payloads, portable ZIPs and per-user NSIS installers.

Run from the public repository. No app/system changes or installer execution.
Third-party notices are gathered from exact locked Cargo packages and supplied
upstream notices. Vesper also requires its pinned runtime and decoder notices.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tomllib
import zipfile


def sha(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for block in iter(lambda: f.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def escaped(value):
    return str(value).replace('$', '$$').replace('"', '$\\"')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--app', choices=['sonora', 'luma', 'vesper'], required=True)
    parser.add_argument('--exe', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--decoder', type=Path)
    parser.add_argument('--decoder-notices', type=Path)
    parser.add_argument('--nsis', type=Path, default=Path(os.environ['LOCALAPPDATA']) / 'tauri/NSIS/makensis.exe')
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    app = root / 'apps' / args.app
    title = args.app.capitalize()
    version = tomllib.loads((app / 'Cargo.toml').read_text())['package']['version']
    args.out.mkdir(parents=True, exist_ok=True)
    stage = args.out / f'{title}-{version}-Windows-x64'
    if stage.exists():
        raise RuntimeError('Use a fresh output directory; existing payloads are preserved.')
    stage.mkdir()
    shutil.copy2(args.exe, stage / f'{args.app}.exe')
    for name in ['README.md', 'LICENSE']:
        shutil.copy2(root / name, stage / name)
    shutil.copy2(root / 'shared/v2-ui/NOTICE', stage / 'VERSION2-NOTICE.txt')
    shutil.copytree(root / 'docs', stage / 'docs')
    meta = json.loads(subprocess.check_output(['cargo', 'metadata', '--locked', '--format-version', '1'], cwd=app))
    tree = subprocess.check_output(['cargo', 'tree', '--locked', '--target', 'x86_64-pc-windows-msvc', '--edges', 'normal,build', '--prefix', 'none', '--format', '{p}'], cwd=app, text=True)
    selected = set()
    for line in tree.splitlines():
        match = re.match(r'([\w-]+) v([\w.+-]+)', line)
        if match:
            selected.add(match.groups())
    inventory = []
    for package in sorted(meta['packages'], key=lambda p: (p['name'], p['version'])):
        if (package['name'], package['version']) not in selected:
            continue
        base = Path(package['manifest_path']).parent
        identifier = f"{package['name']}-{package['version']}"
        if package['source'] is None and package['name'] in [args.app, 'v2-ui']:
            continue
        dest = stage / 'licenses/rust' / identifier
        dest.mkdir(parents=True)
        texts = [p for p in base.rglob('*') if p.is_file() and p.name.lower().startswith(('license', 'licence', 'copying', 'copyright', 'notice', 'patents', 'authors')) and 'target' not in p.parts]
        for p in texts:
            out = dest / p.relative_to(base)
            out.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(p, out)
        supplemental = app / 'notices' / identifier
        if supplemental.exists():
            shutil.copytree(supplemental, dest / 'upstream', dirs_exist_ok=True)
        if not any(p.is_file() for p in dest.rglob('*')):
            raise RuntimeError('Missing dependency notices: ' + identifier)
        if 'MPL' in (package.get('license') or ''):
            shutil.copytree(base, stage / 'source/MPL' / identifier, ignore=shutil.ignore_patterns('.git', 'target'))
        inventory.append({'name': package['name'], 'version': package['version'], 'license': package.get('license'), 'repository': package.get('repository'), 'notices': [p.relative_to(stage).as_posix() for p in dest.rglob('*') if p.is_file()]})
    (stage / 'dependencies.json').write_text(json.dumps(inventory, indent=2) + '\n')
    notice = '# Third-party notices\n\nExact locked Rust dependency versions and license files are mapped in dependencies.json. Full texts are in licenses/rust. MPL source is in source/MPL when applicable. No Windows fonts or optional Windows image codecs are redistributed.\n'
    if args.app == 'vesper':
        if args.decoder is None or args.decoder_notices is None:
            raise RuntimeError('Vesper needs --decoder and --decoder-notices.')
        lock = json.loads((root / 'decoder-packages.lock.json').read_text())
        for record in lock['dlls']:
            file = args.decoder / record['name']
            if sha(file) != record['sha256']:
                raise RuntimeError('Decoder hash mismatch: ' + file.name)
            shutil.copy2(file, stage / file.name)
        for package in lock['packages']:
            folder = args.decoder_notices / package['name']
            if folder.exists():
                shutil.copytree(folder, stage / 'licenses/decoder' / package['name'])
        shutil.copy2(root / 'decoder-packages.lock.json', stage / 'decoder-packages.lock.json')
        shutil.copy2(root / 'licenses/GPL-3.0-or-later.txt', stage / 'COPYING-GPL.txt')
        notice += '\nThis complete Vesper Windows distribution is GPL-3.0-or-later. First-party source remains MIT. Native mpv/FFmpeg and dependency package notices are in licenses/decoder. Exact corresponding source/build recipes and metadata are available alongside this installer at https://github.com/v2matosevic/Version2Desktop/releases/tag/v' + version + ' as Vesper-' + version + '-decoder-source.zip. That source asset is for developers, not a runtime requirement. decoder-packages.lock.json identifies every exact binary and source archive with SHA256.\n'
    (stage / 'THIRD-PARTY-NOTICES.md').write_text(notice)
    records = [{'path': p.relative_to(stage).as_posix(), 'bytes': p.stat().st_size, 'sha256': sha(p)} for p in sorted(stage.rglob('*')) if p.is_file()]
    (stage / 'manifest.json').write_text(json.dumps(records, indent=2) + '\n')
    archive = args.out / f'{title}-{version}-Windows-x64-Portable.zip'
    with zipfile.ZipFile(archive, 'w', zipfile.ZIP_DEFLATED, compresslevel=9, strict_timestamps=False) as z:
        for p in sorted(stage.rglob('*')):
            if p.is_file():
                z.write(p, Path(stage.name) / p.relative_to(stage))
    installer = args.out / f'{title}-{version}-Setup.exe'
    files = sorted(p for p in stage.rglob('*') if p.is_file())
    deletes = '\n'.join('  !insertmacro DeletePayload "$INSTDIR\\' + escaped(p.relative_to(stage)) + '"' for p in files)
    directories = sorted({p for p in stage.rglob('*') if p.is_dir()}, key=lambda p: (len(p.parts), str(p)), reverse=True)
    deletes += '\n' + '\n'.join('  RMDir "$INSTDIR\\' + escaped(p.relative_to(stage)) + '"' for p in directories)
    icon = app / 'assets/icon.ico'
    if not icon.exists():
        icon = root / 'shared/assets' / args.app / 'icon.ico'
    nsis = f'''Unicode True
!include "MUI2.nsh"
Name "{title}"
OutFile "{escaped(installer.resolve())}"
InstallDir "$LOCALAPPDATA\\Programs\\Version2\\{title}"
RequestExecutionLevel user
SetCompressor /SOLID lzma
VIProductVersion "{version}.0"
VIAddVersionKey /LANG=1033 "ProductName" "{title}"
VIAddVersionKey /LANG=1033 "FileVersion" "{version}"
VIAddVersionKey /LANG=1033 "FileDescription" "{title} Windows installer"
VIAddVersionKey /LANG=1033 "LegalCopyright" "Copyright 2026 Marko / Version2"
!define MUI_ICON "{escaped(icon.resolve())}"
!define MUI_UNICON "{escaped(icon.resolve())}"
!define MUI_ABORTWARNING
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "{escaped((stage / ('COPYING-GPL.txt' if args.app == 'vesper' else 'LICENSE')).resolve())}"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"
!macro DeletePayload FILE
  ClearErrors
  Delete "${{FILE}}"
  IfErrors payload_delete_failed 0
!macroend
Section "{title}"
  SectionIn RO
  SetShellVarContext current
  SetOutPath "$INSTDIR"
  ClearErrors
  File /r "{escaped(stage.resolve())}\\*"
  IfErrors install_failed 0
  WriteUninstaller "$INSTDIR\\Uninstall.exe"
  CreateDirectory "$SMPROGRAMS\\Version2"
  CreateShortcut "$SMPROGRAMS\\Version2\\{title}.lnk" "$INSTDIR\\{args.app}.exe"
  WriteRegStr HKCU "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Version2{title}" "DisplayName" "{title}"
  WriteRegStr HKCU "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Version2{title}" "DisplayVersion" "{version}"
  WriteRegStr HKCU "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Version2{title}" "Publisher" "Version2"
  WriteRegStr HKCU "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Version2{title}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Version2{title}" "DisplayIcon" "$INSTDIR\\{args.app}.exe,0"
  WriteRegStr HKCU "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Version2{title}" "UninstallString" '$\\"$INSTDIR\\Uninstall.exe$\\"'
  WriteRegDWORD HKCU "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Version2{title}" "NoModify" 1
  WriteRegDWORD HKCU "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Version2{title}" "NoRepair" 1
  Goto install_done
  install_failed:
  SetErrorLevel 1
  IfSilent +2 0
  MessageBox MB_OK|MB_ICONEXCLAMATION "Files could not be installed. Close {title} yourself and retry."
  Abort
  install_done:
SectionEnd
Section /o "Offer {title} in Windows Open with (no defaults)"
  WriteRegStr HKCU "Software\\Classes\\Applications\\{args.app}.exe" "FriendlyAppName" "{title}"
  WriteRegStr HKCU "Software\\Classes\\Applications\\{args.app}.exe\\shell\\open\\command" "" '$\\"$INSTDIR\\{args.app}.exe$\\" $\\"%1$\\"'
SectionEnd
Section "Uninstall"
  SetShellVarContext current
{deletes}
  ReadRegStr $0 HKCU "Software\\Classes\\Applications\\{args.app}.exe\\shell\\open\\command" ""
  StrCmp $0 '$\\"$INSTDIR\\{args.app}.exe$\\" $\\"%1$\\"' 0 open_with_done
  DeleteRegValue HKCU "Software\\Classes\\Applications\\{args.app}.exe" "FriendlyAppName"
  DeleteRegValue HKCU "Software\\Classes\\Applications\\{args.app}.exe\\shell\\open\\command" ""
  DeleteRegKey /ifempty HKCU "Software\\Classes\\Applications\\{args.app}.exe\\shell\\open\\command"
  DeleteRegKey /ifempty HKCU "Software\\Classes\\Applications\\{args.app}.exe\\shell\\open"
  DeleteRegKey /ifempty HKCU "Software\\Classes\\Applications\\{args.app}.exe\\shell"
  DeleteRegKey /ifempty HKCU "Software\\Classes\\Applications\\{args.app}.exe"
  open_with_done:
  ReadRegStr $0 HKCU "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Version2{title}" "InstallLocation"
  StrCmp $0 "$INSTDIR" 0 registry_done
  DeleteRegKey HKCU "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Version2{title}"
  Delete "$SMPROGRAMS\\Version2\\{title}.lnk"
  RMDir "$SMPROGRAMS\\Version2"
  registry_done:
  Delete "$INSTDIR\\Uninstall.exe"
  RMDir "$INSTDIR"
  Goto uninstall_done
  payload_delete_failed:
  SetErrorLevel 1
  IfSilent +2 0
  MessageBox MB_OK|MB_ICONEXCLAMATION "A file is in use. Close {title} yourself and retry. Its uninstall record was preserved."
  Abort
  uninstall_done:
SectionEnd
'''
    script = args.out / f'{title}-{version}-installer.nsi'
    script.write_text(nsis)
    result = subprocess.run([str(args.nsis), '/V2', str(script)], capture_output=True, text=True)
    if result.returncode or 'warning ' in (result.stdout + result.stderr).lower():
        raise RuntimeError(result.stdout + result.stderr)
    for p in [archive, installer]:
        print(json.dumps({'path': str(p), 'bytes': p.stat().st_size, 'sha256': sha(p)}))


if __name__ == '__main__':
    main()
