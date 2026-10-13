//! 索引构建（构建期，native）。产出 `search.idx` 字节流。

use std::collections::{BTreeMap, HashMap};

use crate::format::{self, h, BLOCK_ENTRY_SIZE, BLOCK_SIZE, DOC_RECORD_SIZE, HEADER_SIZE};
use crate::tokenize::{dedup_preserve_order, normalize, tokenize};

/// tags / categories 在字符串池里的连接符（U+001F，Unit Separator）
const LIST_SEPARATOR_STR: &str = "\u{1f}";

/// 一篇待索引的文档。字段与博客侧 `search-full.json` 的记录一一对应。
#[derive(Debug, Clone, Default)]
pub struct SearchDocument {
    /// 公开路由路径，如 `/post/119`
    pub path: String,
    pub title: String,
    pub date: String,
    pub tags: Vec<String>,
    pub categories: Vec<String>,
    /// 已去 HTML 的纯文本正文
    pub text: String,
}

/// 构建期统计信息，用于日志与体积核对。
#[derive(Debug, Clone, Copy, Default)]
pub struct BuildStats {
    pub docs: usize,
    pub terms: usize,
    pub blocks: usize,
    pub postings: usize,
    pub total_bytes: usize,
    pub terms_bytes: usize,
    pub postings_bytes: usize,
    pub docs_bytes: usize,
    pub pool_bytes: usize,
    pub facets_bytes: usize,
}

impl BuildStats {
    pub fn summary(&self) -> String {
        format!(
            "docs={} terms={} postings={} bytes={} (terms={} postings={} docs={} pool={} facets={})",
            self.docs,
            self.terms,
            self.postings,
            self.total_bytes,
            self.terms_bytes,
            self.postings_bytes,
            self.docs_bytes,
            self.pool_bytes,
            self.facets_bytes,
        )
    }
}

/// 带去重的字符串池。
struct StringPool {
    bytes: Vec<u8>,
    index: HashMap<String, (u32, u32)>,
}

impl StringPool {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            index: HashMap::new(),
        }
    }

    fn intern(&mut self, s: &str) -> (u32, u32) {
        if let Some(&hit) = self.index.get(s) {
            return hit;
        }
        let off = self.bytes.len() as u32;
        self.bytes.extend_from_slice(s.as_bytes());
        let entry = (off, s.len() as u32);
        self.index.insert(s.to_string(), entry);
        entry
    }
}

/// 两个字符串的共同前缀长度（按字符边界取整，避免切断 UTF-8 序列）。
fn common_prefix_len(a: &str, b: &str) -> usize {
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let max = ab.len().min(bb.len());
    let mut i = 0;
    while i < max && ab[i] == bb[i] {
        i += 1;
    }
    while i > 0 && !b.is_char_boundary(i) {
        i -= 1;
    }
    i
}

struct BlockEntry {
    first_term_off: u32,
    term_count: u32,
    postings_off: u32,
}

