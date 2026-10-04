// for-of 的 IteratorClose 收尾：**循环体抛出的异常必须传播**（曾经被吞掉、返回残值）。
// V8 把收尾编译成"单 handler + 完成码"：正解不是去渲染那套机器重抛，而是把整条 try/finally
// 结构认出来 —— JS 的 try/finally 天然会把异常传下去，机器分派整段丢弃。
// 这条 fixture 同时钉住"顺序 try/catch 的续接点"（`target` 里两个 try：第一个 catch 的右端
// 必须截在第二个 try 之前，否则 catch 里的 `out.push("E1")` 会漏到 try 外面无条件执行）。
function sumAll(arr) {
  let s = 0;
  for (const v of arr) s += v;
  return s;
}
function throwsInBody(arr) {
  let s = 0;
  for (const v of arr) {
    s += v;
    if (v === 2) throw new Error("boom");
  }
  return s;
}
function target(x) {
  const out = [];
  try {
    out.push(sumAll(x));
  } catch (e) {
    out.push("E1");
  }
  try {
    out.push(throwsInBody(x));
  } catch (e) {
    out.push("E2:" + e.message);
  }
  return out.join("|");
}