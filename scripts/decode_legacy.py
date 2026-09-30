#!/usr/bin/env python3
"""按 V8 ≤8.4 的规则手工解码 code cache payload，输出带注解的清单。

用途：老族解析在某个对象上错位（backref 命不中 / size mismatch）时，用它把
字节流逐条摊开，和 `jscd` 的 `JSCD_DBG_OBJ=1` 轨迹对照，找出第一个不一致的对象。

规则来源（V8 8.4 源码）：
- `Source::GetInt()`：低 2 位编码长度（bytes-1），值右移 2 位 —— 不是 LEB128
- tag：kNewObject=0x00(+space 0x00..0x05)、kBackref=0x08(+space 0x08..0x0d)、
  kRootArray=0x11、kAttachedReference=0x12、kNop=0x14、kNextChunk=0x15、
  kDeferred=0x16、kAlignmentPrefix=0x17..0x19、kSynchronize=0x1a、
  kVariableRepeat=0x1b、kVariableRawData=0x1f、kRootArrayConstants=0x40..0x5f、
  kFixedRawData=0x60..0x7f（n = b-0x5f 个 tagged 单位）、kFixedRepeat=0x80..0x8f
  （count = b-0x80+2）、kHotObject=0x90..0x97
- 对象：`size_words = GetInt()`（单位 = tagged size）、内容 = map + 槽（按 ref 逐个推进，
  直到字节数用满）

用法: python3 scripts/decode_legacy.py <file.jsc> [--ts 8] [--from 0] [--max 60]
"""
import argparse
import struct
import sys

TAGS = {
    0x11: "kRootArray", 0x12: "kAttachedReference", 0x13: "kReadOnlyObjectCache",
    0x14: "kNop", 0x15: "kNextChunk", 0x16: "kDeferred", 0x17: "kAlignmentPrefix",
    0x1A: "kSynchronize", 0x1B: "kVariableRepeat", 0x1C: "kOffHeapBackingStore",
    0x1E: "kVariableRawCode", 0x1F: "kVariableRawData", 0x20: "kApiReference",
    0x21: "kExternalReference", 0x24: "kInternalReference", 0x25: "kClearedWeakReference",
    0x26: "kWeakPrefix", 0x27: "kOffHeapTarget",
}


class R:
    def __init__(self, data, ts):
        self.d = data
        self.p = 0
        self.ts = ts

    def byte(self):
        b = self.d[self.p]
        self.p += 1
        return b

    def getint(self):
        b0 = self.d[self.p]
        n = (b0 & 3) + 1
        raw = int.from_bytes(self.d[self.p:self.p + n], "little")
        self.p += n
        return raw >> 2

    def peek(self):
        return self.d[self.p] if self.p < len(self.d) else None


def tag_name(b, ts):
    if b < 0x08:
        return f"kNewObject+space{b}"
    if 0x08 <= b <= 0x0D:
        return f"kBackref+space{b - 0x08}"
    if 0x40 <= b <= 0x5F:
        return f"RootConst#{b - 0x40}"
    if 0x60 <= b <= 0x7F:
        return f"FixedRawData({b - 0x5F}u={b - 0x5F}*{ts}B)"
    if 0x80 <= b <= 0x8F:
        return f"FixedRepeat({b - 0x80 + 2})"
    if 0x90 <= b <= 0x97:
        return f"HotObject({b - 0x90})"
    return TAGS.get(b, f"?0x{b:02x}")


