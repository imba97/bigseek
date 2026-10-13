//! 索引读取与查询（运行期）。**零反序列化**：只校验头 + 解析分面表，词条与 postings 按需从字节切片读。

use std::collections::{HashMap, HashSet};

use crate::format::{
    self, d as df, h, BLOCK_ENTRY_SIZE, DOC_RECORD_SIZE, HEADER_SIZE, LIST_SEPARATOR,
};
use crate::tokenize::{dedup_preserve_order, is_cjk_like, normalize, tokenize};

pub const SEARCH_LIMIT_DEFAULT: usize = 40;
pub const SEARCH_LIMIT_MAX: usize = 200;
/// snippet 命中点前后的字符窗口
const SNIPPET_WINDOW_CHARS: usize = 72;
/// 无命中时的摘要长度
const SNIPPET_HEAD_CHARS: usize = 140;
/// 单次前缀展开最多吃进的词条数（防止单字母查询把词典扫穿）
const MAX_PREFIX_TERMS: usize = 4096;
/// 候选集上限
const MAX_CANDIDATES: usize = 20_000;

const WEIGHT_TITLE: u32 = 4;
const WEIGHT_TAG: u32 = 3;
const WEIGHT_CATEGORY: u32 = 2;
const WEIGHT_TEXT: u32 = 1;
/// 整串精确命中（标题/正文/标签含完整查询串）的加权
const BONUS_EXACT: u32 = 6;

/// 一条搜索结果。字段与博客侧 `SearchHit` 对齐，另附 `score` / `exact` 便于调试与排序展示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub path: String,
    pub title: String,
    pub date: String,
    pub tags: Vec<String>,
    pub categories: Vec<String>,
    pub snippet: String,
    pub score: u32,
    pub exact: bool,
}

/// 分面表：归一化名称 → 该分面下的 doc id（升序）
type FacetMap = HashMap<String, Vec<u32>>;

/// 已加载的索引。
pub struct Index {
    data: Vec<u8>,
    doc_count: u32,
    term_count: u32,
    block_count: u32,
    block_size: u32,
    terms_off: usize,
    terms_len: usize,
    blocks_off: usize,
    postings_off: usize,
    docs_off: usize,
    pool_off: usize,
    tags: FacetMap,
    categories: FacetMap,
}

struct DocView<'a> {
    path: &'a str,
    title: &'a str,
    date: &'a str,
    text: &'a str,
    tags: &'a str,
    categories: &'a str,
}

impl DocView<'_> {
    fn tag_list(&self) -> Vec<String> {
        split_list(self.tags)
    }

    fn category_list(&self) -> Vec<String> {
        split_list(self.categories)
    }
}

fn split_list(raw: &str) -> Vec<String> {
    if raw.is_empty() {
        return Vec::new();
    }
    raw.split(LIST_SEPARATOR)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// 顺序解码块内的前端编码词条。
struct TermCursor<'a> {
    data: &'a [u8],
    end: usize,
    pos: usize,
    prev: String,
}

impl<'a> TermCursor<'a> {
    fn new(data: &'a [u8], terms_off: usize, terms_len: usize, start: usize) -> Self {
        Self {
            data,
            end: terms_off + terms_len,
            pos: start,
            prev: String::new(),
        }
    }

    /// 返回（词条, postings 字节长度, doc_freq）并前进游标。块内首条记录共享前缀为 0。
    fn next(&mut self) -> Option<(String, u32, u32)> {
        if self.pos >= self.end {
            return None;
        }
        let shared = format::read_varint(self.data, &mut self.pos)? as usize;
        let suffix_len = format::read_varint(self.data, &mut self.pos)? as usize;
        let suffix_end = self.pos.checked_add(suffix_len)?;
        let suffix = std::str::from_utf8(self.data.get(self.pos..suffix_end)?).ok()?;
        self.pos = suffix_end;

        let postings_len = format::read_varint(self.data, &mut self.pos)?;
        let doc_freq = format::read_varint(self.data, &mut self.pos)?;

        if shared > self.prev.len() || !self.prev.is_char_boundary(shared) {
            return None;
        }
        let mut term = String::with_capacity(shared + suffix_len);
        term.push_str(&self.prev[..shared]);
        term.push_str(suffix);
        self.prev = term.clone();
        Some((term, postings_len, doc_freq))
    }
}

