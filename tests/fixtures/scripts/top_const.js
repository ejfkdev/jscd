// 顶层只有 const/let → 也走不到 DeclareGlobals
const x = 40 + 2;
let y = x / 2;
console.log("const", x, y);
