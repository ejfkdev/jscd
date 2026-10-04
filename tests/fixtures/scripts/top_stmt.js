// 顶层只有语句、没有任何声明 → 没有 DeclareGlobals（曾整个脚本被包成没人调用的 _anon_0）
console.log("stmt", 1 + 1);
console.log("stmt2", [1.5, -0].length);