impl Index {
    /// 加载索引字节流。只做头部校验与分面表解析，不做整体反序列化。
    pub fn load(data: Vec<u8>) -> Result<Self, String> {
        if data.len() < HEADER_SIZE {
            return Err(format!("索引头不完整：{} 字节", data.len()));
        }
        if data[h::MAGIC..h::MAGIC + 4] != format::MAGIC {
            return Err("magic 不匹配，不是本库产出的索引".to_string());
        }
        let read = |off: usize| -> Result<u32, String> {
            format::read_u32(&data, off).ok_or_else(|| format!("头部字段越界 @{off}"))
        };
        let version = read(h::VERSION)?;
        if version != format::VERSION {
            return Err(format!(
                "索引格式版本不匹配：文件 {version}，本库 {}",
                format::VERSION
            ));
        }

        let terms_off = read(h::TERMS_OFF)? as usize;
        let terms_len = read(h::TERMS_LEN)? as usize;
        let blocks_off = read(h::BLOCKS_OFF)? as usize;
        let postings_off = read(h::POSTINGS_OFF)? as usize;
        let docs_off = read(h::DOCS_OFF)? as usize;
        let docs_len = read(h::DOCS_LEN)? as usize;
        let pool_off = read(h::POOL_OFF)? as usize;
        let facets_off = read(h::FACETS_OFF)? as usize;
        let facets_len = read(h::FACETS_LEN)? as usize;
        let doc_count = read(h::DOC_COUNT)?;
        let block_count = read(h::BLOCK_COUNT)?;

        let len = data.len();
        for (name, off, size) in [
            ("terms", terms_off, terms_len),
            (
                "blocks",
                blocks_off,
                block_count as usize * BLOCK_ENTRY_SIZE,
            ),
            ("docs", docs_off, docs_len),
            ("pool", pool_off, 0usize),
            ("facets", facets_off, facets_len),
        ] {
            let fits = off <= len && off.checked_add(size).is_some_and(|end| end <= len);
            if !fits {
                return Err(format!("{name} 段越界：off={off} size={size} len={len}"));
            }
        }
        if docs_len < doc_count as usize * DOC_RECORD_SIZE {
            return Err("文档表长度与文档数不一致".to_string());
        }

        let (tags, categories) = parse_facets(&data, facets_off, facets_len)?;

        Ok(Self {
            doc_count,
            term_count: read(h::TERM_COUNT)?,
            block_count,
            block_size: read(h::BLOCK_SIZE)?.max(1),
            terms_off,
            terms_len,
            blocks_off,
            postings_off,
            docs_off,
            pool_off,
            tags,
            categories,
            data,
        })
    }

    pub fn doc_count(&self) -> usize {
        self.doc_count as usize
    }

    pub fn term_count(&self) -> usize {
        self.term_count as usize
    }

    pub fn block_size(&self) -> usize {
        self.block_size as usize
    }

    pub fn format_version(&self) -> u32 {
        format::VERSION
    }

    pub fn tag_count(&self) -> usize {
        self.tags.len()
    }

    pub fn category_count(&self) -> usize {
        self.categories.len()
    }

    /// 调试用：取一个文档的路径（按构建期顺序，0 起）。
    pub fn doc_path(&self, id: u32) -> Option<String> {
        self.doc(id).map(|d| d.path.to_string())
    }

    // ---- 底层读取 ----------------------------------------------------------

