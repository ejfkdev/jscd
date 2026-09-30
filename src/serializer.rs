//! payload 反序列化器：把 V8 通用快照流解析为对象树（格式详见 docs/stream-format.md）。
//!
//! 设计要点：
//! - **零拷贝**：raw chunk 只记 (payload 内偏移, 长度)，不复制字节；`CodeCache` 借用 payload。
//! - **紧凑类型**：对象类型用 `Ty` 枚举（root 索引/结构指纹/字符串形态），不分配字符串。
//! - **O(log n) 查找**：槽按 index 升序，`slot_at`/`raw_at`/`array_elem` 用二分。
//! - 流在槽粒度自描述（raw chunk 与引用编码首字节空间不相交），对象体线性消费即可。
//! - tag/范围常量全部来自版本表（跨 5 个 V8 锚点验证，见 docs/VERSIONS.md）。

use crate::tables::VersionTable;
use serde::Serialize;
use std::borrow::Cow;

pub type ObjId = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Ref {
    /// 分配序对象
    Object(ObjId),
    /// roots 表项（索引 = roots.h 顺序）
    Root(usize),
    /// 只读堆 (chunk_index, chunk_offset)
    RoRef(u32, u32),
    /// attached reference（0 = 源码字符串占位）
    Attached(usize),
}

/// payload 内的字节区间（零拷贝 raw chunk）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Raw {
    pub off: usize,
    pub len: usize,
    /// 该 raw 自身是否推进对象的**槽指针**。
    /// V8 6.x 的 `kVariableRawData` 只是把字节写进对象（`CopyRaw` 之后 **不** 动
    /// `current`），随后的 `kSkip` 才是推进槽指针的那一步；9.4+ 的 `kVariableRawData`
    /// 则自己推进（`current = raw_data_out + size_in_bytes`）。定长 raw 各版本都推进
    /// （6.2 的序列化端在定长分支里显式 `to_skip = 0`，即"这条 tag 自带 skip"）。
    pub advances: bool,
}

