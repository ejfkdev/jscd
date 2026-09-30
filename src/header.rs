//! `.jsc` 头部：SerializedCodeData（布局随 V8 版本可配）+ Brotli 嗅探。
//!
//! 两种实测布局（由 codegen 从各版本 code-serializer.h 提取）：
//! ```text
//! V8 ≤ 11.x（24 字节）              V8 ≥ 12.x（28 字节）
//! [0]  magic                       [0]  magic
//! [4]  version_hash                [4]  version_hash
//! [8]  source_hash                 [8]  source_hash
//! [12] flag_hash                   [12] flag_hash
//! [16] payload_len                 [16] ro_checksum（只读快照校验和）
//! [20] checksum (Adler-32)         [20] payload_len
//!                                  [24] checksum (Adler-32)
//! ```
//! magic = 0xC0DE0000 ^ ExternalReferenceTable::kSize，低 16 位随构建变化。
//! source_hash = 原源码长度 | (is_module << 31)。

use serde::{Deserialize, Serialize};
use std::path::Path;

/// 每版本头布局（来自 codegen 提取的 code-serializer.cc 常量）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeaderLayout {
    pub version_hash: usize,
    pub source_hash: usize,
    pub flag_hash: usize,
    /// V8 9.x 起存在；旧版本为 None
    pub read_only_checksum: Option<usize>,
    pub payload_length: usize,
    pub checksum: usize,
    /// 头部总字节数（payload 起始偏移，未计预留表/桩键）
    pub header_size: usize,
    /// `kNumReservationsOffset`：预留表条数所在的头字段偏移（6.x/7.x/8.x = 20/16/16…）
    #[serde(default)]
    pub num_reservations: Option<usize>,
    /// `kNumCodeStubKeysOffset`：桩键条数（6.x 专有；表/键都占 4 字节）
    #[serde(default)]
    pub num_stub_keys: Option<usize>,
}

impl Default for HeaderLayout {
    /// V8 ≤ 11.x 的 24 字节布局（锚点版本族）；新版本布局由版本表覆盖。
    fn default() -> Self {
        HeaderLayout {
            version_hash: 4,
            source_hash: 8,
            flag_hash: 12,
            read_only_checksum: None,
            payload_length: 16,
            checksum: 20,
            header_size: 24,
            num_reservations: None,
            num_stub_keys: None,
        }
    }
}

/// 解析后的头部字段。
#[derive(Debug, Clone)]
pub struct Header {
    pub raw_size: usize,
    pub decompressed_size: usize,
    pub brotli: bool,
    pub magic: u32,
    pub version_hash: u32,
    pub source_hash: u32,
    pub flag_hash: u32,
    pub read_only_checksum: Option<u32>,
    pub payload_length: u32,
    pub checksum: u32,
    pub layout: HeaderLayout,
    /// payload 起始字节偏移（= header_size，解压后坐标）
    pub payload_offset: usize,
}

impl Header {
    pub fn parse(data: &[u8]) -> Result<Header, String> {
        Self::parse_with(data, &HeaderLayout::default())
    }