    fn doc(&self, id: u32) -> Option<DocView<'_>> {
        if id >= self.doc_count {
            return None;
        }
        let base = self.docs_off + id as usize * DOC_RECORD_SIZE;
        let field = |off_field: usize, len_field: usize| -> Option<&str> {
            let off = format::read_u32(&self.data, base + off_field)? as usize;
            let len = format::read_u32(&self.data, base + len_field)? as usize;
            let start = self.pool_off.checked_add(off)?;
            let end = start.checked_add(len)?;
            std::str::from_utf8(self.data.get(start..end)?).ok()
        };
        Some(DocView {
            path: field(df::PATH_OFF, df::PATH_LEN)?,
            title: field(df::TITLE_OFF, df::TITLE_LEN)?,
            date: field(df::DATE_OFF, df::DATE_LEN)?,
            text: field(df::TEXT_OFF, df::TEXT_LEN)?,
            tags: field(df::TAGS_OFF, df::TAGS_LEN)?,
            categories: field(df::CATS_OFF, df::CATS_LEN)?,
        })
    }

    /// 块索引项：(首词在 terms 段的绝对偏移, 块内词条数, 块起始 postings 绝对偏移)
    fn block_entry(&self, block: u32) -> Option<(usize, u32, usize)> {
        if block >= self.block_count {
            return None;
        }
        let base = self.blocks_off + block as usize * BLOCK_ENTRY_SIZE;
        Some((
            format::read_u32(&self.data, base)? as usize + self.terms_off,
            format::read_u32(&self.data, base + 4)?,
            format::read_u32(&self.data, base + 8)? as usize + self.postings_off,
        ))
    }

    fn block_first_term(&self, block: u32) -> Option<String> {
        let (term_off, count, _) = self.block_entry(block)?;
        if count == 0 {
            return None;
        }
        let mut cursor = TermCursor::new(&self.data, self.terms_off, self.terms_len, term_off);
        cursor.next().map(|(term, _, _)| term)
    }

    /// 二分找到「首词 <= term」的最大块号；若不存在（首词整体大于 term），退化为 0 号块。
    fn locate_block(&self, term: &str) -> Option<u32> {
        if self.block_count == 0 {
            return None;
        }
        let (mut lo, mut hi) = (0u32, self.block_count - 1);
        let mut found: Option<u32> = None;
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            let first_le =
                matches!(self.block_first_term(mid), Some(first) if first.as_str() <= term);
            if first_le {
                found = Some(mid);
                lo = mid + 1;
            } else if mid == 0 {
                break;
            } else {
                hi = mid - 1;
            }
        }
        Some(found.unwrap_or(0))
    }

    fn postings_slice(&self, pos: usize, len: u32) -> &[u8] {
        let end = pos.saturating_add(len as usize);
        self.data.get(pos..end).unwrap_or(&[])
    }

    fn decode_postings(&self, pos: usize, len: u32) -> Vec<u32> {
        let bytes = self.postings_slice(pos, len);
        let mut ids = Vec::new();
        let mut cursor = 0usize;
        let mut last = 0u32;
        while let Some(delta) = format::read_varint(bytes, &mut cursor) {
            last = last.saturating_add(delta);
            ids.push(last);
        }
        ids
    }

    /// 精确查词 → (postings 绝对偏移, postings 字节长度, doc_freq)
    fn exact_postings(&self, term: &str) -> Option<(usize, u32, u32)> {
        let block = self.locate_block(term)?;
        let (term_off, count, block_postings) = self.block_entry(block)?;
        let mut cursor = TermCursor::new(&self.data, self.terms_off, self.terms_len, term_off);
        let mut postings_pos = block_postings;
        for _ in 0..count {
            let (current, postings_len, doc_freq) = cursor.next()?;
            if current.as_str() == term {
                return Some((postings_pos, postings_len, doc_freq));
            }
            if current.as_str() > term {
                return None;
            }
            postings_pos += postings_len as usize;
        }
        None
    }

    /// 前缀展开：所有以 `prefix` 开头的词条对应的 doc 集合（含精确命中的那个词条）。
    fn prefix_postings(&self, prefix: &str, budget: &mut usize) -> Vec<Vec<u32>> {
        let mut out = Vec::new();
        let Some(start_block) = self.locate_block(prefix) else {
            return out;
        };
        for block in start_block..self.block_count {
            let Some((first_term_off, count, block_postings)) = self.block_entry(block) else {
                break;
            };
            let mut cursor =
                TermCursor::new(&self.data, self.terms_off, self.terms_len, first_term_off);
            let mut postings_pos = block_postings;
            for _ in 0..count {
                let Some((term, postings_len, _)) = cursor.next() else {
                    break;
                };
                let this_pos = postings_pos;
                postings_pos += postings_len as usize;

                if term.starts_with(prefix) {
                    if *budget == 0 {
                        return out;
                    }
                    *budget -= 1;
                    out.push(self.decode_postings(this_pos, postings_len));
                } else if term.as_str() > prefix {
                    // 词条有序：跨过前缀区间即可整体收工
                    return out;
                }
            }
        }
        out
    }

    /// 一个 token 的候选文档集合。
    ///
    /// - 精确命中直接使用
    /// - 单字符 CJK token（索引里只有 bigram）与最后一个 token（自动补全语义）额外做前缀展开
    fn token_docs(&self, token: &str, is_last: bool) -> Vec<u32> {
        let mut ids = match self.exact_postings(token) {
            Some((pos, len, _)) => self.decode_postings(pos, len),
            None => Vec::new(),
        };

        let single_cjk = token.chars().count() == 1 && token.chars().all(is_cjk_like);
        if is_last || single_cjk {
            let mut budget = MAX_PREFIX_TERMS;
            for extra in self.prefix_postings(token, &mut budget) {
                ids.extend(extra);
            }
            ids.sort_unstable();
            ids.dedup();
        }
        ids
    }

    /// 范围过滤：返回受限文档集合；两个都不给则 None（不限定）。
    fn facet_docs(&self, tag: Option<&str>, category: Option<&str>) -> Option<Vec<u32>> {
        let tag_ids = tag.map(|t| self.tags.get(&normalize(t)).cloned().unwrap_or_default());
        let cat_ids = category.map(|c| {
            self.categories
                .get(&normalize(c))
                .cloned()
                .unwrap_or_default()
        });
        match (tag_ids, cat_ids) {
            (None, None) => None,
            (Some(ids), None) => Some(ids),
            (None, Some(ids)) => Some(ids),
            (Some(tag_ids), Some(cat_ids)) => {
                let set: HashSet<u32> = cat_ids.into_iter().collect();
                Some(tag_ids.into_iter().filter(|id| set.contains(id)).collect())
            }
        }
    }

    // ---- 查询 --------------------------------------------------------------

    /// 主查询入口。`tag` / `category` 为空表示不限定范围。
    pub fn query(
        &self,
        keywords: &str,
        tag: Option<&str>,
        category: Option<&str>,
        limit: usize,
    ) -> Vec<SearchHit> {
        let limit = if limit == 0 {
            SEARCH_LIMIT_DEFAULT
        } else {
            limit.min(SEARCH_LIMIT_MAX)
        };
        let query_text = normalize(keywords).trim().to_string();
        let scope = self.facet_docs(tag, category);

        // 只限定范围、没有关键词：按文档原序（构建期已按日期降序）返回，摘要留空
        if query_text.is_empty() {
            let Some(ids) = scope else {
                return Vec::new();
            };
            return ids
                .iter()
                .take(limit)
                .filter_map(|&id| self.build_hit(id, None, 0, false))
                .collect();
        }

        let tokens = dedup_preserve_order(tokenize(&query_text));
        if tokens.is_empty() {
            return Vec::new();
        }
        let scope_set: Option<HashSet<u32>> = scope.map(|ids| ids.into_iter().collect());

        // 每个 token 的候选集（已按范围过滤）
        let mut sets: Vec<Vec<u32>> = Vec::new();
        for (i, token) in tokens.iter().enumerate() {
            let is_last = i + 1 == tokens.len();
            let mut ids = self.token_docs(token, is_last);
            if let Some(set) = &scope_set {
                ids.retain(|id| set.contains(id));
            }
            if !ids.is_empty() {
                sets.push(ids);
            }
        }
        if sets.is_empty() {
            return Vec::new();
        }

        // 先试 AND（所有 token 都命中）；为空则退化为 OR，按命中 token 数排序
        let mut candidates: Vec<u32> = Vec::new();
        if sets.len() == tokens.len() {
            let mut acc = sets[0].clone();
            for set in &sets[1..] {
                let lookup: HashSet<u32> = set.iter().copied().collect();
                acc.retain(|id| lookup.contains(id));
                if acc.is_empty() {
                    break;
                }
            }
            candidates = acc;
        }
        if candidates.is_empty() {
            let mut union: HashSet<u32> = HashSet::new();
            for set in &sets {
                for &id in set {
                    union.insert(id);
                    if union.len() >= MAX_CANDIDATES {
                        break;
                    }
                }
            }
            candidates = union.into_iter().collect();
        }
        candidates.sort_unstable();

        // 打分：字段命中权重（字段级判断用原文 `contains`，候选集只有几十个，成本可忽略）
        let mut scored: Vec<(
            u32,  /*matched*/
            u32,  /*score*/
            u32,  /*doc_id*/
            bool, /*exact*/
        )> = Vec::with_capacity(candidates.len());
        for id in candidates {
            let Some(view) = self.doc(id) else {
                continue;
            };
            let n_title = normalize(view.title);
            let n_tags = normalize(view.tags);
            let n_cats = normalize(view.categories);
            let n_text = normalize(view.text);

            let mut matched = 0u32;
            let mut score = 0u32;
            for token in &tokens {
                let mut token_matched = false;
                if n_title.contains(token.as_str()) {
                    score += WEIGHT_TITLE;
                    token_matched = true;
                }
                if n_tags.contains(token.as_str()) {
                    score += WEIGHT_TAG;
                    token_matched = true;
                }
                if n_cats.contains(token.as_str()) {
                    score += WEIGHT_CATEGORY;
                    token_matched = true;
                }
                if n_text.contains(token.as_str()) {
                    score += WEIGHT_TEXT;
                    token_matched = true;
                }
                if token_matched {
                    matched += 1;
                }
            }
            let exact = n_title.contains(&query_text)
                || n_text.contains(&query_text)
                || n_tags.contains(&query_text)
                || n_cats.contains(&query_text);
            if exact {
                score += BONUS_EXACT;
            }
            scored.push((matched, score, id, exact));
        }

        // 命中 token 多者优先 → 分数高者优先 → 文档原序（日期降序）
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));

        let first_token = tokens.first().map(String::as_str).unwrap_or("");
        scored
            .into_iter()
            .take(limit)
            .filter_map(|(_, score, id, exact)| {
                let needle = if exact {
                    query_text.as_str()
                } else {
                    first_token
                };
                self.build_hit(id, Some(needle), score, exact)
            })
            .collect()
    }

    fn build_hit(
        &self,
        id: u32,
        needle: Option<&str>,
        score: u32,
        exact: bool,
    ) -> Option<SearchHit> {
        let view = self.doc(id)?;
        let snippet = needle
            .map(|n| build_snippet(view.text, n))
            .unwrap_or_default();
        Some(SearchHit {
            path: view.path.to_string(),
            title: view.title.to_string(),
            date: view.date.to_string(),
            tags: view.tag_list(),
            categories: view.category_list(),
            snippet,
            score,
            exact,
        })
    }
}