/// 对象类型（紧凑表示，避免每对象分配字符串）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Ty {
    Pending,
    Unknown,
    /// map 命中 roots 表（索引 < 65536）
    Root(u16),
    /// 自身是 Map 对象（携带 instance_type）
    MapObj(u16),
    /// 只读堆引用
    Ro(u32, u32),
    /// 字符串
    Str(StrKind),
    /// 结构指纹识别
    Structural(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum StrKind {
    OneByte,
    TwoByte,
    Unknown,
}

impl Ty {
    /// 展示名（root 名借用表，其余为静态/短分配）。
    pub fn name<'t>(&self, table: &'t VersionTable) -> Cow<'t, str> {
        match self {
            Ty::Pending => Cow::Borrowed("?pending"),
            Ty::Unknown => Cow::Borrowed("?unknown"),
            Ty::Root(i) => match table.root_name(*i as usize) {
                Some(n) => Cow::Borrowed(n),
                None => Cow::Owned(format!("root#{i}")),
            },
            Ty::MapObj(it) => Cow::Owned(format!("Map(instance_type={it})")),
            Ty::Ro(c, o) => Cow::Owned(format!("?ro:{c}/{o}")),
            Ty::Str(k) => Cow::Borrowed(match k {
                StrKind::OneByte => "OneByteString",
                StrKind::TwoByte => "TwoByteString",
                StrKind::Unknown => "String",
            }),
            Ty::Structural(s) => Cow::Borrowed(s),
        }
    }

    /// 是否匹配某个 roots 名（忽略 String:/Symbol: 前缀与 Map 后缀）。
    pub fn is(&self, table: &VersionTable, want: &str) -> bool {
        let n = self.name(table);
        let base = n
            .strip_prefix("String:")
            .or_else(|| n.strip_prefix("Symbol:"))
            .unwrap_or(&n);
        // 13.x 起同类对象有 trusted 空间变体（TrustedByteArray / TrustedFixedArray…），
        // 名字是 "TrustedXxxMap" —— 这两种写法都算匹配。
        let untrusted = base.strip_prefix("Trusted").unwrap_or(base);
        let stem = |s: &str| s.strip_suffix("Map").unwrap_or(s).to_string();
        base == want
            || stem(base) == want
            || untrusted == want
            || stem(untrusted) == want
    }

    pub fn is_string(&self, table: &VersionTable) -> bool {
        match self {
            Ty::Str(_) => true,
            Ty::Root(i) => table
                .roots
                .get(*i as usize)
                .map(|n| n.starts_with("String:"))
                .unwrap_or(false),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Object {
    pub ty: Ty,
    pub byte_size: usize,
    /// payload 内起始偏移（诊断用）
    pub start_offset: usize,
    /// 槽序列：index 升序（0 = map）
    pub slots: Vec<Slot>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Slot {
    pub index: usize,
    pub value: SlotValue,
}

#[derive(Debug, Clone, Serialize)]
pub enum SlotValue {
    Ref(Ref),
    /// 连续相同引用（FixedRepeat/VariableRepeat）
    Repeat(usize, Box<SlotValue>),
    /// 6.x 的 repeat：语义是"复制**前一个槽**的值"（V8 `UnalignedCopy(&object, current - 1)`），
    /// 流里**没有**额外的引用；展开时用同对象上一个槽的值补上。
    RepeatPrev(usize),
    /// 6.x 的跳过：`kSkip` / `*WithSkip` 跳过若干**字节**（V8 直接推进字节指针），不写值。
    Skip(usize),
    /// 只从流里吃掉若干字节（如 kOffHeapBackingStore 的内联缓冲），**不**推进槽指针。
    StreamBytes(usize),
    /// raw chunk（字节在 payload 里，不复制）
    Raw(Raw),
    ClearedWeak,
    /// 前缀类（weak / 间接指针 / 受保护指针）后随的引用
    WeakRef(Box<SlotValue>),
    /// 尚未 resolve 的 pending forward ref
    PendingRef(u32),
}

impl SlotValue {
    pub fn as_ref(&self) -> Option<Ref> {
        match self {
            SlotValue::Ref(r) => Some(*r),
            SlotValue::WeakRef(inner) => inner.as_ref().as_ref(),
            _ => None,
        }
    }
    pub fn as_raw(&self) -> Option<Raw> {
        match self {
            SlotValue::Raw(r) => Some(*r),
            _ => None,
        }
    }
}

/// FixedArray 元素（借用 payload）。
#[derive(Debug, Clone, Copy)]
pub enum Elem<'a> {
    Ref(Ref),
    Smi(i64),
    Bytes(&'a [u8]),
}

impl<'a> Elem<'a> {
    pub fn as_ref(&self) -> Option<Ref> {
        match self {
            Elem::Ref(r) => Some(*r),
            _ => None,
        }
    }
    pub fn as_smi(&self) -> Option<i64> {
        match self {
            Elem::Smi(v) => Some(*v),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum HotEntry {
    Object(ObjId),
    Root(usize),
    /// 只读堆引用（解不出对象 id）：V8 的 `PutBackReference` 对 RO 对象同样入环，
    /// 占位必须记账，否则后续 hot 索引整体错位。
    Ro(u32, u32),
}

/// HotObjectsList：固定 8 槽环形数组，写入即前进（对齐 serializer.h 语义）。
#[derive(Debug, Clone, Default)]
struct HotRing {
    slots: [Option<HotEntry>; 8],
    index: usize,
}

impl HotRing {
    #[inline]
    fn add(&mut self, e: HotEntry) {
        self.slots[self.index] = Some(e);
        self.index = (self.index + 1) & 7;
    }
    #[inline]
    fn get(&self, i: usize) -> Option<HotEntry> {
        self.slots.get(i).copied().flatten()
    }
}

pub struct CodeCache<'a> {
    payload: &'a [u8],
    tagged_size: usize,
    pub objects: Vec<Object>,
    pub top_sfi: ObjId,
}

struct Walker<'a> {
    data: &'a [u8],
    pos: usize,
    tagged_size: usize,
    tags: &'a std::collections::HashMap<String, u8>,
    /// 老族的**全部**常量（含 ≥256 的普通常量），供 `lnum()` 用
    legacy_consts: &'a std::collections::HashMap<String, u32>,
    /// 老族（V8 ≤ 8.4）的分配器状态：backref 用 (chunk, offset) 寻址，
    /// 需要按 reservation 表模拟分配才能把引用映射回对象。
    legacy: Option<LegacyAlloc>,
    objects: Vec<Object>,
    hot: HotRing,
    pending: std::collections::HashMap<u32, Vec<(ObjId, usize)>>,
    /// 6.x 的 `*WithSkip` 前缀读到的待跳过字节数：值落在跳过之后的位置，
    /// 由对象内容循环统一记账（槽指针 += skip，再写值）。
    extra_skip: usize,
    /// 版本表（调试打印 root 名字用）
    table: &'a VersionTable,
}

type R<T> = Result<T, String>;

impl<'a> Walker<'a> {
    #[inline]
    fn tag(&self, name: &str) -> R<u8> {
        self.tags
            .get(name)
            .copied()
            .ok_or_else(|| format!("tag {name} missing from table"))
    }
    #[inline]
    fn opt_tag(&self, name: &str) -> Option<u8> {
        self.tags.get(name).copied()
    }
    #[inline]
    fn tag_any(&self, names: &[&str]) -> Option<u8> {
        names.iter().find_map(|n| self.opt_tag(n))
    }
    #[inline]
    fn byte(&mut self) -> R<u8> {
        let b = *self.data.get(self.pos).ok_or("unexpected end of payload")?;
        self.pos += 1;
        Ok(b)
    }
    #[inline]
    fn peek(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }
    /// PutUint30（旧 PutInt）：首字节低 2 位 = 字节数-1，小端，值 = raw >> 2。
    #[inline]
    fn putint(&mut self) -> R<u32> {
        let b0 = self.byte()?;
        let n = (b0 & 3) as usize + 1;
        let mut raw = b0 as u32;
        for i in 1..n {
            raw |= (self.byte()? as u32) << (8 * i);
        }
        Ok(raw >> 2)
    }
    #[inline]
    fn raw(&mut self, n: usize) -> R<Raw> {
        if self.pos + n > self.data.len() {
            return Err(format!("raw read {n} bytes overruns payload"));
        }
        let r = Raw {
            off: self.pos,
            len: n,
            advances: true,
        };
        self.pos += n;
        Ok(r)
    }
    /// 对象「类型」的粗略描述（= 它 map 槽所指 root 的名字）：给 `JSCD_DBG_REF`
    /// 的对拍用 —— V8 轨迹里每个回引/hot 都带目标类型，比只对索引强得多
    /// （hot 环错位就是这么被抓出来的）。
    fn obj_map_name(&self, id: ObjId) -> String {
        match self.objects.get(id).and_then(|o| o.slots.first()) {
            Some(Slot { value: SlotValue::Ref(Ref::Root(k)), .. }) => {
                self.table.root_name(*k).unwrap_or("?").to_string()
            }
            Some(Slot { value: SlotValue::Ref(Ref::Object(m)), .. }) => format!("map#{m}"),
            _ => "?".into(),
        }
    }

    /// 版本表里的整数常量（老族把 `kNumberOfSpaces`/`kSpaceTagSize`/`kPageSizeBits`/… 与
    /// 标签一起放在 legacy 表里；值可能是任意 u32）。
    #[inline]
    fn lnum(&self, name: &str) -> Option<u32> {
        self.legacy_consts.get(name).copied()
    }

    /// 6.x 线格式：backref 是**一个**按 `SerializerReference` 位域打包的 uint32，
    /// 且 hot/根常量有 `*WithSkip` 变体（7.8/8.4 都没有 `kBackrefWithSkip`）。
    #[inline]
    fn is_six_x(&self) -> bool {
        self.legacy_consts.contains_key("kBackrefWithSkip")
    }
    #[inline]
    fn n_spaces(&self) -> u8 {
        self.lnum("kNumberOfSpaces").map(|v| v as u8).unwrap_or(6)
    }
    #[inline]
    fn map_space(&self) -> u8 {
        self.lnum("kMapSpace").map(|v| v as u8).unwrap_or(4)
    }
    #[inline]
    fn lo_space(&self) -> u8 {
        self.lnum("kLoSpace").map(|v| v as u8).unwrap_or(5)
    }

    /// hot 标签 → (是否 WithSkip, index)。6.2 的两个区间不相邻（0x38 / 0x58）。
    fn hot_tag_index(&self, b: u8) -> Option<(bool, usize)> {
        let plain = self.opt_tag("kHotObject")?;
        if (plain..plain + 8).contains(&b) {
            return Some((false, (b & 7) as usize));
        }
        if let Some(hs) = self.opt_tag("kHotObjectWithSkip") {
            if (hs..hs + 8).contains(&b) {
                return Some((true, (b & 7) as usize));
            }
        }
        None
    }

    /// 根常量标签 → (是否 WithSkip, root index)（index = data & kRootArrayConstantsMask）。
    fn root_const_tag_index(&self, b: u8) -> Option<(bool, usize)> {
        let mask = self.lnum("kRootArrayConstantsMask").unwrap_or(0x1f) as u8;
        let plain = self.opt_tag("kRootArrayConstants")?;
        if (plain..plain + mask + 1).contains(&b) {
            return Some((false, (b & mask) as usize));
        }
        if let Some(hs) = self.opt_tag("kRootArrayConstantsWithSkip") {
            if (hs..hs + mask + 1).contains(&b) {
                return Some((true, (b & mask) as usize));
            }
        }
        None
    }

    #[inline]
    fn push_slot(&mut self, id: ObjId, index: usize, value: SlotValue) {
        if let Ok(want) = std::env::var("JSCD_DBG_OBJSLOTS") {
            if want == "*" || want.parse::<usize>() == Ok(id) {
                eprintln!("[slot] obj={id} idx={index} value={value:?}");
            }
        }
        self.objects[id].slots.push(Slot { index, value });
    }

    /// 引用序列的逐条事件流（`JSCD_DBG_REF=1`）：与 V8 `--trace-serializer` 的
    /// "Encoding …" 行一一对应，用来做序列级对拍。V8 的轨迹只为新对象/回引/热对象/root
    /// 打印，其它（raw/repeat/对齐前缀）不打印 —— 这里多打印的部分比对脚本按类型过滤。
    fn parse_ref(&mut self, depth: usize) -> R<SlotValue> {
        if depth > 64 {
            return Err("ref recursion too deep".into());
        }
        if std::env::var("JSCD_DBG_REF").is_err() {
            return self.parse_ref_inner(depth);
        }
        let ref_pos = self.pos;
        let b0 = self.data.get(ref_pos).copied().unwrap_or(0);
        let t_new = self.opt_tag("kNewObject").unwrap_or(0);
        let new_spaces = if self.legacy.is_some() {
            self.n_spaces()
        } else {
            0x08
        };
        let is_new = (t_new..t_new + new_spaces).contains(&b0);
        if is_new {
            // 行要在该对象的 map 引用之前打印 → 先手工解码 tag+size（不消费字节）。
            let a0 = self.data.get(ref_pos + 1).copied().unwrap_or(0) as u32;
            let a1 = self.data.get(ref_pos + 2).copied().unwrap_or(0) as u32;
            let a2 = self.data.get(ref_pos + 3).copied().unwrap_or(0) as u32;
            let a3 = self.data.get(ref_pos + 4).copied().unwrap_or(0) as u32;
            let raw = a0 | (a1 << 8) | (a2 << 16) | (a3 << 24);
            let bytes = (raw & 3) + 1;
            let mask = 0xffff_ffffu32 >> (32 - (bytes << 3));
            let words = (raw & mask) >> 2;
            eprintln!("[ref] new pos={ref_pos} space={} words={words}", b0 - t_new);
        }
        let r = self.parse_ref_inner(depth);
        match &r {
            Err(e) => {
                eprintln!("[ref] pos={ref_pos} ERR {e}");
                return r;
            }
            Ok(_) if is_new => return r,
            Ok(_) => {}
        }
        let t_backref = self.opt_tag("kBackref").unwrap_or(8);
        let _t_hot = self.opt_tag("kHotObject").unwrap_or(0x90);
        let t_root = self.opt_tag("kRootArray");
        let _t_rootc = self.opt_tag("kRootArrayConstants").unwrap_or(0x40);
        let desc = match r.as_ref().unwrap() {
            SlotValue::Ref(Ref::Object(id)) => format!("obj={id} ty={}", self.obj_map_name(*id)),
            SlotValue::Ref(Ref::Root(ix)) => format!("root={ix}"),
            SlotValue::Ref(Ref::RoRef(c, o)) => format!("ro=({c},{o})"),
            SlotValue::Ref(Ref::Attached(i)) => format!("attached={i}"),
            SlotValue::Raw(_) => "raw".into(),
            SlotValue::Repeat(n, _) => format!("repeat={n}"),
            SlotValue::RepeatPrev(n) => format!("repeatprev={n}"),
            SlotValue::Skip(n) => format!("skip={n}"),
            SlotValue::StreamBytes(n) => format!("streambytes={n}"),
            SlotValue::WeakRef(_) => "weak".into(),
            SlotValue::ClearedWeak => "cleared".into(),
            SlotValue::PendingRef(_) => "pending".into(),
        };
        let n_sp_dbg = self.n_spaces();
        if (t_backref..t_backref + n_sp_dbg).contains(&b0) {
            eprintln!("[ref] backref pos={ref_pos} space={} {desc}", b0 - t_backref);
        } else if let Some((ws, i)) = self.hot_tag_index(b0) {
            eprintln!("[ref] hot pos={ref_pos} i={i} skip={ws} {desc}");
        } else if Some(b0) == t_root {
            eprintln!("[ref] root pos={ref_pos} {desc}");
        } else if let Some((ws, i)) = self.root_const_tag_index(b0) {
            eprintln!("[ref] rootconst pos={ref_pos} i={i} skip={ws} {desc}");
        } else {
            eprintln!("[ref] other pos={ref_pos} b={b0:#04x} {desc}");
        }
        r
    }

    fn parse_ref_inner(&mut self, depth: usize) -> R<SlotValue> {
        let b_raw = self.byte()?;
        // 老族（≤8.4）：只有 kNewObject / kBackref 这一对家族把空间编号压在低 3 位
        // （0x00..0x05 / 0x08..0x0d，见 serializer-common.h 的 UNUSED_BYTE_CODES），
        // 其余标签是扁平值 —— 早先"一律剥离低 3 位"会把 kRootArray(17) 当成 16。
        let b = b_raw;
        if let Some(_) = &self.legacy {
            if std::env::var("JSCD_DBG_LEGACY").is_ok() {
                eprintln!("[legacy-ref] pos={} b={b_raw:#04x}", self.pos - 1);
            }
            let t_new = self.tag("kNewObject")?;
            let t_backref = self.tag("kBackref")?;
            let n_sp = self.n_spaces();
            if (t_new..t_new + n_sp).contains(&b_raw) {
                let space = (b_raw - t_new) as u8;
                let id = self.parse_new_object_space(depth, space)?;
                // **不能**入 hot 环：V8 8.4 只在 `PutRoot`（根数组分支）与
                // `PutBackReference` 两处 `hot_objects_.Add`，新对象不入环。
                // 多记一笔会让后续 hot 索引整体错位 —— node14 closure 的
                // `target` 名槽（hot 3）因此指到了 inc 的 SFI，函数名全丢。
                return Ok(SlotValue::Ref(Ref::Object(id)));
            }
            // 6.x：`kBackrefWithSkip`（0x10..0x15）——先读"跳过"字节数（跳过的是对象内的
            // **槽指针**，不是解析流程），随后与普通 backref 同构。
            if self.is_six_x() {
                if let Some(t_bs) = self.opt_tag("kBackrefWithSkip") {
                    if (t_bs..t_bs + n_sp).contains(&b_raw) {
                        let skip = self.putint()? as usize;
                        self.extra_skip += skip;
                        return self.legacy_backref((b_raw - t_bs) as u8);
                    }
                }
            }
            if (t_backref..t_backref + n_sp).contains(&b_raw) {
                return self.legacy_backref((b_raw - t_backref) as u8);
            }
            // 老族专有：chunk 切换 / 对齐 / 延迟内容
            if Some(b_raw) == self.opt_tag("kNextChunk") {
                let space = self.byte()?;
                if let Some(l) = self.legacy.as_mut() {
                    l.next_chunk(space);
                }
                return self.parse_ref(depth + 1);
            }
            if let Some(align) = self.opt_tag("kAlignmentPrefix") {
                if (align..align + 3).contains(&b_raw) {
                    let a = (b_raw - align + 1) as u32;
                    if let Some(l) = self.legacy.as_mut() {
                        l.align = a;
                    }
                    return self.parse_ref(depth + 1);
                }
            }
        }
        let t_new = self.tag("kNewObject")?;
        let t_backref = self.tag("kBackref")?;
        let _t_hot = self.tag("kHotObject")?;
        let _t_root_const = self.tag("kRootArrayConstants")?;
        let t_fixed_raw = self.tag("kFixedRawData")?;
        let t_fixed_repeat = self.tag_any(&["kFixedRepeat", "kFixedRepeatRoot"]);
        let t_fixed_repeat_root_only =
            self.opt_tag("kFixedRepeat").is_none() && self.opt_tag("kFixedRepeatRoot").is_some();

        // 「新对象」的取值区间要按**空间数**算，不能想当然用 `t_new..t_backref`：
        // 6.2 只有 5 个空间，`kRootArray` 恰好落在 0x05 —— 落在 kNewObject 的
        // 数值区间内（标签表里两者是相邻的独立值）。按区间判定会把 root 引用
        // 当成"新对象 + 空间 5"（node8 的第一个 payload 就撞上：root 284 → 假对象）。
        let n_new = if self.legacy.is_some() {
            self.n_spaces() as u8
        } else {
            t_backref.saturating_sub(t_new)
        };
        if (t_new..t_new.saturating_add(n_new)).contains(&b) {
            let id = self.parse_new_object(depth)?;
            Ok(SlotValue::Ref(Ref::Object(id)))
        } else if b == t_backref {
            let idx = self.putint()? as usize;
            if idx >= self.objects.len() {
                return Err(format!("backref {idx} out of range ({})", self.objects.len()));
            }
            self.hot.add(HotEntry::Object(idx)); // PutBackReference 会入 hot 环
            Ok(SlotValue::Ref(Ref::Object(idx)))
        } else if let Some((with_skip, i)) = self.hot_tag_index(b_raw) {
            if with_skip {
                let skip = self.putint()? as usize;
                self.extra_skip += skip;
            }
            match self.hot.get(i) {
                Some(HotEntry::Object(id)) => Ok(SlotValue::Ref(Ref::Object(id))),
                Some(HotEntry::Root(r)) => Ok(SlotValue::Ref(Ref::Root(r))),
                Some(HotEntry::Ro(c, o)) => Ok(SlotValue::Ref(Ref::RoRef(c, o))),
                None => Err(format!("hot object index {i} out of ring")),
            }
        } else if b == self.tag("kRootArray")? {
            let idx = self.putint()? as usize;
            self.hot.add(HotEntry::Root(idx));
            Ok(SlotValue::Ref(Ref::Root(idx)))
        } else if let Some((with_skip, i)) = self.root_const_tag_index(b_raw) {
            if with_skip {
                let skip = self.putint()? as usize;
                self.extra_skip += skip;
            }
            Ok(SlotValue::Ref(Ref::Root(i)))
        } else if Some(b) == self.opt_tag("kReadOnlyHeapRef") {
            let c = self.putint()?;
            let o = self.putint()?;
            Ok(SlotValue::Ref(Ref::RoRef(c, o)))
        } else if b == self.tag("kAttachedReference")? {
            Ok(SlotValue::Ref(Ref::Attached(self.putint()? as usize)))
        } else if self.is_six_x() && Some(b) == self.opt_tag("kBuiltin") {
            // 6.x：`kBuiltin <index>` —— 内建代码对象（不在 payload 内）。
            // 占位成"内建空间"的 RoRef，ro-map 不会误命中（空间号 ≠ 0）。
            let i = self.putint()?;
            Ok(SlotValue::Ref(Ref::RoRef(u32::MAX - 1, i)))
        } else if self.is_six_x() && Some(b) == self.opt_tag("kExternalReference") {
            // 6.x：外部引用按 id 编号（地址由 embedder 提供）→ 占位
            let i = self.putint()?;
            Ok(SlotValue::Ref(Ref::RoRef(u32::MAX - 2, i)))
        } else if self.is_six_x() && Some(b) == self.opt_tag("kApiReference") {
            // `kApiReference <skip> <id>`（skip = 先跳过的字节数）
            let skip = self.putint()? as usize;
            self.extra_skip += skip;
            let i = self.putint()?;
            Ok(SlotValue::Ref(Ref::RoRef(u32::MAX - 3, i)))
        } else if self.is_six_x()
            && (Some(b) == self.opt_tag("kInternalReference")
                || Some(b) == self.opt_tag("kInternalReferenceEncoded"))
        {
            // `<pc_offset> <target_offset>`：就地改 reloc，不写槽
            let _ = self.putint()?;
            let _ = self.putint()?;
            Ok(SlotValue::Skip(0))
        } else if self.is_six_x()
            && (Some(b) == self.opt_tag("kDeoptimizerEntryPlain")
                || Some(b) == self.opt_tag("kDeoptimizerEntryFromCode"))
        {
            let _ = self.putint()?;
            Ok(SlotValue::Skip(0))
        } else if self.is_six_x() && Some(b) == self.opt_tag("kOffHeapBackingStore") {
            // `byte_length` + 内联缓冲字节（不写槽）
            let len = self.putint()? as usize;
            Ok(SlotValue::StreamBytes(len))
        } else if self.is_six_x() && Some(b) == self.opt_tag("kVariableRawCode") {
            // `size_in_bytes` + 直接写进 Code 体的字节（不推槽）
            let len = self.putint()? as usize;
            Ok(SlotValue::StreamBytes(len))
        } else if self.is_six_x() && Some(b) == self.opt_tag("kEmbedderFieldsData") {
            // `<object_id> <size>` + 字节（不写普通槽）
            let _ = self.putint()?;
            let len = self.putint()? as usize;
            Ok(SlotValue::StreamBytes(len))
        } else if self.is_six_x() && Some(b) == self.opt_tag("kOffHeapTarget") {
            // `<skip> <builtin_index>`：写一个值（槽推进 1）
            let skip = self.putint()? as usize;
            self.extra_skip += skip;
            let _ = self.putint()?;
            Ok(SlotValue::Ref(Ref::RoRef(u32::MAX - 4, 0)))
        } else if Some(b) == self.opt_tag("kPartialSnapshotCache") {
            // 7.8：部分快照缓存（老族专有）→ 与只读缓存同样按"外部对象"编号
            let i = self.putint()? as usize;
            Ok(SlotValue::Ref(Ref::RoRef(u32::MAX, i as u32)))
        } else if Some(b) == self.opt_tag("kStartupObjectCache")
            || Some(b) == self.opt_tag("kReadOnlyObjectCache")
            || Some(b) == self.opt_tag("kSharedHeapObjectCache")
        {
            let i = self.putint()? as usize;
            Ok(SlotValue::Ref(Ref::RoRef(u32::MAX, i as u32)))
        } else if Some(b) == self.opt_tag("kRegisterPendingForwardRef") {
            Ok(SlotValue::PendingRef(self.putint()?))
        } else if Some(b) == self.opt_tag("kClearedWeakReference") {
            Ok(SlotValue::ClearedWeak)
        } else if Some(b) == self.opt_tag("kWeakPrefix")
            || Some(b) == self.opt_tag("kIndirectPointerPrefix")
            || Some(b) == self.opt_tag("kProtectedPointerPrefix")
        {
            let inner = self.parse_ref(depth + 1)?;
            Ok(SlotValue::WeakRef(Box::new(inner)))
        } else if Some(b) == self.opt_tag("kInitializeSelfIndirectPointer") {
            Ok(SlotValue::ClearedWeak)
        } else if Some(b) == self.opt_tag("kAllocateJSDispatchEntry")
            || Some(b) == self.opt_tag("kJSDispatchEntry")
        {
            let _ = self.putint()?;
            Ok(SlotValue::ClearedWeak)
        } else if (t_fixed_raw..t_fixed_raw + 32).contains(&b) {
            let n = (b - t_fixed_raw + 1) as usize;
            let len = n * self.tagged_size;
            Ok(SlotValue::Raw(Raw { advances: true, ..self.raw(len)? }))
        } else if Some(b) == self.opt_tag("kVariableRawData") {
            let n = self.putint()? as usize;
            // 单位随版本变：8.4 的 `int size_in_bytes = source_.GetInt()`（**字节**），
            // 9.4+ 的 `int size_in_tagged = ...`（**tagged 单位**）。按 tagged 单位读
            // 老族会一次多吞 8 倍字节，主段直接走偏 —— node14 的 branch 等 9 个 fixture
            // 报 `legacy backref (0,0,30816) 未命中` 就是这个。
            let bytes = if self.legacy.is_some() { n } else { n * self.tagged_size };
            let advances = self.table.var_raw_advances;
            Ok(SlotValue::Raw(Raw {
                advances,
                ..self.raw(bytes.max(1))?
            }))
        } else if Some(b) == self.opt_tag("kSkip") {
            // 6.x：`kSkip` —— 读掉距离后跳过若干**字节**。
            // 注意：6.2 的 `OutputRawData` 在**变长 raw** 分支里 `to_skip` 没有清零，
            // 于是"raw(L) 紧跟 skip(L)"成对出现（定长 raw 分支才有 `to_skip = 0`）。
            // 对反序列化来说槽指针已经由 raw 前进过一次，这里再按 V8 的原样前进会
            // 把对象槽账直接顶爆（node8 的 operators/for_of_in 等就死在这）。
            // 实测（node8/10 全 fixture + 语料）按"只吃流、不进槽账"处理才自洽。
            let bytes = self.putint()? as usize;
            Ok(SlotValue::Skip(bytes))
        } else if t_fixed_repeat.map_or(false, |t| (t..t + 16).contains(&b)) {
            let base = t_fixed_repeat.unwrap();
            // 6.x 的计数从 1 起（`DecodeFixedRepeatCount = bytecode - kFixedRepeatStart`，
            // kFixedRepeatStart = kFixedRepeat - 1），且值是"前一个槽"；
            // 7.8/8.4 从 2 起（kFirstEncodableRepeatCount = 2）且流里带引用。
            if self.is_six_x() {
                return Ok(SlotValue::RepeatPrev((b - base + 1) as usize));
            }
            let n = (b - base + 2) as usize;
            if t_fixed_repeat_root_only {
                let root = self.byte()? as usize;
                Ok(SlotValue::Repeat(n, Box::new(SlotValue::Ref(Ref::Root(root)))))
            } else {
                let inner = self.parse_ref(depth + 1)?;
                Ok(SlotValue::Repeat(n, Box::new(inner)))
            }
        } else if Some(b) == self.tag_any(&["kVariableRepeat"]) {
            if self.is_six_x() {
                // 6.x：`int repeats = source_.GetInt();`（无 +N 偏移），值 = 前一个槽
                let n = self.putint()? as usize;
                return Ok(SlotValue::RepeatPrev(n));
            }
            let n = self.putint()? as usize + 18;
            let inner = self.parse_ref(depth + 1)?;
            Ok(SlotValue::Repeat(n, Box::new(inner)))
        } else if Some(b) == self.opt_tag("kVariableRepeatRoot") {
            let n = self.putint()? as usize + 18;
            let root = self.byte()? as usize;
            Ok(SlotValue::Repeat(n, Box::new(SlotValue::Ref(Ref::Root(root)))))
        } else if Some(b) == self.opt_tag("kNewMetaMap")
            || Some(b) == self.opt_tag("kNewContextlessMetaMap")
            || Some(b) == self.opt_tag("kNewContextfulMetaMap")
        {
            Err(format!(
                "meta map tag 0x{b:02x} at {} not supported (rare in code caches)",
                self.pos - 1
            ))
        } else {
            Err(format!(
                "unknown serialization tag 0x{b:02x} at {}",
                self.pos - 1
            ))
        }
    }

    /// 老族 backref 的两种线格式。
    ///
    /// - **7.8 / 8.4**：`PutBackReference` 写两个 varint（chunk_index, chunk_offset），
    ///   但 MAP / LO 空间只写一个序数（map_index / large_object_index）。
    /// - **6.x**：写**一个** uint32，按 `SerializerReference` 位域打包
    ///   （`[SpaceBits][ValueIndex]`，ValueIndex = chunk_index<<16 | chunk_offset>>3），
    ///   MAP/LO 的 ValueIndex 就是序数本身；RO 空间按页号+页内偏移解析。
    fn legacy_backref(&mut self, space: u8) -> R<SlotValue> {
        let map_space = self.map_space();
        let lo_space = self.lo_space();
        if self.is_six_x() {
            let bf = self.putint()? as u32;
            let space_tag = self.lnum("kSpaceTagSize").unwrap_or(3);
            // V8 写的是 `reference.back_reference()` = **只有** ValueIndex 那一段
            // （`bitfield_ & (ChunkOffsetBits::kMask | ChunkIndexBits::kMask)`），
            // 空间号在**标签**里 —— 位域高位恒为 0，不能当空间用。
            let value_bits = 32 - space_tag;
            let index = bf & ((1u32 << value_bits) - 1);
            if space == map_space {
                return match self.legacy.as_ref().and_then(|l| l.resolve_ordinal(4, index as usize)) {
                    Some(id) => {
                        self.hot.add(HotEntry::Object(id));
                        Ok(SlotValue::Ref(Ref::Object(id)))
                    }
                    None => Err(format!("legacy map backref index {index} out of range")),
                };
            }
            if space == lo_space {
                return match self.legacy.as_ref().and_then(|l| l.resolve_ordinal(5, index as usize)) {
                    Some(id) => {
                        self.hot.add(HotEntry::Object(id));
                        Ok(SlotValue::Ref(Ref::Object(id)))
                    }
                    None => Err(format!("legacy large-object backref index {index} out of range")),
                };
            }
            let align_bits = (self.tagged_size as u32).max(1).trailing_zeros();
            let off_bits = self
                .lnum("kPageSizeBits")
                .unwrap_or(19)
                .saturating_sub(align_bits);
            let chunk_offset = (index & ((1u32 << off_bits) - 1)) << align_bits;
            let chunk_index = index >> off_bits;
            return match self
                .legacy
                .as_ref()
                .and_then(|l| l.resolve(space, chunk_index, chunk_offset))
            {
                Some(id) => {
                    self.hot.add(HotEntry::Object(id));
                    Ok(SlotValue::Ref(Ref::Object(id)))
                }
                None => {
                    // 只读堆对象不在 payload 内（与 8.4 同一处理）：交给 ro-map。
                    self.hot.add(HotEntry::Ro(chunk_index, chunk_offset));
                    Ok(SlotValue::Ref(Ref::RoRef(chunk_index, chunk_offset)))
                }
            };
        }

        // 7.8 / 8.4
        if space == map_space || space == lo_space {
            let index = self.putint()? as usize;
            let canonical = if space == map_space { 4 } else { 5 };
            return match self.legacy.as_ref().and_then(|l| l.resolve_ordinal(canonical, index)) {
                Some(id) => {
                    self.hot.add(HotEntry::Object(id));
                    Ok(SlotValue::Ref(Ref::Object(id)))
                }
                None => Err(format!(
                    "legacy {} backref index {index} out of range (have {})",
                    if space == map_space { "map" } else { "large-object" },
                    if space == map_space {
                        self.legacy.as_ref().map_or(0, |l| l.maps.len())
                    } else {
                        self.legacy.as_ref().map_or(0, |l| l.large.len())
                    }
                )),
            };
        }
        let chunk = self.putint()?;
        let offset = self.putint()?;
        match self.legacy.as_ref().and_then(|l| l.resolve(space, chunk, offset)) {
            Some(id) => {
                self.hot.add(HotEntry::Object(id));
                Ok(SlotValue::Ref(Ref::Object(id)))
            }
            None => {
                // 老族里也有**只读堆引用**：RO 堆不在 payload 内（它来自 V8 快照），
                // 于是 V8 写的是 (space, chunk, offset) 形式的地址，这里的 offset 就是
                // RO 堆内的偏移 —— 与 `kReadOnlyHeapRef` 同一套 (chunk, offset) 编号。
                // 实测 node14 `branch`：V8 轨迹里是 `back reference to: String "small"/"big"`，
                // 而地址 0x…3c7861 / 0x…3c7639 相对 0x…3c0000 正是 30816 / 30264 ✓。
                // 这类引用交给 ro-map 解析（查不到时反编译器会留占位）。
                if std::env::var("JSCD_DBG_LEGACY").is_ok() {
                    eprintln!("[ro-ref] ({space},{chunk},{offset}) → 按只读堆引用处理");
                }
                self.hot.add(HotEntry::Ro(chunk, offset));
                Ok(SlotValue::Ref(Ref::RoRef(chunk, offset)))
            }
        }
    }

    fn parse_new_object(&mut self, depth: usize) -> R<ObjId> {
        self.parse_new_object_space(depth, 0)
    }

    /// size/map/slots 的读取；老族额外把分配地址记进分配器（backref 要用）。
    fn parse_new_object_space(&mut self, depth: usize, space: u8) -> R<ObjId> {
        let tag_offset = self.pos.saturating_sub(1);
        let size_words = self.putint()? as usize;
        // 6.x：LO 空间对象在 size 之后还有一个「可执行性」字节
        // （V8 `Allocate(LO_SPACE)` 里 `Executability exec = source_.Get()`，在布置内容之前）。
        if self.legacy.is_some() && self.is_six_x() && space == self.lo_space() {
            let _ = self.byte()?;
        }
        // 老族（≤8.4）的 size 单位是 `1 << kObjectAlignmentBits`，而 V8 8.4 的
        // `kObjectAlignmentBits = kTaggedSizeLog2` —— 也就是**等于 tagged size**
        // （指针压缩构建 ts=4 → 4 字节/单位，非压缩 ts=8 → 8）。按 8 算会让每个对象的
        // 字节尺寸翻倍、后续地址全偏（space2 的 backref 因此命不中）。
        // 现代族固定 8 字节单位（kObjectAlignmentBits = 3）。
        let unit = if self.legacy.is_some() {
            self.tagged_size.max(4)
        } else {
            8
        };
        let byte_size = size_words * unit;
        if let Ok(v) = std::env::var("JSCD_DBG_OBJ") {
            let want = v == "1" || space == v.parse::<u8>().unwrap_or(255);
            if want {
                let ctx = self
                    .data
                    .get(tag_offset..(tag_offset + 12).min(self.data.len()))
                    .unwrap_or(&[]);
                eprintln!(
                    "[objtag] pos={tag_offset} space={space} size_words={size_words} unit={unit} bytes={byte_size} ctx={ctx:02x?}"
                );
            }
        }
        let id = self.objects.len();
        self.objects.push(Object {
            ty: Ty::Pending,
            byte_size,
            start_offset: tag_offset,
            slots: Vec::with_capacity(size_words.min(64)),
        });
        if let Some(l) = self.legacy.as_mut() {
            l.allocate(space, byte_size as u32, id);
            if std::env::var("JSCD_DBG_LEGACY").is_ok() && (id >= 18) {
                let ctx = self.data.get(tag_offset..(tag_offset + 12).min(self.data.len())).unwrap_or(&[]);
                eprintln!(
                    "[obj] id={id} space={space} words={size_words} tag_pos={tag_offset} bytes={:02x?}",
                    ctx
                );
            }
        }
        let map = self.parse_ref(depth + 1)?;
        self.push_slot(id, 0, map);

        // 老族（≤8.4）没有 pending forward ref 机制（表里没有这个标签）→ 跳过这段解析。
        // 注意：这里**不能** return —— 后面还有槽循环（曾因此让老族对象只解析出一个 map）。
        if let Some(t_resolve) = self.opt_tag("kResolvePendingForwardRef") {
            loop {
                match self.peek() {
                    Some(b) if b == t_resolve => {
                        self.byte()?;
                        let pid = self.putint()?;
                        if let Some(list) = self.pending.get_mut(&pid) {
                            for (oid, vec_pos) in list.drain(..) {
                                self.objects[oid].slots[vec_pos].value =
                                    SlotValue::Ref(Ref::Object(id));
                            }
                        }
                        self.pending.remove(&pid);
                    }
                    _ => break,
                }
            }
        }
        // V8 的 `ReadData` 推进的是**字节**指针：`current++` 才是 +kPointerSize，
        // 而 `kVariableRawData` / `kFixedRawData` / `kSkip` 都按字节前进。用"槽"记账时，
        // 长度不是 tagged size 整数倍的 raw（6.x 的对象里就有 int32 尾巴）会算成 0 槽，
        // 循环空转、把兄弟对象整片吞进来 —— node8 的 destructure/operators 就这么死的。
        // 改为按字节记账，槽号只在写槽位时取整。
        let ts = self.tagged_size;
        let size_bytes = size_words * unit;
        let mut consumed_bytes = ts; // map 已占 1 个 tagged 槽
        let t_deferred = self.opt_tag("kDeferred");
        while consumed_bytes < size_bytes {
            // 老族：`kDeferred` 出现在对象头之后 → 该对象剩下的槽在 deferred 段里
            if t_deferred.is_some() && self.peek() == t_deferred {
                self.byte()?;
                if let Some(l) = self.legacy.as_mut() {
                    l.deferred.push(id);
                }
                return Ok(id);
            }
            let v = self.parse_ref(depth + 1)?;
            // 6.x 的 `*WithSkip` 前缀：先跳过若干**字节**，值才落位
            let extra_bytes = std::mem::take(&mut self.extra_skip);
            // 该项在**对象内**占的字节数（V8 `ReadData` 的 `current` 前进量）
            let n_bytes = match &v {
                SlotValue::Raw(r) => if r.advances { r.len } else { 0 },
                SlotValue::Repeat(n, _) => *n * ts,
                SlotValue::RepeatPrev(n) => *n * ts,
                SlotValue::Skip(bytes) => *bytes,
                SlotValue::StreamBytes(_) => 0,
                _ => ts, // 引用：`current++`
            };
            if let SlotValue::StreamBytes(_) = v {
                // 只吃流、不进对象字节数（如 kOffHeapBackingStore 的内联缓冲）
                continue;
            }
            if let SlotValue::Skip(_) = v {
                // 只跳过、不写值
                consumed_bytes += n_bytes;
                continue;
            }
            // 6.x 的 repeat = "复制前一个槽"：展开成 Repeat(n, 上一个槽的值)
            let v = match v {
                SlotValue::RepeatPrev(n) => {
                    let prev = self.objects[id]
                        .slots
                        .last()
                        .map(|s| s.value.clone())
                        .unwrap_or(SlotValue::ClearedWeak);
                    SlotValue::Repeat(n, Box::new(prev))
                }
                other => other,
            };
            if let SlotValue::PendingRef(pid) = &v {
                let vec_pos = self.objects[id].slots.len();
                self.pending.entry(*pid).or_default().push((id, vec_pos));
            }
            let at = consumed_bytes + extra_bytes;
            self.push_slot(id, at / ts, v);
            consumed_bytes = at + n_bytes;
        }
        if std::env::var("JSCD_DBG_LEGACY").is_ok() {
            eprintln!(
                "[objend] id={id} pos={tag_offset} bytes={consumed_bytes} budget={size_bytes} (words {size_words})"
            );
        }
        if consumed_bytes != size_bytes {
            return Err(format!(
                "object {id} (at payload +{tag_offset}) size mismatch: consumed {consumed_bytes} of {size_bytes} bytes"
            ));
        }
        Ok(id)
    }
}

/// 老族分配器模拟（V8 ≤ 8.4）：payload 前有 reservation 表（每空间若干 chunk 的字节尺寸），
/// 对象按 kNewObject+space 顺序分配，backref 用 (chunk_index, chunk_offset) 指回来。
#[derive(Default)]
struct LegacyAlloc {
    /// [space] → 各 chunk 的字节容量
    chunks: Vec<Vec<u32>>,
    /// [space] → 当前 chunk 下标
    cur: Vec<usize>,
    /// [space] → 当前 chunk 内已用字节
    used: Vec<u32>,
    /// (space, chunk_index, offset) → 对象
    by_addr: std::collections::HashMap<(u8, u32, u32), ObjId>,
    /// MAP 空间按**序数**编号（V8 `SerializerAllocator::AllocateMap` → `num_maps_++`，
    /// 反序列化端 `allocated_maps_[map_index]`）。map 不在 chunk 模拟里。
    maps: Vec<ObjId>,
    /// LO 空间同样按序数编号（`AllocateLargeObject` → `seen_large_objects_index_++`，
    /// 反序列化端 `deserialized_large_objects_[index]`）—— 大对象每个独占一份分配。
    large: Vec<ObjId>,
    /// kAlignmentPrefix 提示的下一次对齐：V8 `AllocationAlignment` 枚举值
    /// （1 = kDoubleAligned、2 = kDoubleUnaligned），0 = 无
    align: u32,
    /// tagged size（压缩指针为 4）：`Heap::GetMaximumFillToAlign` / `GetFillToAlign`
    /// 都要用它（kDoubleSize - kTaggedSize）
    ts: u32,
    /// 内容被延迟的对象（deferred 段按 backref 指回来补全）
    deferred: Vec<ObjId>,
    /// 分配发生顺序（调试用）：(space, chunk, offset, size, id)
    trace: Vec<(u8, u32, u32, u32, ObjId)>,
}

impl LegacyAlloc {
    fn from_reservations(res: &[u32]) -> Self {
        let mut me = LegacyAlloc::default();
        let mut space = 0usize;
        me.chunks.push(Vec::new());
        me.cur.push(0);
        me.used.push(0);
        for r in res {
            let size = r & 0x7fff_ffff;
            me.chunks[space].push(size);
            if r & 0x8000_0000 != 0 {
                // is_last：该空间的 chunk 列表结束
                space += 1;
                me.chunks.push(Vec::new());
                me.cur.push(0);
                me.used.push(0);
            }
        }
        if std::env::var("JSCD_DBG_ALLOC").is_ok() {
            for (s, cs) in me.chunks.iter().enumerate() {
                eprintln!("[res] space={s} chunks={cs:?}");
            }
        }
        me
    }

    /// 分配一个对象：记录地址 → 对象 id，推进 high water。
    ///
    /// 对齐分支照 V8 8.4 `DeserializerAllocator::Allocate`：
    ///   reserved = size + Heap::GetMaximumFillToAlign(alignment)
    ///   address  = AllocateRaw(space, reserved)      // 水位按 **reserved** 前进
    ///   obj      = Heap::AlignWithFiller(obj, size, reserved, alignment)
    /// `AlignWithFiller` 把 `pre_filler = Heap::GetFillToAlign(address, alignment)` 字节放在
    /// 对象**前面**（对象地址 = address + pre_filler），余下的 `reserved - pre_filler` 放在
    /// 对象**后面**。旧实现只把游标 pad 到对齐、再 += size —— 少了 filler 记账，
    /// 之后每个对象的地址都偏（space2 在 1008 与 1136 之间就少了那个 128 字节的对象，
    /// backref 因此命不中）。
    fn allocate(&mut self, space: u8, size: u32, id: ObjId) -> Option<(u32, u32)> {
        // MAP(4)/LO(5) 走序数编号，与 chunk 模拟无关，先记账（V8 里它们由
        // AllocateMap / AllocateLargeObject 单独计数）。
        if space == 4 {
            self.maps.push(id);
        } else if space == 5 {
            self.large.push(id);
        }
        let s = space as usize;
        if s >= self.cur.len() {
            return None;
        }
        let idx = self.cur[s] as u32;
        let mut off = self.used[s];
        let mut advance = size;
        if self.align != 0 {
            let pre = fill_to_align(self.used[s], self.align, self.ts);
            off = self.used[s] + pre;
            advance = size + max_fill_to_align(self.ts);
            self.align = 0;
        }
        if std::env::var("JSCD_DBG_ALLOC").is_ok() {
            eprintln!(
                "[alloc] space={space} size={size} id={id} chunk={idx} off={off} \
                 pre={} advance={advance} align={}",
                off - (self.used[s]),
                self.align
            );
        }
        self.by_addr.insert((space, idx, off), id);
        self.trace.push((space, idx, off, size, id));
        self.used[s] += advance;
        Some((idx, off))
    }

    fn next_chunk(&mut self, space: u8) {
        let s = space as usize;
        if s < self.cur.len() {
            self.cur[s] += 1;
            self.used[s] = 0;
        }
    }

    /// MAP(4)/LO(5) 空间的 backref：V8 只写**一个序数**（`PutBackReference` 的
    /// `kMap`/`kLargeObject` 分支各写 map_index / large_object_index），不是
    /// (chunk, offset) 对。按分配序数取。
    fn resolve_ordinal(&self, space: u8, index: usize) -> Option<ObjId> {
        match space {
            4 => self.maps.get(index).copied(),
            5 => self.large.get(index).copied(),
            _ => None,
        }
    }

    /// backref 解析：对应 V8 `DeserializerAllocator::GetObject` ——
    /// 若此刻还有 pending 的对齐（deferred 对象正好在被对齐的那个位置），
    /// 记录下来的 offset 是 filler 之前的地址，要加上 padding 才是对象本身。
    fn resolve(&self, space: u8, chunk: u32, offset: u32) -> Option<ObjId> {
        if let Some(id) = self.by_addr.get(&(space, chunk, offset)) {
            return Some(*id);
        }
        if self.align != 0 {
            let pad = fill_to_align(offset, self.align, self.ts);
            if pad != 0 {
                if let Some(id) = self.by_addr.get(&(space, chunk, offset + pad)) {
                    return Some(*id);
                }
            }
        }
        None
    }
}

/// V8 8.4 `Heap::GetMaximumFillToAlign`：kDoubleAligned/kDoubleUnaligned 都是
/// `kDoubleSize - kTaggedSize`（kDoubleSize = 8）。
fn max_fill_to_align(ts: u32) -> u32 {
    8u32.saturating_sub(ts)
}

/// V8 8.4 `Heap::GetFillToAlign`：
///   kDoubleAligned(1)  ：地址未 8 对齐 → 补 kTaggedSize
///   kDoubleUnaligned(2)：地址**已** 8 对齐 → 补 kDoubleSize - kTaggedSize（让双精度值尾部对齐）
/// 其余 0。注意 64 位非压缩（ts=8）时两者恒为 0 —— 也就是那种构建里根本不会出现对齐前缀。
fn fill_to_align(addr: u32, alignment: u32, ts: u32) -> u32 {
    if alignment == 1 && addr & 7 != 0 {
        ts
    } else if alignment == 2 && addr & 7 == 0 {
        8u32.saturating_sub(ts)
    } else {
        0
    }
}

/// 解析整个 payload（已解压、已去头）。
pub fn parse<'a>(payload: &'a [u8], table: &VersionTable) -> R<CodeCache<'a>> {
    parse_with(payload, table, &[])
}

/// `reservations`：老族（≤8.4）payload 前的 reservation 表（每空间若干 chunk 的尺寸，
/// 低 31 位是尺寸、最高位是该空间最后一块）。现代族传空。
pub fn parse_with<'a>(
    payload: &'a [u8],
    table: &VersionTable,
    reservations: &[u32],
) -> R<CodeCache<'a>> {
    // 合并 tag 表：现代族用 `tags`，老族（≤8.4）的标签值在 `legacy` 里（`tags` 为空）
    let mut merged_map: std::collections::HashMap<String, u8> = table.serialization.tags.clone();
    // 老族（≤8.4）以 legacy 表为准：源码树里同时存在现代枚举，tags 会串味
    for (k, v) in &table.serialization.legacy {
        // 只并进 u8 能表示的标签；≥256 的是普通常量（kInstanceTypes 之类），
        // 由 `lnum()` 直接查 legacy 表。
        if let Ok(v8) = u8::try_from(*v) {
            merged_map.insert(k.clone(), v8);
        }
    }
    let merged_tags: &'static std::collections::HashMap<String, u8> =
        Box::leak(Box::new(merged_map));
    // 最小必需集：跨族恒存在（8.x 无 kReadOnlyHeapRef、11.3 起空间数变化等）
    for n in [
        "kNewObject",
        "kBackref",
        "kRootArray",
        "kAttachedReference",
        "kSynchronize",
        "kNop",
        "kHotObject",
        "kRootArrayConstants",
        "kFixedRawData",
    ] {
        if !merged_tags.contains_key(n) {
            return Err(format!("table missing serialization tag {n}"));
        }
    }
    let ts = table.tagged_size as usize;
    let legacy_ctx = if table.serialization.legacy.contains_key("kSpaceMask") {
        let mut l = LegacyAlloc::from_reservations(reservations);
        l.ts = ts as u32;
        Some(l)
    } else {
        None
    };
    let mut w = Walker {
        data: payload,
        pos: 0,
        tagged_size: ts,
        tags: merged_tags,
        legacy_consts: &table.serialization.legacy,
        legacy: legacy_ctx,
        objects: Vec::new(),
        hot: HotRing::default(),
        pending: std::collections::HashMap::new(),
        extra_skip: 0,
        table,
    };
    if std::env::var("JSCD_DBG_LEGACY").is_ok() {
        eprintln!(
            "[table] v8={} six_x={} spaces={} map={} lo={} ts={}",
            table.v8,
            w.is_six_x(),
            w.n_spaces(),
            w.map_space(),
            w.lo_space(),
            ts
        );
    }
    let b = w.byte()?;
    let t_new = w.tag("kNewObject")?;
    if !(t_new..w.tag("kBackref")?).contains(&b) {
        return Err(format!("payload does not start with kNewObject (got 0x{b:02x})"));
    }
    // 老族：**首个对象也带 space 编号**（V8 常放在 kCode/kOld，不是 0）。
    // 早先按 0 处理会把它错放进别的空间的 chunk 里，于是那个空间的地址整体
    // 偏移一个对象 —— space2 的 backref (2,0,472) 就是这么命不中的
    // （V8 的 472 正好等于我们坐标里的 416，差的正是首个对象的 56 字节）。
    let top_space = if w.legacy.is_some() { b - t_new } else { 0 };
    let top = w.parse_new_object_space(0, top_space)?;
    let t_sync = w.tag("kSynchronize")?;
    if std::env::var("JSCD_DBG_LEGACY").is_ok() {
        let tail = w.data.get(w.pos..(w.pos + 24).min(w.data.len())).unwrap_or(&[]);
        eprintln!("[legacy] 主段结束 pos={} 后续字节={:02x?}", w.pos, tail);
    }
    // 老族的 deferred 段（条目/backref 编码）还没完全对齐：主 payload 已完整，
    // 缺的只是"被延迟的对象内容"，不值得让整份解析失败 —— 记一笔后停下。
    let legacy_tolerant = w.legacy.is_some();
    loop {
        match w.peek() {
            None => {
                if legacy_tolerant {
                    break;
                }
                return Err("payload ended before kSynchronize".into());
            }
            Some(b) if b == t_sync => {
                w.byte()?;
                break;
            }
            Some(_) => {
                let before = w.pos;
                let outcome: R<()> = (|| {
                    let Some(b) = w.peek() else { return Ok(()) };
                    // 对齐前缀是循环内的一等条目（V8 `DeserializeDeferredObjects` 的
                    // `case kAlignmentPrefix`）：要就地消费，不能交给 parse_ref ——
                    // 那样会递归着把后面的 tag 当成"新建对象"，backref/size 全错位。
                    if let Some(align) = w.opt_tag("kAlignmentPrefix") {
                        if (align..align + 3).contains(&b) {
                            let a = (b - align + 1) as u32;
                            if let Some(l) = w.legacy.as_mut() {
                                l.align = a;
                            }
                            w.byte()?;
                            return Ok(());
                        }
                    }
                    let t_new = w.tag("kNewObject")?;
                    let n_sp = w.n_spaces();
                    if legacy_tolerant && (t_new..t_new + n_sp).contains(&b) {
                        // 老族 deferred 条目（V8 `DeserializeDeferredObjects`）：
                        //   kNewObject+space | backref | size | 剩余槽
                        // backref 的**编码随版本**：6.x 是一个位域 uint32（空间号在标签里），
                        // 7.8/8.4 才是 (chunk, offset) 两段 —— 早先这里写死两段，
                        // 6.x 的 deferred 段一律解不出（node8 的四个 fixture 就卡在这）。
                        let space = (b - t_new) as u8;
                        w.byte()?;
                        let id = match w.legacy_backref(space)? {
                            SlotValue::Ref(Ref::Object(id)) => id,
                            other => {
                                return Err(format!("deferred backref 不是对象引用: {other:?}"))
                            }
                        };
                        let size_words = w.putint()? as usize;
                        let mut slot_index = w.objects[id].slots.len().max(1);
                        let mut consumed = 1usize;
                        while consumed < size_words {
                            let v = w.parse_ref(1)?;
                            let ts = w.tagged_size;
                            let n_bytes = match &v {
                                SlotValue::Raw(r) => if r.advances { r.len } else { 0 },
                                SlotValue::Repeat(n, _) => *n * ts,
                                SlotValue::RepeatPrev(n) => *n * ts,
                                SlotValue::Skip(bytes) => *bytes,
                                SlotValue::StreamBytes(_) => 0,
                                _ => ts,
                            };
                            if let SlotValue::Skip(_) = v {
                                consumed += n_bytes;
                                continue;
                            }
                            let n_slots = n_bytes / ts.max(1);
                            for k in 0..n_slots.max(1) {
                                let val = match &v {
                                    SlotValue::Repeat(_, inner) => (**inner).clone(),
                                    other => other.clone(),
                                };
                                w.push_slot(id, slot_index + k, val);
                            }
                            slot_index += n_slots.max(1);
                            consumed += n_bytes.max(ts);
                        }
                        return Ok(());
                    }
                    w.parse_ref(0)?;
                    Ok(())
                })();
                match outcome {
                    Ok(()) => {}
                    Err(e) if legacy_tolerant => {
                        eprintln!("jscd: 老族 deferred 段停在 offset {}: {e}", w.pos);
                        break;
                    }
                    Err(e) => return Err(format!("deferred section: {e}")),
                }
                if w.pos == before {
                    if legacy_tolerant {
                        break;
                    }
                    return Err("deferred section made no progress".into());
                }
            }
        }
    }
    let t_nop = w.tag("kNop")?;
    while let Some(b) = w.peek() {
        if b != t_nop {
            return Err(format!(
                "unexpected byte 0x{b:02x} after kSynchronize at {}",
                w.pos
            ));
        }
        w.byte()?;
    }
    classify(&mut w.objects, table, ts);
    if let Ok(want) = std::env::var("JSCD_DBG_EMPTY") {
        let range = want.parse::<usize>().ok();
        for (i, o) in w.objects.iter().enumerate() {
            let show = match range {
                None => o.slots.is_empty(),
                Some(base) => i >= base && i < base + 12,
            };
            if !show {
                continue;
            }
            let mut s = Vec::new();
            for sl in o.slots.iter() {
                s.push(match &sl.value {
                    SlotValue::Ref(r) => format!("{}:ref{:?}", sl.index, r),
                    SlotValue::Raw(r) => format!("{}:raw{}", sl.index, r.len),
                    SlotValue::Repeat(n, _) => format!("{}:rep{}", sl.index, n),
                    SlotValue::PendingRef(p) => format!("{}:pend{}", sl.index, p),
                    other => format!("{}:{other:?}", sl.index),
                });
            }
            eprintln!(
                "[empty] id={i} ty={:?} bytes={} tag_pos={} slots={s:?}",
                o.ty, o.byte_size, o.start_offset
            );
        }
    }
    Ok(CodeCache {
        payload,
        tagged_size: ts,
        objects: w.objects,
        top_sfi: top,
    })
}