/// 构建索引。返回（字节流，统计）。
pub fn build_index(docs: &[SearchDocument]) -> (Vec<u8>, BuildStats) {
    // ---- 1. 倒排表 ----------------------------------------------------------
    // 只用文档级 postings：字段权重在查询期用原文 `contains` 现算，
    // 这样不必为 title/tags/body 各建一份倒排（体积 ×4），而候选集本来就只有几十个。
    let mut postings: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for (i, doc) in docs.iter().enumerate() {
        let doc_id = i as u32;
        // 字段间用 '\n' 连接：它不是 CJK、也不是 alphanumeric，能阻断跨字段的伪 bigram
        let joined = format!(
            "{}\n{}\n{}\n{}",
            normalize(&doc.title),
            normalize(&doc.tags.join(" ")),
            normalize(&doc.categories.join(" ")),
            normalize(&doc.text),
        );
        for token in dedup_preserve_order(tokenize(&joined)) {
            postings.entry(token).or_default().push(doc_id);
        }
    }

    // ---- 2. 词条与 postings 编码 -------------------------------------------
    let mut terms_blob: Vec<u8> = Vec::new();
    let mut postings_blob: Vec<u8> = Vec::new();
    let mut blocks: Vec<BlockEntry> = Vec::new();
    let mut prev_term = String::new();
    let mut total_postings = 0usize;

    for (i, (term, ids)) in postings.iter().enumerate() {
        if i % BLOCK_SIZE == 0 {
            blocks.push(BlockEntry {
                first_term_off: terms_blob.len() as u32,
                term_count: 0,
                postings_off: postings_blob.len() as u32,
            });
            // 块内首条词必须能独立解码，前缀共享清零
            prev_term.clear();
        }

        let mut delta = Vec::new();
        let mut last = 0u32;
        for &id in ids {
            format::write_varint(&mut delta, id - last);
            last = id;
        }
        total_postings += ids.len();

        let shared = common_prefix_len(&prev_term, term);
        format::write_varint(&mut terms_blob, shared as u32);
        format::write_varint(&mut terms_blob, (term.len() - shared) as u32);
        terms_blob.extend_from_slice(&term.as_bytes()[shared..]);
        format::write_varint(&mut terms_blob, delta.len() as u32);
        format::write_varint(&mut terms_blob, ids.len() as u32);

        postings_blob.extend_from_slice(&delta);

        if let Some(block) = blocks.last_mut() {
            block.term_count += 1;
        }
        prev_term.clear();
        prev_term.push_str(term);
    }

    let mut blocks_blob = Vec::with_capacity(blocks.len() * BLOCK_ENTRY_SIZE);
    for block in &blocks {
        format::write_u32(&mut blocks_blob, block.first_term_off);
        format::write_u32(&mut blocks_blob, block.term_count);
        format::write_u32(&mut blocks_blob, block.postings_off);
        format::write_u32(&mut blocks_blob, 0);
    }

    // ---- 3. 文档表与字符串池 -----------------------------------------------
    let mut pool = StringPool::new();
    let mut docs_blob = Vec::with_capacity(docs.len() * DOC_RECORD_SIZE);
    for doc in docs {
        let (path_off, path_len) = pool.intern(&doc.path);
        let (title_off, title_len) = pool.intern(&doc.title);
        let (date_off, date_len) = pool.intern(&doc.date);
        let (text_off, text_len) = pool.intern(&doc.text);
        let (tags_off, tags_len) = pool.intern(&doc.tags.join(LIST_SEPARATOR_STR));
        let (cats_off, cats_len) = pool.intern(&doc.categories.join(LIST_SEPARATOR_STR));
        for value in [
            path_off, path_len, title_off, title_len, date_off, date_len, text_off, text_len,
            tags_off, tags_len, cats_off, cats_len,
        ] {
            format::write_u32(&mut docs_blob, value);
        }
    }

    // ---- 4. 分面（标签 / 分类）--------------------------------------------
    let mut facets_blob = Vec::new();
    let tag_facets = collect_facets(docs.iter().map(|d| d.tags.iter()));
    let cat_facets = collect_facets(docs.iter().map(|d| d.categories.iter()));
    write_facets(&mut facets_blob, &tag_facets);
    write_facets(&mut facets_blob, &cat_facets);

    // ---- 5. 拼装 -----------------------------------------------------------
    let mut out = vec![0u8; HEADER_SIZE];
    let terms_off = out.len();
    out.extend_from_slice(&terms_blob);
    let terms_len = terms_blob.len();

    let blocks_off = out.len();
    out.extend_from_slice(&blocks_blob);
    let blocks_len = blocks_blob.len();

    let postings_off = out.len();
    out.extend_from_slice(&postings_blob);
    let postings_len = postings_blob.len();

    let docs_off = out.len();
    out.extend_from_slice(&docs_blob);
    let docs_len = docs_blob.len();

    let pool_off = out.len();
    out.extend_from_slice(&pool.bytes);
    let pool_len = pool.bytes.len();

    let facets_off = out.len();
    out.extend_from_slice(&facets_blob);
    let facets_len = facets_blob.len();

    let doc_count = docs.len() as u32;
    let term_count = postings.len() as u32;
    let block_count = blocks.len() as u32;

    out[h::MAGIC..h::MAGIC + 4].copy_from_slice(&format::MAGIC);
    let header_fields = [
        (h::VERSION, format::VERSION),
        (h::FLAGS, format::flags::UNIGRAM_PREFIX_EXPANSION),
        (h::DOC_COUNT, doc_count),
        (h::TERM_COUNT, term_count),
        (h::BLOCK_COUNT, block_count),
        (h::BLOCK_SIZE, BLOCK_SIZE as u32),
        (h::TERMS_OFF, terms_off as u32),
        (h::TERMS_LEN, terms_len as u32),
        (h::BLOCKS_OFF, blocks_off as u32),
        (h::BLOCKS_LEN, blocks_len as u32),
        (h::POSTINGS_OFF, postings_off as u32),
        (h::POSTINGS_LEN, postings_len as u32),
        (h::DOCS_OFF, docs_off as u32),
        (h::DOCS_LEN, docs_len as u32),
        (h::POOL_OFF, pool_off as u32),
        (h::POOL_LEN, pool_len as u32),
        (h::FACETS_OFF, facets_off as u32),
        (h::FACETS_LEN, facets_len as u32),
    ];
    for (off, value) in header_fields {
        format::patch_u32(&mut out, off, value);
    }

    let stats = BuildStats {
        docs: docs.len(),
        terms: postings.len(),
        blocks: blocks.len(),
        postings: total_postings,
        total_bytes: out.len(),
        terms_bytes: terms_len,
        postings_bytes: postings_len,
        docs_bytes: docs_len,
        pool_bytes: pool_len,
        facets_bytes: facets_len,
    };
    (out, stats)
}

/// 分面名归一化后用 BTreeMap 聚合（确定性输出）。
fn collect_facets<'a>(
    lists: impl Iterator<Item = std::slice::Iter<'a, String>>,
) -> BTreeMap<String, Vec<u32>> {
    let mut map: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for (doc_id, list) in lists.enumerate() {
        for name in list {
            let key = normalize(name);
            if key.is_empty() {
                continue;
            }
            map.entry(key).or_default().push(doc_id as u32);
        }
    }
    map
}

fn write_facets(blob: &mut Vec<u8>, facets: &BTreeMap<String, Vec<u32>>) {
    // 计数用 varint，与 query::parse_facet_map 对称
    format::write_varint(blob, facets.len() as u32);
    for (name, ids) in facets {
        format::write_varint(blob, name.len() as u32);
        blob.extend_from_slice(name.as_bytes());
        format::write_varint(blob, ids.len() as u32);
        let mut last = 0u32;
        for &id in ids {
            format::write_varint(blob, id - last);
            last = id;
        }
    }
}
