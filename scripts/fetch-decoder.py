"""Fetch an exact, checksum-pinned native decoder runtime for builds. No global installs."""
import argparse,hashlib,io,json,subprocess,tarfile,urllib.request
from pathlib import Path

def main():
    p=argparse.ArgumentParser();p.add_argument('--out',type=Path,required=True);a=p.parse_args()
    root=Path(__file__).resolve().parents[1]
    lock=json.loads((root/'decoder-packages.lock.json').read_text())
    a.out.mkdir(parents=True,exist_ok=True)
    cache=a.out/'packages';cache.mkdir(exist_ok=True)
    expected={x['name'].lower():x for x in lock['dlls']};written=set()
    for pkg in lock['packages']:
        dest=cache/Path(pkg['url']).name
        if not dest.exists():
            with urllib.request.urlopen(pkg['url'],timeout=120) as response,dest.open('xb') as output:
                while data:=response.read(1024*1024):output.write(data)
        if hashlib.sha256(dest.read_bytes()).hexdigest()!=pkg['sha256']:raise RuntimeError('Archive checksum failed: '+dest.name)
        raw=subprocess.check_output(['7z','x',str(dest),'-so'],stderr=subprocess.PIPE)
        with tarfile.open(fileobj=io.BytesIO(raw)) as archive:
            for entry in archive:
                name=Path(entry.name).name
                if entry.isfile() and entry.name.startswith('ucrt64/bin/') and name.lower() in expected:
                    data=archive.extractfile(entry).read()
                    if hashlib.sha256(data).hexdigest()!=expected[name.lower()]['sha256']:raise RuntimeError('DLL checksum failed: '+name)
                    (a.out/name).write_bytes(data);written.add(name.lower())
    if written!=set(expected):raise RuntimeError('Incomplete runtime: '+str(set(expected)-written))
    print(json.dumps({'dlls':len(written),'result':'passed'}))

if __name__=='__main__':main()
