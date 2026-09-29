function* g1() { yield; }
function* g2(a) { let x = yield a; yield x; }
function* g3(xs) { for (const v of xs) { yield v; } }
function* g4(xs) { yield* xs; }
function* g5(xs) { yield* xs; yield 1; }
