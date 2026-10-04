// async 函数体内的用户 try/catch（12.x+ 现代形态）：
// V8 在用户 catch 之外再加一层"隐式 reject 包装器"handler，它的 range 覆盖
// try 体 + 用户 catch 体（catch 体自己抛也走 reject）——反编译要消费掉包装器、
// 只重建用户那一层。包装器的判据：target 块里调 AsyncFunctionReject/RejectPromise
// （用户代码的 throw 走 `Throw` 字节码，不会调这两个内建）。
async function withTry(p) {
  try {
    const v = await p;
    return v;
  } catch (e) {
    return "caught:" + e.message;
  }
}
async function withRethrow(p) {
  try {
    return await p;
  } catch (e) {
    throw e;
  }
}
// catch 体里再 await 一次：包装器与 await 挂起点叠加
async function retry(p, q) {
  try {
    return await p;
  } catch (e) {
    return "retry:" + (await q);
  }
}
function target(n) {
  return Promise.all([
    withTry(Promise.resolve("v" + n)),
    withTry(Promise.reject(new Error("bad" + n))),
    withRethrow(Promise.resolve("r" + n)),
    withRethrow(Promise.reject(new Error("rej" + n))).catch(function (e) { return "re:" + e.message; }),
    retry(Promise.resolve("ok" + n), Promise.resolve("q" + n)),
    retry(Promise.reject(new Error("e" + n)), Promise.resolve("q" + n)),
  ]).then(function (vs) {
    return vs.join("|");
  });
}