/// 类型分类：map 引用 → roots 名 / Map 对象 / 结构指纹兜底（三段式，避免借用冲突）。
fn classify(objects: &mut [Object], table: &VersionTable, _ts: usize) {
    let n = objects.len();
    let inst_types: Vec<u16> = objects.iter().map(instance_type_from_slots).collect();
    let mut tys: Vec<Ty> = Vec::with_capacity(n);
    for obj in objects.iter() {
        let mut ty = match obj.slots.first().and_then(|s| s.value.as_ref()) {
            Some(Ref::Root(i)) => {
                let name = table.root_name(i).unwrap_or("");
                if name.ends_with("Map") {
                    Ty::Root(i as u16)
                } else if name.starts_with("String:") {
                    Ty::Str(StrKind::OneByte)
                } else {
                    Ty::Unknown
                }
            }
            Some(Ref::Object(m)) if m < n => Ty::MapObj(inst_types[m]),
            Some(Ref::RoRef(c, o)) => Ty::Ro(c, o),
            _ => Ty::Unknown,
        };
        if let Ty::Root(i) = ty {
            let name = table.root_name(i as usize).unwrap_or("");
            if name.contains("String") {
                ty = Ty::Str(if name.contains("OneByte") {
                    StrKind::OneByte
                } else {
                    StrKind::TwoByte
                });
            }
        }
        if matches!(ty, Ty::Unknown | Ty::Root(_)) {
            if let Some(t) = structural_type(obj) {
                ty = Ty::Structural(t);
            }
        }
        tys.push(ty);
    }
    for (obj, ty) in objects.iter_mut().zip(tys) {
        obj.ty = ty;
    }
}