    pub fn parse_with(data: &[u8], layout: &HeaderLayout) -> Result<Header, String> {
        let brotli = !is_code_cache(data);
        let raw_size = data.len();
        let buf: Vec<u8>;
        let data: &[u8] = if brotli {
            buf = brotli_decompress(data)?;
            &buf
        } else {
            data
        };
        let decompressed_size = data.len();
        let need = layout.header_size;
        if data.len() < need {
            return Err(format!("file too small for a {need}-byte code cache header: {} bytes", data.len()));
        }
        let u32_at = |off: usize| -> Result<u32, String> {
            let b = data
                .get(off..off + 4)
                .ok_or_else(|| format!("header truncated at offset {off}"))?;
            Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        let magic = u32_at(0)?;
        // 魔数上 16 位恒为 0xC0DE（低 16 位是 ExternalReferenceTable::kSize，随构建变化，不作精确校验）
        if (magic & 0xFFFF_0000) != 0xC0DE_0000 {
            return Err(format!(
                "bad magic {magic:#010x}: not a V8 code cache (expected 0xC0DE????)"
            ));
        }
        let read_only_checksum = layout
            .read_only_checksum
            .map(|off| u32_at(off))
            .transpose()?;
        let payload_offset = layout.header_size;
        Ok(Header {
            raw_size,
            decompressed_size,
            brotli,
            magic,
            version_hash: u32_at(layout.version_hash)?,
            source_hash: u32_at(layout.source_hash)?,
            flag_hash: u32_at(layout.flag_hash)?,
            read_only_checksum,
            payload_length: u32_at(layout.payload_length)?,
            checksum: u32_at(layout.checksum)?,
            layout: layout.clone(),
            payload_offset,
        })
    }

    /// source_hash 的真身：`SourceHash() = source->length() | (is_module << 31)`。
    pub fn source_length(&self) -> u32 {
        self.source_hash & 0x7FFF_FFFF
    }
    pub fn is_module(&self) -> bool {
        self.source_hash >> 31 == 1
    }
    /// 魔数低 16 位：ExternalReferenceTable::kSize（随 V8 版本/构建变化）。
    pub fn external_ref_table_size(&self) -> u32 {
        self.magic & 0xFFFF
    }

    pub fn payload<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        let start = self.payload_offset;
        let end = if self.payload_length > 0 && start + self.payload_length as usize <= data.len() {
            start + self.payload_length as usize
        } else {
            data.len()
        };
        data.get(start..end).unwrap_or(&[])
    }
}

/// 文件是否直接是 code cache（魔数上 16 位 0xC0DE），否则尝试按 Brotli 解压。
pub fn is_code_cache(data: &[u8]) -> bool {
    data.len() >= 4
        && u32::from_le_bytes([data[0], data[1], data[2], data[3]]) & 0xFFFF_0000 == 0xC0DE_0000
}

/// bytenode --compress 产物：整个文件是一条 Brotli 流（无魔数，识别靠解压回退）。
pub fn maybe_brotli_decompress(raw: Vec<u8>) -> Vec<u8> {
    if is_code_cache(&raw) {
        return raw;
    }
    match brotli_decompress(&raw) {
        Ok(v) => v,
        // 解压失败则原样返回，让上层报出可理解的魔数错误
        Err(_) => raw,
    }
}

fn brotli_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(data.len() * 2);
    let mut reader = brotli::Decompressor::new(data, 4096);
    std::io::Read::read_to_end(&mut reader, &mut out)
        .map_err(|e| format!("brotli decompress failed: {e}"))?;
    if out.is_empty() {
        return Err("brotli decompress produced no data".into());
    }
    Ok(out)
}

