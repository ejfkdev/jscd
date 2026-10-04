// try/catch 之后还有代码（V8 9.x 的普通 try/catch 不发完成码、函数里只有这一个 handler）——
// catch 体的右端曾退化成"函数末尾"，把 catch 之后的语句整段吞进 catch：
// `try { out = x } catch { out = -1 } return out;` 的成功路径返回 undefined。
function maybeThrow(v) {
  if (v < 0) throw new Error("neg");
  return v;
}
function tcAssign(x) {
  let out = 0;
  try {
    out = maybeThrow(x);
  } catch (e) {
    out = -1;
  }
  return out;
}
function tcAfter(x) {
  let out = 0;
  try {
    out = maybeThrow(x);
  } catch (e) {
    out = -1;
  }
  if (out > 3) return "big";
  return out;
}
function tcVoid(x) {
  let out = 0;
  try {
    out = maybeThrow(x);
  } catch (e) {
    out = -2;
  }
}
function target(x) {
  return [tcAssign(x), tcAfter(x), tcVoid(x)].join("|");
}