/// 摘要：在归一化文本里定位 needle，再把「字符下标」映射回原文切窗口。
///
/// 成立的前提是 `normalize` 逐字符 1:1（不增删字符），见 `tokenize::normalize` 的说明。
fn build_snippet(original: &str, needle: &str) -> String {
    let total_chars = original.chars().count();
    if needle.is_empty() || total_chars == 0 {
        return head_of(original, SNIPPET_HEAD_CHARS);
    }
    let normalized = normalize(original);
    let Some(byte_idx) = normalized.find(needle) else {
        return head_of(original, SNIPPET_HEAD_CHARS);
    };
    let hit_char_idx = normalized[..byte_idx].chars().count();
    let needle_chars = needle.chars().count();

    let start = hit_char_idx.saturating_sub(SNIPPET_WINDOW_CHARS);
    let end = (hit_char_idx + needle_chars + SNIPPET_WINDOW_CHARS).min(total_chars);
    let body: String = original.chars().skip(start).take(end - start).collect();
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        body,
        if end < total_chars { "…" } else { "" }
    )
}

fn head_of(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}…")
}

fn parse_facets(data: &[u8], off: usize, len: usize) -> Result<(FacetMap, FacetMap), String> {
    let section = data
        .get(off..off + len)
        .ok_or_else(|| "分面段越界".to_string())?;
    let mut pos = 0usize;
    let tags = parse_facet_map(section, &mut pos)?;
    let categories = parse_facet_map(section, &mut pos)?;
    Ok((tags, categories))
}

