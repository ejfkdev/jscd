// 源码级质量度量：对比"原始明文"与"反编译产物"的结构相似度。
// 规范化：标识符 → ID（变量名物理上不可恢复）；保留关键字/运算符/字面量（字符串与数字可恢复）。
// 指标：token 召回率（原始结构被还原的比例）、Dice 相似度、未还原占位计数。
// 用法: node scripts/quality_diff.js <fixture> <node-version> [--ro-map PATH] [--dir DIR]
const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');

const [name, nodeVer, ...rest] = process.argv.slice(2);
const dirIdx = rest.indexOf('--dir');
const dir = dirIdx >= 0 ? rest[dirIdx + 1] : 'tests/fixtures/behav';
const mapIdx = rest.indexOf('--ro-map');
const roMap = mapIdx >= 0 ? rest[mapIdx + 1] : null;

const KEYWORDS = new Set(['if','else','for','while','do','return','function','switch','case',
  'default','break','continue','try','catch','finally','throw','new','typeof','delete',
  'instanceof','in','of','var','let','const','class','extends','super','this','yield','await',
  'async','null','undefined','true','false','static','get','set']);

function tokenize(src) {
  const out = [];
  const re = /\/\*[\s\S]*?\*\/|\/\/[^\n]*|"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|`(?:\\.|[^`\\])*`|0[xX][0-9a-fA-F]+|\d+\.?\d*(?:[eE][+-]?\d+)?|[A-Za-z_$][A-Za-z0-9_$]*|===|!==|==|!=|<=|>=|=>|\*\*|\+\+|--|&&|\|\||\?\?|\.\.\.|[{}()\[\];,.<>+\-*/%&|^!~?:=]/g;
  let m;
  while ((m = re.exec(src))) {
    const t = m[0];
    if (t.startsWith('/*') || t.startsWith('//')) continue;           // 去注释
    if (/^["'`]/.test(t)) { out.push('STR'); continue; }               // 字符串字面量（可恢复，但模板串内容会变）
    if (/^[0-9]/.test(t)) { out.push('NUM'); continue; }
    if (/^[A-Za-z_$]/.test(t)) {
      out.push(KEYWORDS.has(t) ? t : 'ID');                           // 关键字保留，标识符归一
      continue;
    }
    out.push(t);
  }
  return out;
}

function lcs(a, b) {
  // 滚动数组 O(min) 空间
  const n = a.length, m = b.length;
  if (!n || !m) return 0;
  let prev = new Uint32Array(m + 1), cur = new Uint32Array(m + 1);
  for (let i = 1; i <= n; i++) {
    for (let j = 1; j <= m; j++) {
      cur[j] = a[i - 1] === b[j - 1] ? prev[j - 1] + 1 : Math.max(prev[j], cur[j - 1]);
    }
    [prev, cur] = [cur, prev];
    cur.fill(0);
  }
  return prev[m];
}

const src = fs.readFileSync(path.join(dir, `${name}.js`), 'utf8');
const jsc = `/tmp/qa-${name}-${nodeVer}.jsc`;
const nodeBin = `${process.env.HOME}/.local/share/mise/installs/node/${nodeVer}/bin/node`;
const run = (cmd, args) => execFileSync(cmd, args, { encoding: 'utf8', stdio: ['ignore','pipe','pipe'] });

run(nodeBin, ['scripts/mkcorpus.js', path.join(dir, `${name}.js`), jsc]);
const decArgs = ['decompile', jsc];
if (roMap) decArgs.push('--ro-map', roMap);
let decoded;
try { decoded = run('./target/release/jscd', decArgs); }
catch (e) { console.log(JSON.stringify({ fixture: name, node: nodeVer, status: 'decompile-failed',
  detail: (e.stderr||String(e)).trim().split('\n')[0] })); process.exit(0); }

const A = tokenize(src), B = tokenize(decoded);
const L = lcs(A, B);
const recall = A.length ? L / A.length : 0;
const dice = (A.length + B.length) ? (2 * L) / (A.length + B.length) : 0;
const placeholders = {
  unknown: (decoded.match(/__unknown_\w+/g) || []).length,
  constref: (decoded.match(/__const_\d+/g) || []).length,
  ro: (decoded.match(/<ro\d+_\d+>/g) || []).length,
  todo: (decoded.match(/TODO \w+/g) || []).length,
  hole: (decoded.match(/undefined \/\* hole \*\//g) || []).length,
  runtime: (decoded.match(/__runtime_\w+|__intrinsic_\w+/g) || []).length,
};
console.log(JSON.stringify({
  fixture: name, node: nodeVer, status: 'ok',
  origTokens: A.length, decTokens: B.length,
  recall: +recall.toFixed(3), dice: +dice.toFixed(3),
  placeholders,
}));
