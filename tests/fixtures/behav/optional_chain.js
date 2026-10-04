// 可选链 / 空值合并（ES2020，node14+ 的解析器才支持 → cases.json 里标 min_node）
function target(a) {
  const obj = a === 0 ? null : { v: a, get raw() { return this.v; } };
  const val = obj?.raw ?? -1;
  const missing = obj?.nope?.deep;
  const call = obj?.raw?.toString?.() ?? "none";
  const keyed = obj?.["v"] ?? "k";
  return val + "|" + (missing === undefined ? 1 : 0) + "|" + call + "|" + keyed;
}
