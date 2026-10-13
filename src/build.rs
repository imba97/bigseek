//! 索引构建（构建期，native）。产出 `search.idx` 字节流。

use std::collections::{BTreeMap, HashMap};

use crate::format::{self, h, BLOCK_ENTRY_SIZE, BLOCK_SIZE, DOC_RECORD_SIZE, HEADER_SIZE};
use crate::tokenize::{dedup_preserve_order, normalize, normalize_into, tokenize};

/// tags / categories 在字符串池里的连接符（U+001F，Unit Separator）
const LIST_SEPARATOR_STR: &str = "\u{1f}";

/// 字段分隔符：`\n` 既不是 CJK 也不是 alphanumeric，用来阻断跨字段的伪 bigram
const FIELD_SEPARATOR: char = '\n';

/// 把一篇文档拼成「索引用的归一化文本」，追加式（复用缓冲）。
///
/// 字段顺序（title / tags / categories / text）与分隔符是**索引语义的一部分**，不能改。
/// 索引构建与 `--verify` 自检必须共用这一个函数：自检原先漏掉 categories 且字段顺序不同，
/// 导致「分类没进索引」这类回归永远不被发现。
pub fn push_indexed_text(doc: &SearchDocument, out: &mut String) {
    out.clear();
    normalize_into(&doc.title, out);
    out.push(FIELD_SEPARATOR);
    normalize_list_into(&doc.tags, out);
    out.push(FIELD_SEPARATOR);
    normalize_list_into(&doc.categories, out);
    out.push(FIELD_SEPARATOR);
    normalize_into(&doc.text, out);
}

/// [`push_indexed_text`] 的分配版（自检等一次性场景用）
pub fn indexed_text(doc: &SearchDocument) -> String {
    let mut out = String::new();
    push_indexed_text(doc, &mut out);
    out
}

/// 把一组标签/分类用空格连接并归一化后追加到缓冲。
///
/// 等价于 `normalize(&list.join(" "))`：归一化是逐字符映射，而空格映射到自身。
fn normalize_list_into(list: &[String], out: &mut String) {
    for (i, item) in list.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        normalize_into(item, out);
    }
}

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
    //
    // 用 HashMap 而不是 BTreeMap：插入省掉 O(log n) 次字符串比较（本语料 24.5 万次插入），
    // 输出顺序仍由第 2 步「按词条字典序排序」保证，与 BTreeMap 的迭代顺序完全一致。
    let mut postings: HashMap<String, Vec<u32>> = HashMap::new();
    let mut joined = String::new();
    for (i, doc) in docs.iter().enumerate() {
        let doc_id = i as u32;
        push_indexed_text(doc, &mut joined);

        for token in dedup_preserve_order(tokenize(&joined)) {
            // 先查再插：命中已有词条时不产生 String 分配
            match postings.get_mut(token) {
                Some(ids) => ids.push(doc_id),
                None => {
                    postings.insert(token.to_string(), vec![doc_id]);
                }
            }
        }
    }

    // ---- 2. 词条与 postings 编码 -------------------------------------------
    let mut terms_blob: Vec<u8> = Vec::new();
    let mut postings_blob: Vec<u8> = Vec::new();
    let mut blocks: Vec<BlockEntry> = Vec::new();
    let mut prev_term: &str = "";
    let mut total_postings = 0usize;

    let mut sorted: Vec<(&str, &[u32])> = postings
        .iter()
        .map(|(term, ids)| (term.as_str(), ids.as_slice()))
        .collect();
    sorted.sort_unstable_by(|a, b| a.0.cmp(b.0));

    for (i, (term, ids)) in sorted.into_iter().enumerate() {
        if i % BLOCK_SIZE == 0 {
            blocks.push(BlockEntry {
                first_term_off: terms_blob.len() as u32,
                term_count: 0,
                postings_off: postings_blob.len() as u32,
            });
            // 块内首条词必须能独立解码，前缀共享清零
            prev_term = "";
        }

        // delta 直接写进 postings_blob：不再为每个词条新建一个临时 Vec 再拷贝
        let delta_start = postings_blob.len();
        let mut last = 0u32;
        for &id in ids {
            format::write_varint(&mut postings_blob, id - last);
            last = id;
        }
        let delta_len = postings_blob.len() - delta_start;
        total_postings += ids.len();

        let shared = common_prefix_len(prev_term, term);
        format::write_varint(&mut terms_blob, shared as u32);
        format::write_varint(&mut terms_blob, (term.len() - shared) as u32);
        terms_blob.extend_from_slice(&term.as_bytes()[shared..]);
        format::write_varint(&mut terms_blob, delta_len as u32);
        format::write_varint(&mut terms_blob, ids.len() as u32);

        if let Some(block) = blocks.last_mut() {
            block.term_count += 1;
        }
        prev_term = term;
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
