// jscd 语料特征矩阵：覆盖 bytenode 目标常见语法与运行时构造。
// 本文件会被 mkcorpus.js 编译成 .jsc（--no-lazy 全量编译），并由 golden 脚本
// 与 `node --print-bytecode` 对拍。保持纯函数式，避免依赖外部状态。

// —— 函数形态 ——
function add(a, b) { return a + b; }
const mul = (a, b) => a * b;
const defaultParam = (a = 1, b = 2) => a + b;
const restParams = (...xs) => xs.length;
const destructured = ({ a, b: { c } = {} } = {}, [d, e] = [4, 5]) => a + c + d + e;

// —— 控制流 ——
function control(n) {
  let acc = 0;
  for (let i = 0; i < n; i++) {
    if (i % 2 === 0) acc += i;
    else if (i % 3 === 0) acc -= i;
    else acc ^= i;
  }
  let j = 0;
  while (j < n) { acc += j++; }
  do { acc--; } while (acc > 100);
  switch (n) {
    case 0: return 'zero';
    case 1: case 2: return 'small';
    default: return acc > 0 ? 'pos' : 'neg';
  }
}

// —— 异常 ——
function throwing(x) {
  try {
    if (x < 0) throw new RangeError('negative');
    return Math.sqrt(x);
  } catch (e) {
    return e instanceof RangeError ? -1 : -2;
  } finally {
    globalThis.__jscd_marker = (globalThis.__jscd_marker || 0) + 1;
  }
}

// —— 类与继承 ——
class Base {
  #secret = 42;
  constructor(name) { this.name = name; }
  get label() { return `Base(${this.name})`; }
  static create(n) { return new Base(n); }
  reveal() { return this.#secret; }
}
class Derived extends Base {
  constructor(name, extra) { super(name); this.extra = extra; }
  get label() { return super.label + '+'; }
  *items() { yield this.name; yield this.extra; }
}

// —— async / 生成器 ——
async function asyncFlow(v) {
  const a = await Promise.resolve(v);
  const b = await Promise.resolve(a + 1);
  return a + b;
}
function* gen(n) { for (let i = 0; i < n; i++) yield i * i; }
async function* agen(n) { for (let i = 0; i < n; i++) yield await Promise.resolve(i); }

// —— 字面量/模板/常量池 ——
const strings = ['alpha', 'beta', 'gamma'];
const obj = { a: 1, 'b-c': 2, [Symbol.iterator]: null };
const nested = { deep: { deeper: { value: 3.5 } } };
const bigIntValue = 123456789012345678901234567890n;
const re = /^jscd[-_](\d+)$/gi;
function template(name, count) { return `${name}:${count}${strings.join('|')}`; }

// —— 闭包与上下文 ——
function counterFactory(start) {
  let n = start;
  return { inc() { return ++n; }, get value() { return n; } };
}
const outer = 7;
const closure = (x) => (y) => x + y + outer;

// —— 解构/展开/可选链 ——
function modern(o) {
  const { a = 1, ...rest } = o || {};
  const arr = [...strings, ...(o?.list ?? [])];
  const val = o?.deep?.deeper?.value ?? 'none';
  return a + arr.length + String(val).length + Object.keys(rest).length;
}

// —— 默认导出形态（CommonJS 下由 module.wrap 包装）——
function main() {
  const c = control(5);
  const d = new Derived('x', 'y');
  return [add(1, 2), mul(3, 4), defaultParam(), restParams(1, 2, 3),
          destructured(), c, throwing(9), throwing(-1), d.label, d.reveal(),
          [...gen(3)].length, template('t', 1), closure(1)(2), modern(nested),
          bigIntValue.toString().length, re.source.length, strings.length, obj.a];
}

main();