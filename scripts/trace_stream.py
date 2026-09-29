#!/usr/bin/env python3
"""开发用流追踪器：把 .jsc 的 payload 按序列化 bytecode 解码并打印。

只做"语法层"解码（tag + varint + raw chunk），不做对象重建——用于在实现
Rust 反序列化器之前/期间快速观察真实 payload 的结构。

用法: python3 scripts/trace_stream.py file.jsc [--offset N]
"""
import argparse
import struct
import sys

TAGS = {
    0x08: "kReadOnlyHeapRef", 0x09: "kStartupObjectCache", 0x0A: "kRootArray",
    0x0B: "kAttachedReference", 0x0C: "kReadOnlyObjectCache", 0x0D: "kNop",
    0x0E: "kSynchronize", 0x0F: "kVariableRepeat", 0x10: "kOffHeapBackingStore",
    0x11: "kEmbedderFieldsData", 0x12: "kVariableRawData", 0x13: "kApiReference",
    0x14: "kExternalReference", 0x17: "kInternalReference",
    0x18: "kClearedWeakReference", 0x19: "kWeakPrefix", 0x1A: "kOffHeapTarget",
    0x1B: "kRegisterPendingForwardRef", 0x1C: "kResolvePendingForwardRef",
    0x1D: "kNewMetaMap", 0x1E: "kCodeBody",
}
SPACES = ["kReadOnlyHeap", "kOld", "kCode", "kMap"]


class Reader:
    def __init__(self, data, pos=0):
        self.data = data
        self.pos = pos

    def eof(self):
        return self.pos >= len(self.data)

    def byte(self):
        b = self.data[self.pos]
        self.pos += 1
        return b

    def putint(self):
        b0 = self.byte()
        n = (b0 & 3) + 1
        raw = b0
        for i in range(1, n):
            raw |= self.byte() << (8 * i)
        return raw >> 2

    def raw(self, nbytes):
        d = self.data[self.pos:self.pos + nbytes]
        self.pos += nbytes
        return d


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("file")
    ap.add_argument("--header", type=int, default=24, help="header size (24 or 28)")
    args = ap.parse_args()
    data = open(args.file, "rb").read()
    magic = struct.unpack_from("<I", data, 0)[0]
    if magic & 0xFFFF0000 != 0xC0DE0000:
        import brotli  # type: ignore
        data = brotli.decompress(data)
    vh = struct.unpack_from("<I", data, 4)[0]
    print(f"version_hash={vh:#010x} payload={len(data)-args.header} bytes at {args.header}")
    r = Reader(data, args.header)
    depth = 0
    while not r.eof():
        off = r.pos
        b = r.byte()
        if 0x00 <= b <= 0x03:
            n = r.putint()
            print(f"{off:6d} {'  '*depth}kNewObject space={SPACES[b]} size={n}w")
            depth += 1
        elif 0x40 <= b <= 0x5F:
            print(f"{off:6d} {'  '*depth}RootArrayConstant root={b-0x40}")
        elif 0x60 <= b <= 0x7F:
            n = b - 0x60 + 1
            d = r.raw(n * 4)
            print(f"{off:6d} {'  '*depth}FixedRawData {n}w = {d.hex()}")
        elif 0x80 <= b <= 0x8F:
            print(f"{off:6d} {'  '*depth}FixedRepeat n={b-0x80+2}")
        elif 0x90 <= b <= 0x97:
            print(f"{off:6d} {'  '*depth}HotObject i={b-0x90}")
        elif b == 0x04:
            print(f"{off:6d} {'  '*depth}kBackref idx={r.putint()}")
        elif b in TAGS:
            name = TAGS[b]
            if b == 0x12:
                n = r.putint()
                d = r.raw(n * 4)
                print(f"{off:6d} {'  '*depth}kVariableRawData {n}w = {d.hex()}")
            elif b == 0x0F:
                print(f"{off:6d} {'  '*depth}kVariableRepeat n={r.putint()+18}")
            elif b in (0x08, 0x09, 0x0B, 0x0C, 0x1B, 0x1C):
                a = r.putint()
                extra = f" offset={r.putint()}" if b == 0x08 else ""
                print(f"{off:6d} {'  '*depth}{name} {a}{extra}")
            elif b == 0x0A:
                print(f"{off:6d} {'  '*depth}kRootArray root={r.putint()}")
            else:
                print(f"{off:6d} {'  '*depth}{name}")
        else:
            print(f"{off:6d} {'  '*depth}??? 0x{b:02x}")
            return


if __name__ == "__main__":
    main()
