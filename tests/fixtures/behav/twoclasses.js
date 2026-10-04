class A {
  constructor(n) { this.n = n; }
  bump() { return "A:" + this.n; }
  static tag() { return "A.tag"; }
}
class B {
  constructor(n) { this.n = n; }
  bump() { return "B:" + this.n; }
  static tag() { return "B.tag"; }
}
function target(n) {
  const a = new A(n);
  const b = new B(n + 1);
  return (
    a.bump() + "|" + b.bump() + "|" + A.tag() + "|" + B.tag() +
    "|" + makeC1(n) + "|" + makeC2(n + 1)
  );
}

// 同名类（不同作用域）→ 构造器 SFI 同名，去重后第二个是 `var C_2 = class`（不提升）
function makeC1(n) {
  class C { constructor(v) { this.v = v; } get() { return "C1:" + this.v; } }
  return new C(n).get();
}
function makeC2(n) {
  class C { constructor(v) { this.v = v; } get() { return "C2:" + this.v; } }
  return new C(n).get();
}
