// 整脚本对拍：fixture 是"完整脚本"（不是导出 target 的用例）——
// 用 node 跑原脚本与反编译产物，比 stdout 与退出码。
// 覆盖那些 `target` 型用例照不到的形态（顶层只有语句、只有 const、只有 class …）。
// 用法: node scripts/script_diff.js <name> <node-version> [--ro-map PATH]
const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');

const [name, nodeVer, ...rest] = process.argv.slice(2);
const roMapIdx = rest.indexOf('--ro-map');
const roMap = roMapIdx >= 0 ? rest[roMapIdx + 1] : null;
const fixture = path.join('tests/fixtures/scripts', `${name}.js`);
const nodeBin = `${process.env.HOME}/.local/share/mise/installs/node/${nodeVer}/bin/node`;
const jsc = `/tmp/script-${name}-${nodeVer}.jsc`;

const run = (cmd, args, input) => {
  try {
    const out = execFileSync(cmd, args, { encoding: 'utf8', input, stdio: ['pipe', 'pipe', 'pipe'], timeout: 8000 });
    return { out, code: 0 };
  } catch (e) {
    return { out: (e.stdout || '') + (e.stderr || ''), code: e.status === undefined ? -1 : e.status };
  }
};

const result = { fixture: name, node: nodeVer, kind: 'script' };

// 1) 原脚本（node 直接跑）
const orig = run(nodeBin, [fixture]);
if (orig.code !== 0 && !orig.out) {
  console.log(JSON.stringify({ ...result, status: 'compile-fail', detail: 'original run failed' }));
  process.exit(0);
}

// 2) 编译成 jsc
const comp = run(nodeBin, ['scripts/mkcorpus.js', fixture, jsc]);
if (comp.code !== 0) {
  console.log(JSON.stringify({ ...result, status: 'compile-fail', detail: comp.out.trim().split('\n')[0] }));
  process.exit(0);
}

// 3) 反编译（--runtime：要能跑）
const decArgs = ['decompile', jsc, '--runtime'];
if (roMap) decArgs.push('--ro-map', roMap);
let decoded = '';
try {
  decoded = execFileSync('./target/release/jscd', decArgs, { encoding: 'utf8' });
} catch (e) {
  console.log(JSON.stringify({ ...result, status: 'decompile-failed', detail: (e.stderr || '').trim().split('\n')[0] }));
  process.exit(0);
}
const decFile = `/tmp/script-${name}-${nodeVer}.dec.js`;
fs.writeFileSync(decFile, decoded);

// 4) 跑反编译产物
const mine = run(nodeBin, [decFile]);
const norm = (s) => s.replace(/\s+$/gm, '').trim();
result.matched = norm(orig.out) === norm(mine.out) && orig.code === mine.code;
if (result.matched) {
  result.status = 'pass';
} else {
  result.status = 'fail';
  result.diffs = [{ origOut: norm(orig.out).slice(0, 400), mineOut: norm(mine.out).slice(0, 400), origCode: orig.code, mineCode: mine.code }];
}
console.log(JSON.stringify(result));