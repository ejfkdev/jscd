function* gen(n) {
  const a = yield n;
  const b = yield a + 1;
  const c = yield b * 2;
  return a + b + c;
}
function target(n) {
  const it = gen(n);
  const r1 = it.next();
  const r2 = it.next(10);
  const r3 = it.next(20);
  const r4 = it.next(30);
  return [r1.value, r2.value, r3.value, r4.value, r4.done].join(",");
}
