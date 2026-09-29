function* count(n) {
  for (let i = 0; i < n; i++) {
    yield i * 2;
  }
  return "done";
}
function* pair(a, b) {
  yield* count(a);
  yield b;
}
function target(n) {
  const out = [];
  const it = pair(n, 99);
  for (const v of it) {
    out.push(v);
  }
  return out.join("-");
}
