//! 索引读取与查询（运行期）。**零反序列化**：只校验头 + 解析分面表，词条与 postings 按需从字节切片读。

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::format::{
    self, d as df, h, BLOCK_ENTRY_SIZE, DOC_RECORD_SIZE, HEADER_SIZE, LIST_SEPARATOR,
};
use crate::tokenize::{dedup_preserve_order, is_cjk_like, normalize, normalize_cow, tokenize};

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
///
/// 词条缓冲在整个游标生命周期内复用：`advance()` 只做 `truncate + push_str`，
/// 不再为每个词条新建 `String` 并整词克隆。词典扫描是查询热路径
/// （前缀展开一轮可达数千词条），这里的分配次数直接决定查询延迟。
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

    /// 前进到下一条记录，返回（postings 字节长度, doc_freq）；成功后 `term()` 给出词条。
    ///
    /// 拆成 `advance` + `term` 两步而不是返回 `String`，是为了让借用在这里结束——
    /// 调用方拿到比较结果（`Ordering`/`bool`）后即可继续前进，无需复制词条。
    fn advance(&mut self) -> Option<(u32, u32)> {
        if self.pos >= self.end {
            return None;
        }
        // 先取出切片引用，后续对 self.prev 的可变借用互不干扰
        let data = self.data;
        let shared = format::read_varint(data, &mut self.pos)? as usize;
        let suffix_len = format::read_varint(data, &mut self.pos)? as usize;
        let suffix_end = self.pos.checked_add(suffix_len)?;
        let suffix = std::str::from_utf8(data.get(self.pos..suffix_end)?).ok()?;
        self.pos = suffix_end;

        let postings_len = format::read_varint(data, &mut self.pos)?;
        let doc_freq = format::read_varint(data, &mut self.pos)?;

        if shared > self.prev.len() || !self.prev.is_char_boundary(shared) {
            return None;
        }
        self.prev.truncate(shared);
        self.prev.push_str(suffix);
        Some((postings_len, doc_freq))
    }

    fn term(&self) -> &str {
        &self.prev
    }
}

/// 文档集合的位图（`doc_count` 位）。
///
/// 为什么不用 `HashSet<u32>` / `Vec<u32>`：
/// - 求交、求并是 `doc_count/64` 次字操作（本站 144 篇 = **3 个 u64**），与集合大小无关；
/// - 位图天然去重且按 id 升序，省掉候选集的 `sort_unstable + dedup`；
/// - 从 postings 直接置位，不需要「先解出 `Vec<u32>` 再合并」的中间分配。
#[derive(Clone)]
struct DocBits {
    words: Vec<u64>,
}

impl DocBits {
    fn new(doc_count: u32) -> Self {
        Self {
            words: vec![0u64; (doc_count as usize).div_ceil(64)],
        }
    }

    /// 全集（doc_count 位全 1，末字多余的位保持 0）
    fn full(doc_count: u32) -> Self {
        let mut bits = Self::new(doc_count);
        if doc_count == 0 {
            return bits;
        }
        let last = bits.words.len() - 1;
        for slot in bits.words[..last].iter_mut() {
            *slot = u64::MAX;
        }
        let remainder = doc_count % 64;
        bits.words[last] = if remainder == 0 {
            u64::MAX
        } else {
            (1u64 << remainder) - 1
        };
        bits
    }

    #[inline]
    fn insert(&mut self, id: u32) {
        let (word, bit) = (id as usize / 64, id as usize % 64);
        if let Some(slot) = self.words.get_mut(word) {
            *slot |= 1u64 << bit;
        }
    }

    #[inline]
    fn and_assign(&mut self, other: &Self) {
        for (slot, rhs) in self.words.iter_mut().zip(other.words.iter()) {
            *slot &= *rhs;
        }
    }

    #[inline]
    fn or_assign(&mut self, other: &Self) {
        for (slot, rhs) in self.words.iter_mut().zip(other.words.iter()) {
            *slot |= *rhs;
        }
    }

    fn count(&self) -> usize {
        self.words.iter().map(|slot| slot.count_ones() as usize).sum()
    }

    fn is_empty(&self) -> bool {
        self.words.iter().all(|slot| *slot == 0)
    }