/// Map 对象 body 的 raw chunk 是否覆盖 instance_type（@12..14）。
fn instance_type_from_slots(map: &Object) -> u16 {
    for s in &map.slots {
        if let SlotValue::Raw(r) = &s.value {
            let start = s.index * 8;
            if start <= 12 && start + r.len >= 14 {
                return 1; // 标记"含 instance_type"，具体值由 CodeCache::map_instance_type 惰性读
            }
        }
    }
    0
}

/// 结构指纹分类（roots 表解析不到时的兜底）。
fn structural_type(o: &Object) -> Option<&'static str> {
    // Script：source 槽恒为 attached ref 0（源码占位），对象较大
    if o.byte_size >= 96
        && matches!(
            o.slots.get(1),
            Some(Slot {
                value: SlotValue::Ref(Ref::Attached(0)),
                ..
            })
        )
    {
        return Some("Script");
    }
    // UncompiledData：map + inferred_name(ptr) + start/end
    if o.slots.len() == 3 {
        let s1_ref = matches!(o.slots.get(1), Some(Slot { value: SlotValue::Ref(_), .. }));
        let s2_raw = matches!(
            o.slots.get(2),
            Some(Slot { value: SlotValue::Raw(r), .. }) if r.len == 8
        );
        if s1_ref && s2_raw && o.byte_size == 32 {
            return Some("UncompiledData");
        }
    }
    None
}

