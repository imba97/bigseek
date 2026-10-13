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

/// 归一化：逐字符 1:1 替换，输出与输入**字符数相同**。
///
/// - `U+3000`（全角空格）→ `' '`
/// - `U+FF01..=U+FF5E`（全角 ASCII）→ 对应半角
/// - ASCII `A..=Z` → 小写（只折 ASCII，避免 `İ` 这类展开成多个字符破坏 1:1）
pub fn normalize(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        let mapped = match ch {
            '\u{3000}' => ' ',
            '\u{FF01}'..='\u{FF5E}' => char::from_u32(ch as u32 - 0xFEE0).unwrap_or(ch),
            'A'..='Z' => ch.to_ascii_lowercase(),
            other => other,
        };
        out.push(mapped);
    }
    out
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
/// ```
/// use blog_search::tokenize::{normalize, tokenize};
/// let toks = tokenize(&normalize("性能优化 Vue3 指南"));
/// assert_eq!(toks, ["性能", "能优", "优化", "vue3", "指南"]);
/// ```
pub fn tokenize(normalized: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut run: Vec<char> = Vec::new();
    let mut run_is_cjk = false;

    fn flush(run: &mut Vec<char>, run_is_cjk: bool, out: &mut Vec<String>) {
        if run.is_empty() {
            return;
        }
        if run_is_cjk {
            if run.len() == 1 {
                out.push(run[0].to_string());
            } else {
                for w in run.windows(2) {
                    out.push(w.iter().collect());
                }
            }
        } else {
            let mut cur = String::new();
            for &c in run.iter() {
                if c.is_alphanumeric() {
                    cur.push(c);
                } else if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            if !cur.is_empty() {
                out.push(cur);
            }
        }
        run.clear();
    }

    for ch in normalized.chars() {
        let cjk = is_cjk_like(ch);
        if !run.is_empty() && cjk != run_is_cjk {
            flush(&mut run, run_is_cjk, &mut out);
        }
        run_is_cjk = cjk;
        run.push(ch);
    }
    flush(&mut run, run_is_cjk, &mut out);
    out
}

/// 去重且保持首次出现顺序；索引期用（同一文档里同词只记一次 posting）。
pub fn dedup_preserve_order(tokens: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::with_capacity(tokens.len());
    let mut out = Vec::with_capacity(tokens.len());
    for t in tokens {
        if seen.insert(t.clone()) {
            out.push(t);
        }
    }
    out
}
