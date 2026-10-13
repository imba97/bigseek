//! 二进制索引格式（v1）。所有整数小端序，变长整数为 LEB128 无符号（最多 5 字节）。
//!
//! ```text
//! ┌ header 80 B ───────────────────────────────────────────────────────────┐
//! │ magic "BSSI" | version | flags | doc_count | term_count | block_count    │
//! │ block_size | terms_off/len | blocks_off/len | postings_off/len           │
//! │ docs_off/len | pool_off/len | facets_off/len                            │
//! ├ terms ──────────────────────────────────────────────────────────────────┤
//! │ 按字典序排列的前端编码（front-coded）词条记录，每 BLOCK_SIZE 条为一块    │
//! │ 记录：varint prefix_len | varint suffix_len | suffix bytes               │
//! │       | varint postings_byte_len | varint doc_freq                       │
//! ├ blocks ─────────────────────────────────────────────────────────────────┤
//! │ 稀疏块索引，每块 16 B：first_term_off u32 | term_count u32               │
//! │                        | postings_off u32 | reserved u32                 │
//! │ 块内首条词的 prefix_len 恒为 0，因此可以随机跳进任意块开始顺序解码        │
//! ├ postings ───────────────────────────────────────────────────────────────┤
//! │ 各词条的 doc id 升序 delta + varint，按词条顺序连续存放                   │
//! ├ docs ───────────────────────────────────────────────────────────────────┤
//! │ 定长 48 B 记录：path/title/date/text/tags/cats 的 (off u32, len u32)      │
//! │ tags / cats 在 pool 里以 U+001F 连接                                      │
//! ├ pool ───────────────────────────────────────────────────────────────────┤
//! │ 所有字符串（原文，未归一化）按写盘顺序拼接，带相同串去重                   │
//! └ facets ─────────────────────────────────────────────────────────────────┘
//!   u32 tag_count | (varint name_len, name, varint doc_count, delta-varint ids)*
//!   u32 category_count | 同上
//! ```
//!
//! 读取路径刻意做到「零反序列化」：`Index::load` 只校验头、算好 doc 记录偏移、解析 facet 表，
//! 词条与 postings 全部按需从字节切片读取。这是冷启动能压到毫秒级的原因。

pub const MAGIC: [u8; 4] = *b"BSSI";
pub const VERSION: u32 = 1;
/// 头部固定 80 字节：19 个 u32 字段（0..76）+ 4 字节保留。
/// 注意 `h::FACETS_LEN` 落在 72..76，最后 4 字节必须留给保留区，
/// 否则会写穿到 terms 段的第一条词上（曾因此让整个词典解不出来）。
pub const HEADER_SIZE: usize = 80;
/// 稀疏块索引的块大小（词条数）。越小随机访问越快、块索引越大。
pub const BLOCK_SIZE: usize = 16;
pub const BLOCK_ENTRY_SIZE: usize = 16;
pub const DOC_RECORD_SIZE: usize = 48;
/// facets 里 tags / categories 的分隔符
pub const LIST_SEPARATOR: char = '\u{1f}';

/// flags 位定义
pub mod flags {
    /// 单字符 CJK 查询走词典前缀展开（v1 恒为 1）
    pub const UNIGRAM_PREFIX_EXPANSION: u32 = 1 << 0;
}

/// header 字段在文件中的字节偏移
pub mod h {
    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 4;
    pub const FLAGS: usize = 8;
    pub const DOC_COUNT: usize = 12;
    pub const TERM_COUNT: usize = 16;
    pub const BLOCK_COUNT: usize = 20;
    pub const BLOCK_SIZE: usize = 24;
    pub const TERMS_OFF: usize = 28;
    pub const TERMS_LEN: usize = 32;
    pub const BLOCKS_OFF: usize = 36;
    pub const BLOCKS_LEN: usize = 40;
    pub const POSTINGS_OFF: usize = 44;
    pub const POSTINGS_LEN: usize = 48;
    pub const DOCS_OFF: usize = 52;
    pub const DOCS_LEN: usize = 56;
    pub const POOL_OFF: usize = 60;
    pub const POOL_LEN: usize = 64;
    pub const FACETS_OFF: usize = 68;
    pub const FACETS_LEN: usize = 72;
}

/// doc 记录字段偏移
pub mod d {
    pub const PATH_OFF: usize = 0;
    pub const PATH_LEN: usize = 4;
    pub const TITLE_OFF: usize = 8;
    pub const TITLE_LEN: usize = 12;
    pub const DATE_OFF: usize = 16;
    pub const DATE_LEN: usize = 20;
    pub const TEXT_OFF: usize = 24;
    pub const TEXT_LEN: usize = 28;
    pub const TAGS_OFF: usize = 32;
    pub const TAGS_LEN: usize = 36;
    pub const CATS_OFF: usize = 40;
    pub const CATS_LEN: usize = 44;
}

pub fn write_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

pub fn write_varint(buf: &mut Vec<u8>, mut v: u32) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

/// 在 buf 的 `off` 处写入 u32（要求该区域已存在，用于回填 header/块索引）。
pub fn patch_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

pub fn read_u32(data: &[u8], off: usize) -> Option<u32> {
    let end = off.checked_add(4)?;
    let slice = data.get(off..end)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

pub fn read_varint(data: &[u8], pos: &mut usize) -> Option<u32> {
    let mut result: u32 = 0;
    let mut shift = 0u32;
    loop {
        let byte = *data.get(*pos)?;
        *pos += 1;
        result |= u32::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift > 28 {
            return None;
        }
    }
}

/// LEB128 编码 `v` 所需的字节数。
pub fn varint_len(v: u32) -> usize {
    let mut n = 1;
    let mut v = v >> 7;
    while v != 0 {
        n += 1;
        v >>= 7;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        for v in [0u32, 1, 127, 128, 300, 16_383, 16_384, u32::MAX] {
            let mut buf = Vec::new();
            write_varint(&mut buf, v);
            assert_eq!(buf.len(), varint_len(v), "varint_len 与实际不符: {v}");
            let mut pos = 0;
            assert_eq!(read_varint(&buf, &mut pos), Some(v));
            assert_eq!(pos, buf.len());
        }
    }
}
