// `TestTypeOf` 的 flag 枚举**随版本差一位**（BigInt 支持 V8 6.7 往枚举里插了 kIsBigInt，
// undefined/function/object 从 4/5/6 挪到 5/6/7）。认错位会把"是不是函数"判成
// "是不是 undefined"（8.17 的 for-of 收尾因此对着活函数抛假 TypeError）。
function pick(x) {
  if (x === 0) return undefined;
  if (x === 1) return 1;
  if (x === 2) return "s";
  if (x === 3) return true;
  if (x === 4) return Symbol("q");
  if (x === 5) return function () {};
  if (x === 6) return {};
  if (x === 7) return [];
  return /re/;
}
function target(x) {
  const v = pick(x);
  return [
    typeof v === "number",
    typeof v === "string",
    typeof v === "symbol",
    typeof v === "boolean",
    typeof v === "bigint",
    typeof v === "undefined",
    typeof v === "function",
    typeof v === "object",
    Array.isArray(v),
    v instanceof RegExp,
  ].join(",");
}
