// **finally 之前算出的值要在 finally 之后用**（try/catch/finally 三 handler 形态）：
// 完成码分派里那条"正常完成"路径必须落回 try/catch 之后的代码，值靠寄存器活着。
// 曾经整个函数体丢空（catch 体右端退化成函数尾 + 线性死存储误删 try 里的赋值）。
function bumpInFinally(x) {
  let out = 0;
  try {
    out = x;
  } catch (e) {
    out = -1;
  } finally {
    x = x + 7;
  }
  return out + ":" + x;
}
function target(x) {
  return "v" + bumpInFinally(x);
}
