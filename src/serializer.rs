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
            Ty::Root(i) => match table.roots.get(*i as usize) {
                Some(n) => Cow::Borrowed(n.as_str()),
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
        base == want
            || base
                .strip_suffix("Map")
                .map(|s| s == want)
                .unwrap_or(false)
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
    objects: Vec<Object>,
    hot: HotRing,
    pending: std::collections::HashMap<u32, Vec<(ObjId, usize)>>,
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
        };
        self.pos += n;
        Ok(r)
    }
    #[inline]
    fn push_slot(&mut self, id: ObjId, index: usize, value: SlotValue) {
        self.objects[id].slots.push(Slot { index, value });
    }

    fn parse_ref(&mut self, depth: usize) -> R<SlotValue> {
        if depth > 64 {
            return Err("ref recursion too deep".into());
        }
        let b = self.byte()?;
        let t_new = self.tag("kNewObject")?;
        let t_backref = self.tag("kBackref")?;
        let t_hot = self.tag("kHotObject")?;
        let t_root_const = self.tag("kRootArrayConstants")?;
        let t_fixed_raw = self.tag("kFixedRawData")?;
        let t_fixed_repeat = self.tag_any(&["kFixedRepeat", "kFixedRepeatRoot"]);
        let t_fixed_repeat_root_only =
            self.opt_tag("kFixedRepeat").is_none() && self.opt_tag("kFixedRepeatRoot").is_some();

        if (t_new..t_backref).contains(&b) {
            let id = self.parse_new_object(depth)?;
            Ok(SlotValue::Ref(Ref::Object(id)))
        } else if b == t_backref {
            let idx = self.putint()? as usize;
            if idx >= self.objects.len() {
                return Err(format!("backref {idx} out of range ({})", self.objects.len()));
            }
            self.hot.add(HotEntry::Object(idx)); // PutBackReference 会入 hot 环
            Ok(SlotValue::Ref(Ref::Object(idx)))
        } else if (t_hot..t_hot + 8).contains(&b) {
            let i = (b - t_hot) as usize;
            match self.hot.get(i) {
                Some(HotEntry::Object(id)) => Ok(SlotValue::Ref(Ref::Object(id))),
                Some(HotEntry::Root(r)) => Ok(SlotValue::Ref(Ref::Root(r))),
                None => Err(format!("hot object index {i} out of ring")),
            }
        } else if b == self.tag("kRootArray")? {
            let idx = self.putint()? as usize;
            self.hot.add(HotEntry::Root(idx));
            Ok(SlotValue::Ref(Ref::Root(idx)))
        } else if (t_root_const..t_root_const + 32).contains(&b) {
            Ok(SlotValue::Ref(Ref::Root((b - t_root_const) as usize)))
        } else if b == self.tag("kReadOnlyHeapRef")? {
            let c = self.putint()?;
            let o = self.putint()?;
            Ok(SlotValue::Ref(Ref::RoRef(c, o)))
        } else if b == self.tag("kAttachedReference")? {
            Ok(SlotValue::Ref(Ref::Attached(self.putint()? as usize)))
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
            Ok(SlotValue::Raw(self.raw(n * self.tagged_size)?))
        } else if Some(b) == self.opt_tag("kVariableRawData") {
            let n = self.putint()? as usize;
            Ok(SlotValue::Raw(self.raw(n * self.tagged_size)?))
        } else if t_fixed_repeat.map_or(false, |t| (t..t + 16).contains(&b)) {
            let base = t_fixed_repeat.unwrap();
            let n = (b - base + 2) as usize;
            if t_fixed_repeat_root_only {
                let root = self.byte()? as usize;
                Ok(SlotValue::Repeat(n, Box::new(SlotValue::Ref(Ref::Root(root)))))
            } else {
                let inner = self.parse_ref(depth + 1)?;
                Ok(SlotValue::Repeat(n, Box::new(inner)))
            }
        } else if Some(b) == self.tag_any(&["kVariableRepeat"]) {
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

    fn parse_new_object(&mut self, depth: usize) -> R<ObjId> {
        let tag_offset = self.pos.saturating_sub(1);
        let size_words = self.putint()? as usize;
        let byte_size = size_words * 8;
        let id = self.objects.len();
        self.objects.push(Object {
            ty: Ty::Pending,
            byte_size,
            start_offset: tag_offset,
            slots: Vec::with_capacity(size_words.min(64)),
        });
        let map = self.parse_ref(depth + 1)?;
        self.push_slot(id, 0, map);
        let t_resolve = self.tag("kResolvePendingForwardRef")?;
        loop {
            match self.peek() {
                Some(b) if b == t_resolve => {
                    self.byte()?;
                    let pid = self.putint()?;
                    if let Some(list) = self.pending.get_mut(&pid) {
                        for (oid, vec_pos) in list.drain(..) {
                            self.objects[oid].slots[vec_pos].value = SlotValue::Ref(Ref::Object(id));
                        }
                    }
                    self.pending.remove(&pid);
                }
                _ => break,
            }
        }
        let mut consumed = 1usize;
        let mut slot_index = 1usize;
        while consumed < size_words {
            let v = self.parse_ref(depth + 1)?;
            let n_slots = match &v {
                SlotValue::Raw(r) => r.len / self.tagged_size,
                SlotValue::Repeat(n, _) => *n,
                _ => 1,
            };
            if let SlotValue::PendingRef(pid) = &v {
                let vec_pos = self.objects[id].slots.len();
                self.pending.entry(*pid).or_default().push((id, vec_pos));
            }
            self.push_slot(id, slot_index, v);
            consumed += n_slots;
            slot_index += n_slots;
        }
        if consumed != size_words {
            return Err(format!(
                "object {id} (at payload +{tag_offset}) size mismatch: consumed {consumed} of {size_words} words"
            ));
        }
        Ok(id)
    }
}

/// 解析整个 payload（已解压、已去头）。
pub fn parse<'a>(payload: &'a [u8], table: &VersionTable) -> R<CodeCache<'a>> {
    for n in [
        "kNewObject",
        "kBackref",
        "kRootArray",
        "kReadOnlyHeapRef",
        "kAttachedReference",
        "kSynchronize",
        "kNop",
        "kHotObject",
        "kRootArrayConstants",
        "kFixedRawData",
    ] {
        if !table.serialization.tags.contains_key(n) {
            return Err(format!("table missing serialization tag {n}"));
        }
    }
    let ts = table.tagged_size as usize;
    let mut w = Walker {
        data: payload,
        pos: 0,
        tagged_size: ts,
        tags: &table.serialization.tags,
        objects: Vec::new(),
        hot: HotRing::default(),
        pending: std::collections::HashMap::new(),
    };
    let b = w.byte()?;
    let t_new = w.tag("kNewObject")?;
    if !(t_new..w.tag("kBackref")?).contains(&b) {
        return Err(format!("payload does not start with kNewObject (got 0x{b:02x})"));
    }
    let top = w.parse_new_object(0)?;
    let t_sync = w.tag("kSynchronize")?;
    loop {
        match w.peek() {
            None => return Err("payload ended before kSynchronize".into()),
            Some(b) if b == t_sync => {
                w.byte()?;
                break;
            }
            Some(_) => {
                let before = w.pos;
                w.parse_ref(0).map_err(|e| format!("deferred section: {e}"))?;
                if w.pos == before {
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
                let name = table.roots.get(i).map(|s| s.as_str()).unwrap_or("");
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
            let name = table.roots.get(i as usize).map(|s| s.as_str()).unwrap_or("");
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