fn parse_facet_map(section: &[u8], pos: &mut usize) -> Result<HashMap<String, Vec<u32>>, String> {
    let count = format::read_varint(section, pos).ok_or_else(|| "分面表读取失败".to_string())?;
    let mut map = HashMap::with_capacity(count as usize);
    for _ in 0..count {
        let name_len = format::read_varint(section, pos)
            .ok_or_else(|| "分面名长度读取失败".to_string())? as usize;
        let name_end = (*pos)
            .checked_add(name_len)
            .ok_or_else(|| "分面名越界".to_string())?;
        let name = std::str::from_utf8(
            section
                .get(*pos..name_end)
                .ok_or_else(|| "分面名越界".to_string())?,
        )
        .map_err(|e| format!("分面名非 UTF-8：{e}"))?
        .to_string();
        *pos = name_end;

        let doc_count =
            format::read_varint(section, pos).ok_or_else(|| "分面文档数读取失败".to_string())?;
        let mut ids = Vec::with_capacity(doc_count as usize);
        let mut last = 0u32;
        for _ in 0..doc_count {
            let delta = format::read_varint(section, pos)
                .ok_or_else(|| "分面 doc id 读取失败".to_string())?;
            last = last.saturating_add(delta);
            ids.push(last);
        }
        map.insert(name, ids);
    }
    Ok(map)
}
