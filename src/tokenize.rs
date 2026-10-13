//! 归一化与分词：**索引期与查询期共用同一份实现**，这是本库存在的核心理由之一。
//!
//! 设计要点（与 `imba97-blog-vite` 的 TS 侧实现必须逐字对齐，见 `testdata/tokenize-vectors.json`）：
//!
//! 1. 归一化是**逐字符 1:1 替换**（全角→半角、全角空格→空格、ASCII 大写→小写），
//!    不插入也不删除字符。这样「归一化文本里的字符下标」可以直接映射回原文，
//!    摘要（snippet）不需要额外的位置表。代价是不做完整 NFKC（见下）。
//! 2. 中文（以及假名、谚文这类无空格分词文字）用 **bigram**：`性能优化` → `性能`/`能优`/`优化`。
//!    单字片段退化为 unigram。查询侧对单个 CJK 字符走词典前缀展开，保证单字可搜。
//! 3. 拉丁/数字片段按 `char::is_alphanumeric` 切分并保留数字，`vue3` → `vue3`（不拆）。
//!
//! 为什么不做完整 NFKC：`unicode-normalization` 的表会让 wasm 体积增加几十 KB，
//! 而本站语料里真正出现的等价形式只有全角 ASCII、全角空格和英文大小写。若日后需要
//! （繁体、组合字符），在 `normalize` 里补一张映射表即可，格式版本号 +1。

use std::borrow::Cow;

/// 归一化：逐字符 1:1 替换，输出与输入**字符数相同**。
///
/// - `U+3000`（全角空格）→ `' '`
/// - `U+FF01..=U+FF5E`（全角 ASCII）→ 对应半角
/// - ASCII `A..=Z` → 小写（只折 ASCII，避免 `İ` 这类展开成多个字符破坏 1:1）
pub fn normalize(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    normalize_into(input, &mut out);
    out
}

/// 归一化并**追加**到已有缓冲：构建期把多个字段拼进同一个缓冲时用，省掉中间 `String`。
pub fn normalize_into(input: &str, out: &mut String) {
    out.reserve(input.len());
    for ch in input.chars() {
        out.push(map_char(ch));
    }
}

/// 会被 `normalize` 改写的字符判定。纯中文/已小写 ASCII 语料里几乎恒为 false。
#[inline]
fn needs_rewrite(ch: char) -> bool {
    matches!(ch, '\u{3000}' | '\u{FF01}'..='\u{FF5E}' | 'A'..='Z')
}

#[inline]
fn map_char(ch: char) -> char {
    match ch {
        '\u{3000}' => ' ',
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(ch as u32 - 0xFEE0).unwrap_or(ch),
        'A'..='Z' => ch.to_ascii_lowercase(),
        other => other,
    }
}

/// 归一化的借用版：**无需改写时直接返回入参**，不改写就不分配。
///
/// 查询热路径专用：打分阶段每个候选都要归一化标题/标签/分类/正文，
/// 而这四份文本绝大多数本来就已经是归一化形态（纯中文、小写英文），
/// 无条件 `String` 复制等于每次击键把整份候选语料再抄一遍。
pub fn normalize_cow(input: &str) -> Cow<'_, str> {
    if input.chars().any(needs_rewrite) {
        Cow::Owned(normalize(input))
    } else {
        Cow::Borrowed(input)
    }
}

/// 无空格分词文字（CJK 表意文字、假名、谚文音节）：这些字符参与 bigram 切分。
///
/// 范围与 TS 侧 `isCjkLike` 完全一致（不使用 `\p{Script=Han}`，避免两边 Unicode 版本差异）。
pub fn is_cjk_like(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3040..=0x30FF      // 平假名 U+3040-U+309F + 片假名 U+30A0-U+30FF
            | 0x3400..=0x4DBF   // CJK 扩展 A
            | 0x4E00..=0x9FFF   // CJK 基本区
            | 0xAC00..=0xD7AF   // 谚文音节
            | 0xF900..=0xFAFF   // CJK 兼容表意文字
            | 0x20000..=0x2FA1F // CJK 扩展 B~F + 兼容补充
    )
}

/// 分词。输入应当已 `normalize`（本函数不再做大小写折叠，便于测试对齐）。
///
/// 返回的是**原文的借用切片**：bigram 本身就是相邻两字符的连续区间，
/// 拉丁词也是连续区间，所以这里不需要任何堆分配（旧实现为每个 token 建一个 `String`，
/// 索引期 24.5 万次、查询期每 token 一次）。
///
/// ```
/// use bigseek::tokenize::{normalize, tokenize};
/// let normalized = normalize("性能优化 Vue3 指南");
/// let toks = tokenize(&normalized);
/// assert_eq!(toks, ["性能", "能优", "优化", "vue3", "指南"]);
/// ```
pub fn tokenize(normalized: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    let mut run_start: Option<usize> = None;
    let mut run_is_cjk = false;

    for (idx, ch) in normalized.char_indices() {
        let cjk = is_cjk_like(ch);
        match run_start {
            // 字符类别切换：结算上一段
            Some(start) if cjk != run_is_cjk => {
                push_run(&normalized[start..idx], run_is_cjk, &mut out);
                run_start = Some(idx);
            }
            None => run_start = Some(idx),
            _ => {}
        }
        run_is_cjk = cjk;
    }
    if let Some(start) = run_start {
        push_run(&normalized[start..], run_is_cjk, &mut out);
    }
    out
}

/// 结算一个同类别片段。CJK 段出 bigram（单字段出 unigram），拉丁段按 `is_alphanumeric` 切连续区间。
fn push_run<'a>(run: &'a str, is_cjk: bool, out: &mut Vec<&'a str>) {
    if run.is_empty() {
        return;
    }

    if is_cjk {
        if run.chars().count() == 1 {
            out.push(run);
            return;
        }
        // 相邻两字符的区间 = 前一字符起点 .. 后一字符终点（= 再下一个字符的起点或段末）
        let mut iter = run.char_indices().peekable();
        let mut prev: Option<usize> = None;
        while let Some((idx, _)) = iter.next() {
            if let Some(start) = prev {
                let end = iter.peek().map_or(run.len(), |(next, _)| *next);
                out.push(&run[start..end]);
            }
            prev = Some(idx);
        }
    } else {
        let mut start: Option<usize> = None;
        for (idx, ch) in run.char_indices() {
            if ch.is_alphanumeric() {
                if start.is_none() {
                    start = Some(idx);
                }
            } else if let Some(from) = start.take() {
                out.push(&run[from..idx]);
            }
        }
        if let Some(from) = start {
            out.push(&run[from..]);
        }
    }
}

/// 去重且保持首次出现顺序；索引期与查询期都用（同一文档里同词只记一次 posting）。
///
/// 接收借用切片，返回的仍是借用切片——不再为每个唯一 token 克隆一份 `String`。
pub fn dedup_preserve_order(tokens: Vec<&str>) -> Vec<&str> {
    let mut seen = std::collections::HashSet::with_capacity(tokens.len());
    let mut out = Vec::with_capacity(tokens.len());
    for token in tokens {
        if seen.insert(token) {
            out.push(token);
        }
    }
    out
}
