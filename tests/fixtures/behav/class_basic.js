class Counter {
  constructor(start) {
    this.n = start;
  }
  bump(by) {
    this.n += by;
    return this;
  }
  get value() {
    return this.n;
  }
  static zero() {
    return new Counter(0);
  }
}
function target(a) {
  const c = new Counter(a);
  const d = Counter.zero();
  d.bump(1).bump(2);
  return c.bump(3).value + d.value;
}
