// 数字字面量的常量池解码：双精度元素数组（FixedDoubleArray 的原始 f64）与 BigInt。
// 曾经：`[1.5, -0]` 被读成高半截断的整数（1.5 → 1073217536、-0 → -2147483648）、
// `[10n]` 变成 `undefined`。
function show(v) {
  if (typeof v === "bigint") return String(v) + "n";
  if (Object.is(v, -0)) return "-0";
  return String(v);
}
function target(which) {
  const doubles = [1.5, -0, 2.25, 1e21, 0.1];
  const smis = [1, 2, 3];
  const mixed = [1.5, 2];
  const bigints = [10n, 9007199254740993n, -7n, 0n, 123456789012345678901234567890n];
  if (which === 0) return doubles.map(show).join(",");
  if (which === 1) return smis.map(show).join(",");
  if (which === 2) return mixed.map(show).join(",");
  if (which === 3) {
    let out = "";
    new Set([-0]).forEach((v) => {
      out += show(v);
    });
    return out;
  }
  return bigints.map(show).join(",");
}