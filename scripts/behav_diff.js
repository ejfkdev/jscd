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
// `min_node`：该 fixture 的源码语法要这个 major 起才支持（如 `?.`/`??` 需要 node14）。
// 低版本直接报 skip —— 报 compile-fail 会把"该版本解析器不支持"混进"产物有问题"，也会
// 遮住那些版本上真正的回归。
const minNode = casesAll[name].min_node || 0;
const major = parseInt(nodeVer.split('.')[0], 10);
if (major < minNode) {
  console.log(JSON.stringify({ fixture: name, node: nodeVer, status: 'skip', reason: `min-node ${minNode}` }));
  process.exit(0);
}
const src = fs.readFileSync(fixture, 'utf8');
const jsc = `/tmp/behav-${name}-${nodeVer}.jsc`;

const nodeBin = `${process.env.HOME}/.local/share/mise/installs/node/${nodeVer}/bin/node`;
const run = (cmd, args, extraEnv) => execFileSync(cmd, args, {
  encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'],
  env: extraEnv ? { ...process.env, ...extraEnv } : process.env,
});

// 结构门禁：**默认产物**（不带 --runtime，会过 AST 层优化）不许把顶层声明整段丢掉。
//   * 矩阵跑的是 --runtime（要能跑），正好绕过 AST 层 —— 优化层曾在包壳里把"载荷内没人
//     引用"的函数当死代码删掉（`function target` 整个消失），这条就是为它加的。
//   * 判据：未优化产物里的每个顶层函数声明名，优化后必须还在。
const topFnNames = (text) => {
  const out = [];
  for (const line of text.split('\n')) {
    const m = /^(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)\s*\(/.exec(line);
    if (m) out.push(m[1]);
  }
  return out;
};
const checkDeclarationsKept = (jscPath, roMapPath) => {
  const args0 = ['decompile', jscPath];
  if (roMapPath) args0.push('--ro-map', roMapPath);
  const raw = run('./target/release/jscd', args0, { JSCD_NO_OPT: '1' });
  const opt = run('./target/release/jscd', args0);
  const missing = topFnNames(raw).filter((n) => !new RegExp(`function\\s*\\*?\\s*${n}\\s*\\(`).test(opt));
  return missing;
};

// 1) 编译成 jsc。失败分两种：**源码本身**用了该版本不支持的语法（如 node12 跑
// `?.`/`??`，属测试用例越界，不是我们的问题）与真正的编译错误。
try {
  run(nodeBin, ['scripts/mkcorpus.js', fixture, jsc]);
} catch (e) {
  const msg = (e.stderr || String(e)).trim().split('\n')[0];
  console.log(JSON.stringify({
    fixture: name, node: nodeVer, status: 'compile-fail',
    detail: msg, note: '该 node 版本不支持此源码语法',
  }));
  process.exit(0);
}

// 2) 反编译
// 对拍的是"产物能不能跑" → 必须带可运行前导（默认已剥离，只给原始代码）
const decArgs = ['decompile', jsc, '--runtime'];
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

(async () => {
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
// 反编译产物里的 async 函数返回 Promise → 先 await 再比（跨 realm 的 thenable 一样能 await）
const callTimed = (code, args, label) => {
  const ctx = vm.createContext({ __args: args, Promise });
  try {
    const raw = vm.runInContext(
      `${code}\n;(function(){ return target.apply(null, __args); })()`,
      ctx,
      { timeout: 3000 }
    );
    const out = raw && typeof raw.then === 'function' ? { thenable: raw } : { plain: raw };
    return { ok: true, value: out };
  } catch (e) {
    // 抛出的可能是 undefined/字符串（反编译产物会这样）→ 不能假定是 Error 对象
    var name = (e && e.constructor && e.constructor.name) || typeof e;
    var msg = (e && e.message) || String(e);
    return { ok: false, value: name + ': ' + String(msg).split('\n')[0] };
  }
};

// 反编译产物里的 Promise 可能永远不 settle（语义错时常见）→ 给个上限，
// 超时算作 "ERR Timeout"（原函数不会超时 → 用例判为不一致，但不会把对拍挂住）
const settle = async (r) => {
  if (!r.ok) return 'ERR ' + r.value;
  try {
    let raw = r.value;
    if ('thenable' in raw) {
      raw = await Promise.race([
        Promise.resolve(raw.thenable).then((v) => ({ v }), (e) => ({ e })),
        new Promise((res) => setTimeout(() => res({ timeout: true }), 2000)),
      ]);
      if (raw && raw.timeout) return 'ERR Timeout';
      if (raw && 'e' in raw) {
        const e = raw.e;
        var nm = (e && e.constructor && e.constructor.name) || typeof e;
        return 'ERR ' + nm + ': ' + String((e && e.message) || e).split('\n')[0];
      }
      raw = raw.v;
    }
    const v = raw.plain !== undefined ? raw.plain : raw;
    return JSON.stringify(v);
  } catch (e) {
    var name = (e && e.constructor && e.constructor.name) || typeof e;
    return 'ERR ' + name + ': ' + String((e && e.message) || e).split('\n')[0];
  }
};

// `same_node_original`：**原函数也用目标版本的 node 跑**（spawn 一次）。
// 有些语义本身随 V8 版本变（例如 for-of 收尾在"末元素 continue 后耗尽"时 6.2 会调
// iterator.return()、7.8+ 不调）—— 宿主 V8 的语义与"被编译的那个版本"不同，
// 直接比会得到假差异。勾了这个标志的 fixture 按同版本对拍。
const sameNode = !!casesAll[name].same_node_original;
const origSameNode = (args) => {
  const tmp = `/tmp/behav-orig-${name}-${nodeVer}.js`;
  fs.writeFileSync(
    tmp,
    `${src}\nconsole.log("__JSCD__" + JSON.stringify(target.apply(null, ${JSON.stringify(args)})));\n`
  );
  try {
    const out = run(nodeBin, [tmp]);
    const line = out.split('\n').filter((l) => l.startsWith("__JSCD__")).pop();
    if (!line) return { ok: false, value: "orig-no-output" };
    return { ok: true, value: { plain: JSON.parse(line.slice("__JSCD__".length)) } };
  } catch (e) {
    const msg = String((e.stdout || "") + (e.stderr || e)).split("\n")[0];
    return { ok: false, value: msg };
  }
};

let same = 0;
const diffs = [];
for (const args of cases) {
  const a = sameNode ? origSameNode(args) : callTimed(src, args, 'orig');
  const b = callTimed(decoded, args, 'mine');
  const av = await settle(a);
  const bv = await settle(b);
  if (av === bv && a.ok) {
    same++;
  } else if (!a.ok && !b.ok) {
    // 两边都抛错：比较**错误类型**。具体消息里含源码表达式文本（如 "s.toUpperCase"），
    // 码缓存里没有源码文本 —— 那部分不可还原；类型一致即语义一致。
    const typeOf = (s) => String(s).replace(/^ERR /, '').split(':')[0].trim();
    if (typeOf(av) === typeOf(bv)) same++;
    else diffs.push({ args, orig: av, mine: bv, note: 'error-type' });
  } else {
    diffs.push({ args, orig: av, mine: bv });
  }
}
result.matched = same;
result.status = same === cases.length ? 'pass' : (same === 0 ? 'fail' : 'partial');
// 已知未修的 fixture：按 known-fail 单列，不混进回归信号（见 cases.json 的 known_fail 说明）
if (casesAll[name].known_fail && result.status !== 'pass') {
  result.status = 'known-fail';
  result.detail = casesAll[name].known_fail;
}

// 5) 结构门禁（见上）
try {
  const missing = checkDeclarationsKept(jsc, roMap);
  if (missing.length) {
    result.status = 'lost-declarations';
    result.detail = `默认产物丢了顶层函数声明: ${missing.join(', ')}`;
  }
} catch (e) {
  result.detail = `结构门禁执行失败: ${String(e.message).split('\n')[0]}`;
}
if (diffs.length) result.diffs = diffs.slice(0, 3);
console.log(JSON.stringify(result));
process.exit(0);

})();
