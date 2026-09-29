// 语料生成器：与 bytenode 完全同参地编译 JS → .jsc。
// bytenode 的关键动作：--no-lazy + --no-flush-bytecode（全部函数即时编译、字节码不被 GC 丢弃），
// 然后 vm.Script(code, {produceCachedData}) / script.createCachedData()。
// 用法: node mkcorpus.js <input.js> <output.jsc> [--brotli] [--module]
const fs = require('fs');
const vm = require('vm');
const v8 = require('v8');

const [input, output, ...flags] = process.argv.slice(2);
if (!input || !output) {
  console.error('usage: node mkcorpus.js <input.js> <output.jsc> [--brotli] [--module]');
  process.exit(2);
}

v8.setFlagsFromString('--no-lazy');
if (Number(process.versions.node.split('.')[0]) >= 12) {
  v8.setFlagsFromString('--no-flush-bytecode');
}

let code = fs.readFileSync(input, 'utf8');
// 与 bytenode compileFile 一致：剥离 shebang
if (code.startsWith('#')) {
  const nl = code.indexOf('\n');
  code = nl === -1 ? '' : code.slice(nl + 1);
}
const asModule = flags.includes('--module');
if (asModule) {
  code = require('module').wrap(code);
}

const script = new vm.Script(code, { produceCachedData: true });
let cache = script.cachedData;
if (!cache || cache.length === 0) {
  cache = script.createCachedData();
}
let out = Buffer.from(cache);
if (flags.includes('--brotli')) {
  out = require('zlib').brotliCompressSync(out);
}
fs.writeFileSync(output, out);
console.log(`${process.versions.node} v8=${process.versions.v8} -> ${output} (${out.length} bytes)`);