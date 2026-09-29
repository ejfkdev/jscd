// 行为对拍：把 fixture 编译成 jsc → jscd 反编译 → 运行反编译结果 → 与原函数逐用例对比。
// 用法: node scripts/behav_diff.js <fixture-name> <node-version> [--ro-map PATH] [--json]
const fs = require('fs');
const path = require('path');
const vm = require('vm');
const { execFileSync } = require('child_process');

const [name, nodeVer, ...rest] = process.argv.slice(2);
const roMapIdx = rest.indexOf('--ro-map');
const roMap = roMapIdx >= 0 ? rest[roMapIdx + 1] : null;
const fixture = path.join('tests/fixtures/behav', `${name}.js`);
const casesAll = JSON.parse(fs.readFileSync(path.join('tests/fixtures/behav/cases.json'), 'utf8'));
const cases = casesAll[name].cases;
const src = fs.readFileSync(fixture, 'utf8');
const jsc = `/tmp/behav-${name}-${nodeVer}.jsc`;

const nodeBin = `${process.env.HOME}/.local/share/mise/installs/node/${nodeVer}/bin/node`;
const run = (cmd, args) => execFileSync(cmd, args, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });

// 1) 编译成 jsc
run(nodeBin, ['scripts/mkcorpus.js', fixture, jsc]);

// 2) 反编译
const decArgs = ['decompile', jsc];
if (roMap) decArgs.push('--ro-map', roMap);
let decoded = '';
let decompileErr = null;
try {
  decoded = run('./target/release/jscd', decArgs);
} catch (e) {
  decompileErr = (e.stderr || String(e)).trim().split('\n')[0];
}

const result = { fixture: name, node: nodeVer, cases: cases.length };

if (decompileErr) {
  result.status = 'decompile-failed';
  result.detail = decompileErr;
  console.log(JSON.stringify(result));
  process.exit(0);
}

// 3) 语法校验
try {
  new vm.Script(decoded);
  result.syntax = 'ok';
} catch (e) {
  result.syntax = 'error';
  result.detail = String(e.message).split('\n')[0];
  result.status = 'syntax-error';
  console.log(JSON.stringify(result));
  process.exit(0);
}

// 4) 取原函数与反编译函数的引用并逐用例对比
const evalTarget = (code) => {
  const ctx = vm.createContext({});
  vm.runInContext(code, ctx, { timeout: 4000 });
  return ctx.target;
};

let orig, mine;
try {
  orig = evalTarget(src);
} catch (e) {
  result.status = 'harness-error';
  result.detail = 'original eval: ' + e.message;
  console.log(JSON.stringify(result));
  process.exit(0);
}
try {
  mine = evalTarget(decoded);
} catch (e) {
  result.status = 'eval-failed';
  result.detail = String(e.message).split('\n')[0];
  console.log(JSON.stringify(result));
  process.exit(0);
}
if (typeof mine !== 'function') {
  result.status = 'target-missing';
  console.log(JSON.stringify(result));
  process.exit(0);
}

const norm = (v) => JSON.stringify(v);
// 每个用例在独立上下文里执行并带超时（防止反编译产物里的死循环挂住对拍）
const callTimed = (code, args, label) => {
  const ctx = vm.createContext({ __args: args });
  try {
    const out = vm.runInContext(
      `${code}\n;JSON.stringify((function(){ return target.apply(null, __args); })())`,
      ctx,
      { timeout: 3000 }
    );
    return { ok: true, value: out };
  } catch (e) {
    return { ok: false, value: e.constructor.name + ': ' + String(e.message).split('\n')[0] };
  }
};

let same = 0;
const diffs = [];
for (const args of cases) {
  const a = callTimed(src, args, 'orig');
  const b = callTimed(decoded, args, 'mine');
  const av = a.ok ? a.value : 'ERR ' + a.value;
  const bv = b.ok ? b.value : 'ERR ' + b.value;
  if (av === bv && a.ok) same++;
  else diffs.push({ args, orig: av, mine: bv });
}
result.matched = same;
result.status = same === cases.length ? 'pass' : (same === 0 ? 'fail' : 'partial');
if (diffs.length) result.diffs = diffs.slice(0, 3);
console.log(JSON.stringify(result));
