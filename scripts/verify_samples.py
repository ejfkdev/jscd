#!/usr/bin/env python3
"""跨版本样本回归：work/samples/*.js × 所有 node 版本 → jsc → 反编译 → 语法/加载检查。

检查项：
  1) 语法：用该版本的 node --check 检查产物（硬要求）
  2) 加载：在 vm 里以最小 test262 桩（assert/$DONE/$262/print）执行产物，
     把 ReferenceError 且名字是我们生成的模式（__ / _anon_ / ctx / phi）当作**我们的 bug**，
     其他错误算测试自身的断言/逻辑（不计入）。
用法: python3 scripts/verify_samples.py [node版本 ...] [--limit N] [--quiet]
"""
import json, os, pathlib, re, subprocess, sys, collections

ROOT = pathlib.Path(__file__).resolve().parent.parent
SAMPLES = ROOT / 'work' / 'samples'
OUT = ROOT / 'work' / 'out'
VERSIONS_ALL = ['8.17.0', '10.24.1', '12.22.12', '14.21.3', '16.20.2', '18.20.8', '20.20.2', '22.12.0', '24.12.0']
OURS = re.compile(r'\b(__[A-Za-z0-9_]*|_anon_[0-9]+|ctx[0-9]+|phi[0-9]+|r[0-9]+)\b')


_NODE_BIN = {}


def node_bin(version):
    if version not in _NODE_BIN:
        r = subprocess.run(['mise', 'which', 'node', '--', f'node@{version}'],
                           capture_output=True, text=True, cwd=ROOT)
        path = (r.stdout or '').strip().splitlines()[-1] if r.stdout else ''
        _NODE_BIN[version] = path or 'node'
    return _NODE_BIN[version]


def mise(version, *args, timeout=60):
    # 直接调二进制：省掉每次 `mise exec` 的启动开销（矩阵里会跑上千次）
    return subprocess.run([node_bin(version), *args],
                          capture_output=True, text=True, timeout=timeout, cwd=ROOT)


def run(version, files, quiet=False):
    OUT.mkdir(parents=True, exist_ok=True)
    stats = collections.Counter()
    failures = []
    ro = ROOT / 'workspace' / 'ro' / f'ro-map-{version}.json'
    for src in files:
        jsc = OUT / f'{src.stem}-{version}.jsc'
        dec = OUT / f'{src.stem}-{version}.js'
        r = mise(version, '--experimental-vm-modules', str(ROOT / 'scripts' / 'mkcorpus.js'), str(src), str(jsc))
        if r.returncode != 0 or not jsc.exists():
            # 编译不了的多半是 ESM 提案语法（`import defer` 之类）或老 node 无模块支持
            stats['compile-fail'] += 1
            failures.append((src.name, 'compile-env', r.stderr.strip().splitlines()[0][:80] if r.stderr.strip() else ''))
            continue
        cmd = [str(ROOT / 'target' / 'release' / 'jscd'), 'decompile', str(jsc)]
        if ro.exists():
            cmd += ['--ro-map', str(ro)]
        d = subprocess.run(cmd, capture_output=True, text=True, cwd=ROOT, timeout=120)
        if d.returncode != 0:
            stats['decompile-fail'] += 1
            failures.append((src.name, 'decompile', (d.stderr.strip() or d.stdout.strip())[:90]))
            continue
        dec.write_text(d.stdout)
        c = mise(version, '--check', str(dec))
        if c.returncode != 0:
            stats['syntax-fail'] += 1
            first = [l for l in c.stderr.splitlines() if l.strip()][:2]
            failures.append((src.name, 'syntax', ' | '.join(first)[:110]))
            continue
        # 加载检查：最小 test262 桩；只把"我们生成的名字"报 ReferenceError 当作我们的 bug
        harness = (
            "const fs=require('fs'),vm=require('vm');"
            "const src=fs.readFileSync(process.argv[1],'utf8');"
            "const sandbox={console,print(){},assert:Object.assign(function(){},)"
            "{sameValue(){},notSameValue(){},throws(){},fail(){}},$DONE(){},$262:{},}"
            "Symbol,Object,Array,String,Number,Boolean,Math,JSON,Error,TypeError,RangeError"
            " ,SyntaxError,ReferenceError,RegExp,Date,Map,Set,WeakMap,WeakSet,Promise,Proxy,Reflect"
            " ,parseInt,parseFloat,isNaN,isFinite,decodeURI,encodeURI,undefined,NaN,Infinity,globalThis"
            " ,ArrayBuffer,DataView,Uint8Array,Int8Array,Float64Array,BigInt,Function,eval,Infinity};"
            "sandbox.globalThis=sandbox;vm.createContext(sandbox);"
            "try{vm.runInContext(src,sandbox,{timeout:3000});process.stdout.write('OK');}"
            "catch(e){process.stdout.write((e&&e.constructor&&e.constructor.name||'Err')+':'+String(e&&e.message).slice(0,120));}"
        )
        (OUT / 'harness.js').write_text(harness)
        h = mise(version, str(OUT / 'harness.js'), str(dec))
        out = (h.stdout or '').strip()
        if out.startswith('OK'):
            stats['ok'] += 1
        elif out.startswith('ReferenceError') and OURS.search(out):
            stats['ref-error(ours)'] += 1
            failures.append((src.name, 'ref-ours', out[:110]))
        else:
            stats['ran(other)'] += 1
    return stats, failures


def main():
    argv = sys.argv[1:]
    limit = None
    quiet = '--quiet' in argv
    if '--limit' in argv:
        i = argv.index('--limit')
        limit = int(argv[i + 1])
        argv = argv[:i] + argv[i + 2:]
    versions = [a for a in argv if not a.startswith('--')] or VERSIONS_ALL
    files = sorted(SAMPLES.glob('*.js'))
    if limit:
        files = files[:limit]
    total = collections.Counter()
    for v in versions:
        stats, failures = run(v, files, quiet)
        total.update(stats)
        print(f'--- node {v}: ' + ' '.join(f'{k}={n}' for k, n in sorted(stats.items())))
        if not quiet:
            for name, kind, msg in failures[:12]:
                print(f'      {kind:14s} {name:46s} {msg}')
    print('==== 合计 ' + ' '.join(f'{k}={n}' for k, n in sorted(total.items())))


if __name__ == '__main__':
    main()
