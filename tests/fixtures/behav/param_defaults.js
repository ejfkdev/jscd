// 参数默认值：**求值时机**（调用时，不是体开始执行时）、链式默认值、`.length`、
// 以及"抛错的默认值要同步抛"（生成器也一样：形参绑定在 [[Call]] 里）。
// 曾经：默认值被内联进函数体（生成器里延迟到第一次 next()）、`c = a + b` 折不出来、
// `.length` 多算一个、6.x 的形参拷贝槽被按脚本作用域解名（覆盖脚本级变量）。
let log = "";
function d(tag, v) {
  log += tag;
  return v;
}
function sync(a, b = d("b", 2), c = d("c", a + b)) {
  return a + b + c;
}
function* gen(a, b = d("g", 3)) {
  yield a + b;
}
function nolength(x = 1, y = 2) {
  return x + y;
}
function boom(a = (function () {
  throw new Error("sdflt");
})()) {
  return a;
}
function* gboom(a = (function () {
  throw new Error("gdflt");
})()) {
  yield a;
}
function target(x) {
  log = "";
  const out = [];
  out.push("len=" + [sync.length, gen.length, nolength.length].join("/"));
  out.push("sync=" + sync(x) + "|" + log);
  log = "";
  out.push("gen=" + [...gen(x)].join(",") + "|" + log);
  try {
    boom(undefined);
    out.push("boom=noThrow");
  } catch (e) {
    out.push("boom=threw:" + e.message);
  }
  try {
    gboom(undefined);
    out.push("gboom=noThrow");
  } catch (e) {
    out.push("gboom=threw:" + e.message);
  }
  return out.join(" ; ");
}
