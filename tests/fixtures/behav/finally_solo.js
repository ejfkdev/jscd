// 无 catch 的 try/finally（V8 的"单 handler"完成码形态）——本轮修好的一种，
// 两种布局都要盯住：try 里带 return（完成码 1）与正常完成（完成码 -1）。
function bump(x) {
  try {
    return x + 1;
  } finally {
    x = x + 100;
  }
}
function trail(x) {
  const out = [];
  try {
    out.push("t" + x);
  } finally {
    out.push("f" + x);
  }
  return out.join(",");
}
function target(x) {
  return bump(x) + "|" + trail(x);
}
