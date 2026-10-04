async function noAwait(x) {
  return x + 1;
}
async function withAwait(p) {
  const v = await p;
  return v * 2;
}
async function multi(p, q) {
  const a = await p;
  const b = await q;
  return a + b;
}
async function loopAwait(n) {
  let s = 0;
  for (let i = 0; i < 3; i++) {
    s += await Promise.resolve(i * n);
  }
  return s + n;
}
async function rejectWith(p) {
  const v = await p;
  if (v < 0) throw new RangeError("negative");
  return "ok:" + v;
}
function target(n) {
  return Promise.all([
    noAwait(n),
    withAwait(Promise.resolve(n)),
    multi(Promise.resolve(n), Promise.resolve(n + 1)),
    loopAwait(n),
    rejectWith(Promise.resolve(n)).catch(function (e) { return "caught:" + e.message; }),
    rejectWith(Promise.resolve(-n)).catch(function (e) { return "caught:" + e.message; }),
  ]).then(function (vs) {
    return vs.join("|");
  });
}