def decode_ref(r, depth, out, ts, budget):
    """读一个 ref，返回 (描述, 消耗字节)。递归的 kNewObject 会展开成完整对象。"""
    start = r.p
    if r.p >= len(r.d):
        return ("<eof>", 0)
    b = r.byte()
    name = tag_name(b, ts)
    if b < 0x08:  # kNewObject + space
        size_words = r.getint()
        size = size_words * ts
        out.append("  " * depth + f"@{start}: {name} size={size_words}u({size}B)")
        # 内容：map + 槽。**内联嵌套对象只占父对象 1 个槽**（它的字节虽在同一流里，
        # 但不计入父对象的 size 预算）—— 早先按子树字节计会让父对象提前"吃饱"。
        consumed = ts  # map
        while consumed < size:
            at = r.p
            desc, used = decode_ref(r, depth + 1, out, ts, budget)
            out.append("  " * depth + f"    @{at} slot: {desc}")
            # 槽数：raw 段按字节/ts 计多槽；repeat 计 n 槽；其余（对象/根/backref/热对象）1 槽
            consumed += used_slots(desc, ts)
            budget[0] -= 1
            if budget[0] <= 0:
                return (f"{name}(!)", r.p - start)
        return (f"{name} size={size}B", r.p - start)
    if 0x08 <= b <= 0x0D:
        chunk = r.getint()
        off = r.getint()
        return (f"BackRef(space={b - 8}, chunk={chunk}, off={off})", r.p - start)
    if 0x40 <= b <= 0x5F:
        return (f"Root#{b - 0x40}", r.p - start)
    if 0x60 <= b <= 0x7F:
        n = (b - 0x5F) * ts
        r.p += n
        return (f"Raw({n}B)", r.p - start)
    if 0x80 <= b <= 0x8F:
        cnt = b - 0x80 + 2
        inner, used = decode_ref(r, depth + 1, out, ts, budget)
        return (f"Repeat({cnt}x {inner})", r.p - start)
    if 0x90 <= b <= 0x97:
        return (f"Hot#{b - 0x90}", r.p - start)
    if b in (0x11, 0x12, 0x13, 0x10):
        idx = r.getint()
        nm = {0x10: "StartupObjectCache", 0x11: "RootArray", 0x12: "AttachedRef",
              0x13: "ReadOnlyObjectCache"}[b]
        return (f"{nm}[{idx}]", r.p - start)
    if b == 0x1F:
        n = r.getint()
        r.p += n
        return (f"VariableRaw({n}B)", r.p - start)
    if b == 0x1B:
        cnt = r.getint()
        inner, used = decode_ref(r, depth + 1, out, ts, budget)
        return (f"VarRepeat({cnt}x {inner})", r.p - start)
    if b in (0x17, 0x18, 0x19):
        return (f"Align{align_idx(b)}", r.p - start)
    return (name, r.p - start)


def used_slots(desc, ts):
    """一个 slot 条目占几个槽。"""
    if desc.startswith("Raw("):
        n = int(desc[4:-2])
        return max(1, n // ts)
    if desc.startswith("Repeat(") or desc.startswith("VarRepeat("):
        return int(desc.split("(")[1].split("x")[0])
    return 1


def align_idx(b):
    return b - 0x17 + 1


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("file")
    ap.add_argument("--ts", type=int, default=8)
    ap.add_argument("--from", type=int, default=0, dest="start")
    ap.add_argument("--max", type=int, default=40)
    a = ap.parse_args()

    d = open(a.file, "rb").read()
    num_res = struct.unpack_from("<I", d, 16)[0]
    res = [struct.unpack_from("<I", d, 32 + 4 * i)[0] for i in range(num_res)]
    payload_at = (32 + num_res * 4 + 7) & ~7
    payload = d[payload_at:]
    print(f"file={len(d)}B payload@{payload_at} len={len(payload)}B")
    print("reservations:", [hex(x) for x in res], "→ sizes", [x & 0x7FFFFFFF for x in res])
    print()

    r = R(payload, a.ts)
    r.p = a.start
    out = []
    budget = [a.max]
    n = 0
    while r.p < len(payload) and n < 6:
        out.append(f"--- 顶层 ref @{r.p}")
        decode_ref(r, 0, out, a.ts, budget)
        n += 1
        if budget[0] <= 0:
            break
    print("\n".join(out))


if __name__ == "__main__":
    main()