impl Header {
    pub fn render_text(&self, file: &Path, ident: &crate::tables::Identified) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        let ro = self
            .read_only_checksum
            .map(|v| format!("{v:#010x}"))
            .unwrap_or_else(|| "(absent)".into());
        let _ = writeln!(s, "file:          {}", file.display());
        let _ = writeln!(
            s,
            "size:          {} bytes (raw) / {} bytes (decompressed)",
            self.raw_size, self.decompressed_size
        );
        let _ = writeln!(s, "brotli:        {}", if self.brotli { "yes" } else { "no" });
        let _ = writeln!(
            s,
            "magic:         {:#010x} (external refs: {:#x})",
            self.magic,
            self.external_ref_table_size()
        );
        let _ = writeln!(s, "version_hash:  {:#010x}", self.version_hash);
        let _ = writeln!(
            s,
            "source_hash:   {:#010x} (source length: {} bytes, module: {})",
            self.source_hash,
            self.source_length(),
            if self.is_module() { "yes" } else { "no" }
        );
        let _ = writeln!(s, "flag_hash:     {:#010x}", self.flag_hash);
        let _ = writeln!(s, "ro_checksum:   {ro}");
        let _ = writeln!(s, "payload:       {} bytes at offset {}", self.payload_length, self.payload_offset);
        let _ = writeln!(s, "checksum:      {:#010x}", self.checksum);
        let _ = write!(s, "{}", ident.render_text());
        s
    }

    pub fn render_json(&self, file: &Path, ident: &crate::tables::Identified) -> String {
        #[derive(Serialize)]
        struct InfoOut<'a> {
            file: String,
            size_raw: usize,
            size_decompressed: usize,
            brotli: bool,
            magic: u32,
            external_ref_table_size: u32,
            version_hash: u32,
            source_hash: u32,
            source_length: u32,
            is_module: bool,
            flag_hash: u32,
            read_only_checksum: Option<u32>,
            payload_length: u32,
            checksum: u32,
            node: &'a str,
            v8: &'a str,
            identification: &'a str,
        }
        let out = InfoOut {
            file: file.display().to_string(),
            size_raw: self.raw_size,
            size_decompressed: self.decompressed_size,
            brotli: self.brotli,
            magic: self.magic,
            external_ref_table_size: self.external_ref_table_size(),
            version_hash: self.version_hash,
            source_hash: self.source_hash,
            source_length: self.source_length(),
            is_module: self.is_module(),
            flag_hash: self.flag_hash,
            read_only_checksum: self.read_only_checksum,
            payload_length: self.payload_length,
            checksum: self.checksum,
            node: ident.node,
            v8: ident.v8,
            identification: ident.confidence,
        };
        serde_json::to_string_pretty(&out).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_dummy(vh: u32, sh: u32, fh: u32, pl: u32, ck: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&0xC0DE_0000u32.to_le_bytes());
        v.extend_from_slice(&vh.to_le_bytes());
        v.extend_from_slice(&sh.to_le_bytes());
        v.extend_from_slice(&fh.to_le_bytes());
        v.extend_from_slice(&pl.to_le_bytes());
        v.extend_from_slice(&ck.to_le_bytes());
        v
    }

    #[test]
    fn parses_v11_header() {
        let data = build_dummy(0x11223344, 999, 0x55, 7, 0);
        let h = Header::parse(&data).unwrap();
        assert_eq!(h.version_hash, 0x11223344);
        assert_eq!(h.source_length(), 999);
        assert!(!h.is_module());
        assert_eq!(h.payload_offset, 24);
        assert_eq!(h.read_only_checksum, None);
        assert!(!h.brotli);
    }

    #[test]
    fn parses_v13_header() {
        let layout = HeaderLayout {
            version_hash: 4,
            source_hash: 8,
            flag_hash: 12,
            read_only_checksum: Some(16),
            payload_length: 20,
            checksum: 24,
            header_size: 28,
            num_reservations: None,
            num_stub_keys: None,
        };
        // [0..16] magic/vh/sh/fh + [16] ro + [20] plen + [24] checksum
        let mut data = Vec::new();
        data.extend_from_slice(&0xC0DE_0000u32.to_le_bytes());
        data.extend_from_slice(&0x11223344u32.to_le_bytes());
        data.extend_from_slice(&999u32.to_le_bytes());
        data.extend_from_slice(&0x55u32.to_le_bytes());
        data.extend_from_slice(&0xABCDu32.to_le_bytes());
        data.extend_from_slice(&7u32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        let h = Header::parse_with(&data, &layout).unwrap();
        assert_eq!(h.payload_offset, 28);
        assert_eq!(h.read_only_checksum, Some(0xABCD));
        assert_eq!(h.payload_length, 7);
    }

    #[test]
    fn module_flag_is_high_bit() {
        let data = build_dummy(1, 0x8000_03e8, 0, 0, 0);
        let h = Header::parse(&data).unwrap();
        assert!(h.is_module());
        assert_eq!(h.source_length(), 1000);
    }

    #[test]
    fn rejects_non_cache() {
        let data = b"not a jsc file at all........".to_vec();
        assert!(Header::parse(&data).is_err());
    }
}
