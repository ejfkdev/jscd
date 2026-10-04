// `delete` 的结果是布尔值（旧实现把它当语句发掉、读处拿到 undefined）
function target(x) {
  const o = { a: x, b: 2 };
  const r = delete o.a;
  const s = delete o.b;
  const n = Object.create(null);
  n.k = x;
  const t = delete n.k;
  const arr = [x, x + 1];
  const u = delete arr[0];
  return [r, s, t, u, Object.keys(o).length, o.a === undefined, arr.length].join(",");
}
