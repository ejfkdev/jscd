class Greeter {
  constructor(name) {
    this.name = name;
  }
  hello() {
    return "hi " + this.name;
  }
  static twice(n) {
    return n * 2;
  }
}
function run() {
  const g = new Greeter("ada");
  return g.hello() + "/" + Greeter.twice(21);
}
console.log("class", run());