impl<'a> CodeCache<'a> {
    /// Map 对象的 instance_type（@12..14，惰性读 payload）。
    pub fn map_instance_type(&self, map_id: ObjId) -> Option<u16> {
        let ts = self.tagged_size;
        let d = self.raw_at(map_id, 12, 2)?;
        let _ = ts;
        Some(u16::from_le_bytes(d.try_into().ok()?))
    }

    #[inline]
    pub fn obj(&self, id: ObjId) -> &Object {
        &self.objects[id]
    }
    #[inline]
    pub fn payload(&self) -> &'a [u8] {
        self.payload
    }
    #[inline]
    pub fn tagged_size(&self) -> usize {
        self.tagged_size
    }
    /// raw chunk 字节（零拷贝）。
    #[inline]
    pub fn raw_bytes(&self, r: Raw) -> &'a [u8] {
        &self.payload[r.off..r.off + r.len]
    }
    #[inline]
    pub fn ref_object(&self, r: Ref) -> Option<ObjId> {
        match r {
            Ref::Object(id) => Some(id),
            _ => None,
        }
    }

    /// 按槽号取槽值（slots 升序 → 二分）。
    #[inline]
    pub fn slot_at(&self, id: ObjId, index: usize) -> Option<&SlotValue> {
        let slots = &self.objects[id].slots;
        let i = slots.partition_point(|s| s.index < index);
        slots.get(i).filter(|s| s.index == index).map(|s| &s.value)
    }

    /// 按对象内字节偏移取槽值（含 raw/repeat 覆盖范围）。
    pub fn unit_at_ts(&self, id: ObjId, byte_off: usize, ts: usize) -> Option<&SlotValue> {
        let want = byte_off / ts;
        let slots = &self.objects[id].slots;
        let mut i = slots.partition_point(|s| s.index <= want);
        while i > 0 {
            i -= 1;
            let s = &slots[i];
            let end = match &s.value {
                SlotValue::Raw(r) => s.index + r.len / ts,
                SlotValue::Repeat(n, _) => s.index + n,
                _ => s.index + 1,
            };
            if want >= s.index && want < end {
                return Some(&s.value);
            }
            if s.index + 4 < want {
                break;
            }
        }
        None
    }

    #[inline]
    pub fn unit_at(&self, id: ObjId, byte_off: usize) -> Option<&SlotValue> {
        self.unit_at_ts(id, byte_off, self.tagged_size)
    }

    /// 按对象内字节偏移取 raw 字节（不跨 chunk 拼接）。
    pub fn raw_at_ts(&self, id: ObjId, byte_off: usize, len: usize, ts: usize) -> Option<&'a [u8]> {
        let want = byte_off / ts;
        let slots = &self.objects[id].slots;
        let mut i = slots.partition_point(|s| s.index <= want);
        while i > 0 {
            i -= 1;
            let s = &slots[i];
            if let SlotValue::Raw(r) = &s.value {
                let start = s.index * ts;
                if byte_off >= start && byte_off + len <= start + r.len {
                    let rel = byte_off - start;
                    return Some(&self.payload[r.off + rel..r.off + rel + len]);
                }
            }
            if s.index + 4 < want {
                break;
            }
        }
        None
    }

    #[inline]
    pub fn raw_at(&self, id: ObjId, byte_off: usize, len: usize) -> Option<&'a [u8]> {
        self.raw_at_ts(id, byte_off, len, self.tagged_size)
    }

    /// FixedArray 长度（length 槽在 offset = ts）。
    pub fn array_len(&self, id: ObjId) -> usize {
        let ts = self.tagged_size;
        self.raw_at(id, ts, ts)
            .and_then(decode_smi_bytes)
            .map(|v| v as usize)
            .unwrap_or(0)
    }

    /// FixedArray 第 index 个元素（元素从 offset = 2*ts 开始）。
    pub fn array_elem(&self, id: ObjId, index: usize) -> Option<Elem<'a>> {
        let ts = self.tagged_size;
        let off = (2 + index) * ts;
        let want = off / ts;
        let slots = &self.objects[id].slots;
        let mut i = slots.partition_point(|s| s.index <= want);
        while i > 0 {
            i -= 1;
            let s = &slots[i];
            match &s.value {
                SlotValue::Ref(r) => {
                    if s.index == want {
                        return Some(Elem::Ref(*r));
                    }
                }
                SlotValue::Raw(r) => {
                    let start = s.index * ts;
                    if off >= start && off + ts <= start + r.len {
                        let rel = off - start;
                        let d = &self.payload[r.off + rel..r.off + rel + ts];
                        return Some(match decode_smi_bytes(d) {
                            Some(v) => Elem::Smi(v),
                            None => Elem::Bytes(d),
                        });
                    }
                }
                SlotValue::Repeat(n, inner) => {
                    let start = s.index * ts;
                    if off >= start && off < start + n * ts {
                        return match inner.as_ref() {
                            SlotValue::Ref(r) => Some(Elem::Ref(*r)),
                            SlotValue::Raw(r) => {
                                let d = &self.payload[r.off..r.off + r.len.min(ts)];
                                Some(decode_smi_bytes(d).map(Elem::Smi).unwrap_or(Elem::Bytes(d)))
                            }
                            _ => None,
                        };
                    }
                }
                _ => {}
            }
            if s.index + 4 < want {
                break;
            }
        }
        None
    }
}

/// Smi 解码：无压缩（ts=8）值在高 32 位且低半为 0；压缩（ts=4）低 32 位带 tag。
#[inline]
pub fn decode_smi_bytes(d: &[u8]) -> Option<i64> {
    match d.len() {
        8 => {
            let raw = u64::from_le_bytes(d.try_into().ok()?);
            if raw & 0xFFFF_FFFF == 0 {
                Some(((raw >> 32) as u32 as i32) as i64)
            } else {
                None
            }
        }
        4 => {
            let raw = u32::from_le_bytes(d.try_into().ok()?);
            if raw & 1 == 0 {
                None
            } else {
                Some(((raw as i32) >> 1) as i64)
            }
        }
        _ => None,
    }
}