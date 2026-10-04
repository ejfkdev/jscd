// for-of 的收尾协议：**正常结束（迭代器耗尽）不应调 `iterator.return()`**；
// `break` / 体里抛错才该调（各一次）。
//
// 注意：**"末元素触发 continue 后耗尽"这条边是 V8 自身的版本差异** ——
//   6.2/6.8：`for (const v of [1,1,2]) { if (v===2) continue; }` 耗尽时**会**调一次 return()；
//   7.8+  ：不调。
// 产物必须**忠实复现各自版本**的行为（实测：同版本对拍逐字相等）。
// 所以本用例在 cases.json 里标了 `same_node_original`：原函数用**目标版本**的 node 跑
// （宿主 V8 的语义可能与被编译的版本不同，直接比会得到假差异）。
function target(x) {
  let calls = 0;
  const orig = Array.prototype[Symbol.iterator];
  Array.prototype[Symbol.iterator] = function () {
    const it = orig.call(this);
    const real = it.return;
    it.return = function () {
      calls += 1;
      return real ? real.call(it) : { value: undefined, done: true };
    };
    return it;
  };
  let sum = 0;
  try {
    for (const v of [x, 1, 2]) {
      if (v === 2) continue;
      sum += v;
    }
  } finally {
    Array.prototype[Symbol.iterator] = orig;
  }
  return sum + "/" + calls;
}