    /// 按 id 升序迭代（与构建期的文档顺序一致，即日期降序）
    fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.words.iter().enumerate().flat_map(|(index, slot)| {
            let base = (index * 64) as u32;
            let mut bits = *slot;
            std::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let offset = bits.trailing_zeros();
                bits &= bits - 1;
                Some(base + offset)
            })
        })
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
        let postings_len = read(h::POSTINGS_LEN)? as usize;
        let docs_off = read(h::DOCS_OFF)? as usize;
        let docs_len = read(h::DOCS_LEN)? as usize;
        let pool_off = read(h::POOL_OFF)? as usize;
        let facets_off = read(h::FACETS_OFF)? as usize;
        let facets_len = read(h::FACETS_LEN)? as usize;
        let doc_count = read(h::DOC_COUNT)?;
        let block_count = read(h::BLOCK_COUNT)?;

        let len = data.len();
        // 段长度一律用 checked 乘法：wasm32 的 usize 只有 32 位，
        // 头部里的 block_count / doc_count 能让 `count * 16` 回绕，从而绕过越界校验
        let blocks_len = (block_count as usize)
            .checked_mul(BLOCK_ENTRY_SIZE)
            .ok_or_else(|| format!("blocks 段长度溢出：block_count={block_count}"))?;
        let docs_needed = (doc_count as usize)
            .checked_mul(DOC_RECORD_SIZE)
            .ok_or_else(|| format!("文档表长度溢出：doc_count={doc_count}"))?;

        for (name, off, size) in [
            ("terms", terms_off, terms_len),
            ("blocks", blocks_off, blocks_len),
            ("postings", postings_off, postings_len),
            ("docs", docs_off, docs_len),
            ("pool", pool_off, 0usize),
            ("facets", facets_off, facets_len),
        ] {
            let fits = off <= len && off.checked_add(size).is_some_and(|end| end <= len);
            if !fits {
                return Err(format!("{name} 段越界：off={off} size={size} len={len}"));
            }
        }
        if docs_len < docs_needed {
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
            format::read_u32(&self.data, base)?.checked_add(self.terms_off as u32)? as usize,
            format::read_u32(&self.data, base + 4)?,
            format::read_u32(&self.data, base + 8)?
                .checked_add(self.postings_off as u32)? as usize,
        ))
    }

    /// 块内首词与 `term` 的比较结果。
    ///
    /// 二分查找只需要比较，不需要把首词**构造出来**——旧实现为此每个二分步都新建一个 String。
    fn block_first_term_cmp(&self, block: u32, term: &str) -> Option<Ordering> {
        let (term_off, count, _) = self.block_entry(block)?;
        if count == 0 {
            return None;
        }
        let mut cursor = TermCursor::new(&self.data, self.terms_off, self.terms_len, term_off);
        cursor.advance()?;
        Some(cursor.term().cmp(term))
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
            let first_le = matches!(
                self.block_first_term_cmp(mid, term),
                Some(Ordering::Less | Ordering::Equal)
            );
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

    fn postings_slice(&self, pos: usize, len: u32) -> Option<&[u8]> {
        let end = pos.checked_add(len as usize)?;
        self.data.get(pos..end)
    }

    /// 把某个词条的 postings 直接解码进位图（delta + varint，升序）。
    ///
    /// 越界或截断时返回 false，调用方按「该词条无命中」处理——索引损坏时宁可少结果，
    /// 也不能在 wasm 里 panic（`panic = "abort"` 会 trap 掉整个 worker）。
    fn postings_into(&self, pos: usize, len: u32, out: &mut DocBits) -> bool {
        let Some(bytes) = self.postings_slice(pos, len) else {
            return false;
        };
        let mut cursor = 0usize;
        let mut last = 0u32;
        while let Some(delta) = format::read_varint(bytes, &mut cursor) {
            last = last.saturating_add(delta);
            out.insert(last);
        }
        true
    }

    /// 精确查词 → (postings 绝对偏移, postings 字节长度, doc_freq)
    fn exact_postings(&self, term: &str) -> Option<(usize, u32, u32)> {
        let block = self.locate_block(term)?;
        let (term_off, count, block_postings) = self.block_entry(block)?;
        let mut cursor = TermCursor::new(&self.data, self.terms_off, self.terms_len, term_off);
        let mut postings_pos = block_postings;
        for _ in 0..count {
            let (postings_len, doc_freq) = cursor.advance()?;
            match cursor.term().cmp(term) {
                Ordering::Equal => return Some((postings_pos, postings_len, doc_freq)),
                Ordering::Greater => return None,
                Ordering::Less => {}
            }
            postings_pos = postings_pos.checked_add(postings_len as usize)?;
        }
        None
    }

    /// 前缀展开：把「所有以 `prefix` 开头的词条」的 postings 并入 `out`。
    ///
    /// 精确命中的那个词条由 `exact_postings` 负责，这里跳过它以免重复解码。
    fn prefix_postings(&self, prefix: &str, budget: &mut usize, out: &mut DocBits) {
        let Some(start_block) = self.locate_block(prefix) else {
            return;
        };
        for block in start_block..self.block_count {
            let Some((first_term_off, count, block_postings)) = self.block_entry(block) else {
                break;
            };
            let mut cursor =
                TermCursor::new(&self.data, self.terms_off, self.terms_len, first_term_off);
            let mut postings_pos = block_postings;
            for _ in 0..count {
                let Some((postings_len, _)) = cursor.advance() else {
                    break;
                };
                let this_pos = postings_pos;
                let Some(next_pos) = postings_pos.checked_add(postings_len as usize) else {
                    return;
                };
                postings_pos = next_pos;

                let term = cursor.term();
                if !term.starts_with(prefix) {
                    // 词条有序：越过前缀区间即可整体收工
                    if term > prefix {
                        return;
                    }
                    continue;
                }
                // 与 prefix 等长的就是精确命中那条，已经在 out 里了
                if term.len() == prefix.len() {
                    continue;
                }
                if *budget == 0 {
                    return;
                }
                *budget -= 1;
                self.postings_into(this_pos, postings_len, out);
            }
        }
    }

    /// 一个 token 的候选文档集合。
    ///
    /// - 精确命中先用
    /// - 最后一个 token（自动补全语义）与单字符 CJK token（索引里只有 bigram）额外做前缀展开
    fn token_docs(&self, token: &str, is_last: bool, out: &mut DocBits) {
        if let Some((pos, len, _)) = self.exact_postings(token) {
            self.postings_into(pos, len, out);
        }

        let single_cjk = token.chars().count() == 1 && token.chars().all(is_cjk_like);
        if is_last || single_cjk {
            let mut budget = MAX_PREFIX_TERMS;
            self.prefix_postings(token, &mut budget, out);
        }
    }

    /// 分面 id 列表 → 位图
    fn bits_of(&self, ids: &[u32]) -> DocBits {
        let mut bits = DocBits::new(self.doc_count);
        for &id in ids {
            bits.insert(id);
        }
        bits
    }

    /// 范围过滤：返回受限文档位图；两个都不给则 None（不限定）。
    ///
    /// 查不到的分面（改名/删文后的旧标签）返回**空**位图，语义与「该标签下没有文档」一致。
    fn facet_docs(&self, tag: Option<&str>, category: Option<&str>) -> Option<DocBits> {
        if tag.is_none() && category.is_none() {
            return None;
        }
        // 分面名在构建期已归一化；查询值大多也已是归一化形态，normalize_cow 免去分配
        let mut bits = DocBits::full(self.doc_count);
        if let Some(name) = tag {
            let ids = self.tags.get(normalize_cow(name).as_ref());
            bits.and_assign(&self.bits_of(ids.map_or(&[][..], |v| v.as_slice())));
        }
        if let Some(name) = category {
            let ids = self.categories.get(normalize_cow(name).as_ref());
            bits.and_assign(&self.bits_of(ids.map_or(&[][..], |v| v.as_slice())));
        }
        Some(bits)
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
        let normalized_query = normalize(keywords);
        let query_text = normalized_query.trim();
        let scope = self.facet_docs(tag, category);

        // 只限定范围、没有关键词：按文档原序（构建期已按日期降序）返回，摘要留空
        if query_text.is_empty() {
            let Some(scope) = scope else {
                return Vec::new();
            };
            return scope
                .iter()
                .take(limit)
                .filter_map(|id| self.build_hit(id, None, 0, false))
                .collect();
        }

        let tokens = dedup_preserve_order(tokenize(query_text));
        if tokens.is_empty() {
            return Vec::new();
        }

        // 每个 token 的候选位图（已按范围过滤）
        let mut sets: Vec<DocBits> = Vec::new();
        for (i, &token) in tokens.iter().enumerate() {
            let is_last = i + 1 == tokens.len();
            let mut bits = DocBits::new(self.doc_count);
            self.token_docs(token, is_last, &mut bits);
            if let Some(scope) = &scope {
                bits.and_assign(scope);
            }
            if !bits.is_empty() {
                sets.push(bits);
            }
        }
        if sets.is_empty() {
            return Vec::new();
        }

        // 先试 AND（所有 token 都命中）；为空则退化为 OR，由打分阶段按命中 token 数排序
        let mut candidates = DocBits::new(self.doc_count);
        if sets.len() == tokens.len() {
            candidates = sets[0].clone();
            for set in &sets[1..] {
                candidates.and_assign(set);
                if candidates.is_empty() {
                    break;
                }
            }
        }
        if candidates.is_empty() {
            for set in &sets {
                candidates.or_assign(set);
                // 上限实际生效：旧实现在内层 break，外层仍会继续并集
                if candidates.count() >= MAX_CANDIDATES {
                    break;
                }
            }
        }

        // 打分：字段命中权重（字段级判断用原文 `contains`，候选集只有几十个，成本可忽略）
        let mut scored: Vec<(
            u32,  /*matched*/
            u32,  /*score*/
            u32,  /*doc_id*/
            bool, /*exact*/
        )> = Vec::with_capacity(candidates.count());
        for id in candidates.iter() {
            let Some(view) = self.doc(id) else {
                continue;
            };
            // 借用版归一化：语料本来无需改写时不分配（旧实现在这里为每个候选复制四份文本）
            let n_title = normalize_cow(view.title);
            let n_tags = normalize_cow(view.tags);
            let n_cats = normalize_cow(view.categories);
            let n_text = normalize_cow(view.text);

            let mut matched = 0u32;
            let mut score = 0u32;
            for &token in &tokens {
                let mut token_matched = false;
                if n_title.contains(token) {
                    score += WEIGHT_TITLE;
                    token_matched = true;
                }
                if n_tags.contains(token) {
                    score += WEIGHT_TAG;
                    token_matched = true;
                }
                if n_cats.contains(token) {
                    score += WEIGHT_CATEGORY;
                    token_matched = true;
                }
                if n_text.contains(token) {
                    score += WEIGHT_TEXT;
                    token_matched = true;
                }
                if token_matched {
                    matched += 1;
                }
            }
            let exact = n_title.contains(query_text)
                || n_text.contains(query_text)
                || n_tags.contains(query_text)
                || n_cats.contains(query_text);
            if exact {
                score += BONUS_EXACT;
            }
            scored.push((matched, score, id, exact));
        }

        // 命中 token 多者优先 → 分数高者优先 → 文档原序（日期降序）
        let by_rank = |a: &(u32, u32, u32, bool), b: &(u32, u32, u32, bool)| {
            b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2))
        };
        // 只要前 limit 条：先 O(n) 选择再对入选者排序（原实现是全量 O(n log n)）
        if scored.len() > limit {
            scored.select_nth_unstable_by(limit, by_rank);
            scored.truncate(limit);
        }
        scored.sort_by(by_rank);

        let first_token = tokens.first().copied().unwrap_or("");
        scored
            .into_iter()
            .filter_map(|(_, score, id, exact)| {
                let needle = if exact { query_text } else { first_token };
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
            .map(|needle| build_snippet(view.text, needle))
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

/// 摘要：在正文里定位 needle，再把「字符下标」映射回原文切窗口。
///
/// 成立的前提是 `normalize` 逐字符 1:1（不增删字符），见 `tokenize::normalize` 的说明。
///
/// 性能要点（每条命中都会跑，命中多时是查询的主要开销）：
/// 1. **原文优先**：needle 直接出现在原文里就不再归一化（纯中文语料几乎总成立），
///    省掉一次「是否需要改写」的全串扫描 + 一次分配；
/// 2. 只用一趟 `chars()` 迭代取窗口，并用「迭代器还有没有下一个」判断要不要加尾省略号，
///    不再先 `chars().count()` 数一遍总长（旧实现每条命中要 4 趟字符遍历）。
fn build_snippet(original: &str, needle: &str) -> String {
    if needle.is_empty() || original.is_empty() {
        return head_of(original, SNIPPET_HEAD_CHARS);
    }

    // 慢路径的归一化结果需要活得比 haystack 借用更久，故在此提前声明
    let normalized;
    let (haystack, byte_idx) = match original.find(needle) {
        Some(idx) => (original, idx),
        None => {
            normalized = normalize(original);
            match normalized.find(needle) {
                Some(idx) => (normalized.as_str(), idx),
                None => return head_of(original, SNIPPET_HEAD_CHARS),
            }
        }
    };

    let hit_char_idx = haystack[..byte_idx].chars().count();
    let needle_chars = needle.chars().count();
    let start = hit_char_idx.saturating_sub(SNIPPET_WINDOW_CHARS);
    let want = hit_char_idx + needle_chars + SNIPPET_WINDOW_CHARS - start;

    let mut chars = original.chars();
    let body: String = chars.by_ref().skip(start).take(want).collect();
    let more_after = chars.next().is_some();

    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        body,
        if more_after { "…" } else { "" }
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
    // 计数来自文件、最多可到 u32::MAX；直接用它 with_capacity 会让一个约 100 字节的
    // 构造文件申请几十 GB，分配失败直接 abort（wasm 里就是 trap，JS 侧连 Err 都拿不到）。
    // 每个分面至少占 2 字节，段长天然是计数的上界。
    if count as usize > section.len() {
        return Err(format!(
            "分面表计数越界：count={count} 段长={}",
            section.len()
        ));
    }
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
        // 同上：每个 doc id 至少 1 字节
        if doc_count as usize > section.len() {
            return Err(format!(
                "分面文档数越界：doc_count={doc_count} 段长={}",
                section.len()
            ));
        }
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
