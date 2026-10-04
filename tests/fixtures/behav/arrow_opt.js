// 箭头函数/默认参数/访问器（ES6，九版本解析器都支持）
function target(a, b = 2) {
  const add = (x, y) => x + y;
  const nest = (x) => (y) => x * y;
  const obj = { v: a, get raw() { return this.v; } };
  return add(a, b) + nest(2)(3) + obj.raw;
}
