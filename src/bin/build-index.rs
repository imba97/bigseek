//! 构建期 CLI：把博客生成的 `search-full.json` 转成紧凑的 `search.idx`，并可选做召回自检。
//!
//! 刻意**不**在这个二进制里解析 markdown / frontmatter：文档抽取（哪些文章可发布、id 怎么算、
//! 日期怎么格式化、正文怎么去 HTML）仍由博客的 TS 侧负责，那是 `post-policy` 的唯一真源。
//! 这里只做「结构化文档 → 二进制索引」，避免两套发布规则漂移。
//!
//! ```bash
//! cargo run --release --features cli --bin build-index -- \
//!   --input public/search-full.json --output public/search/search.idx --verify
//! ```
//!
//! `--verify` 会在写完索引后立刻加载它，从语料里自动抽样中文词做**文档级召回**自检，
//! 低于阈值直接以非 0 退出——这样每次博客构建都会跑一次真实质量门禁。

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use bigseek::build::{build_index, indexed_text, SearchDocument};
use bigseek::posts::load_posts;
use bigseek::tokenize::is_cjk_like;
use bigseek::{load_index, SEARCH_LIMIT_MAX};

/// 召回自检阈值（百分比）
const RECALL_THRESHOLD: f64 = 95.0;
/// 自检抽样词数
const VERIFY_SAMPLE_SIZE: usize = 40;
/// 抽样词长度（CJK 字符数）
const VERIFY_TERM_CHARS: usize = 2;
/// 只保留「跨文档重复出现」的候选词：真实词倾向于复现，跨词边界的碎片通常只出现一次
const VERIFY_MIN_DOC_FREQ: usize = 2;

#[derive(Debug, serde::Deserialize)]
struct JsonRecord {
    #[serde(default)]
    path: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    date: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    categories: Vec<String>,
    #[serde(default)]
    text: String,
}

struct Args {
    /// 直接从文章目录读取（推荐）
    posts_dir: Option<PathBuf>,
    /// 读取博客 TS 侧产出的 search-full.json（回归基准 / 兼容旧流程）
    input: Option<PathBuf>,
    output: PathBuf,
    quiet: bool,
    verify: bool,
}

fn usage() -> &'static str {
    "用法: build-index (--posts-dir <posts/> | --input <search-full.json>) --output <search.idx> [--verify] [--quiet]"
}

fn parse_args() -> Result<Args, String> {
    let mut posts_dir: Option<PathBuf> = None;
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut quiet = false;
    let mut verify = false;
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--posts-dir" => posts_dir = argv.next().map(PathBuf::from),
            "--input" | "-i" => input = argv.next().map(PathBuf::from),
            "--output" | "-o" => output = argv.next().map(PathBuf::from),
            "--quiet" | "-q" => quiet = true,
            "--verify" => verify = true,
            "--help" | "-h" => return Err(usage().to_string()),
            other => return Err(format!("未知参数：{other}\n{}", usage())),
        }
    }
    match (&posts_dir, &input) {
        (Some(_), Some(_)) => {
            return Err(format!("--posts-dir 与 --input 只能给一个\n{}", usage()))
        }
        (None, None) => return Err(format!("必须给 --posts-dir 或 --input\n{}", usage())),
        _ => {}
    }
    Ok(Args {
        posts_dir,
        input,
        output: output.ok_or_else(|| format!("缺少 --output\n{}", usage()))?,
        quiet,
        verify,
    })
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("[build-index] {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let started = Instant::now();

    let (docs, source_label) = if let Some(posts_dir) = &args.posts_dir {
        let (docs, report) = load_posts(posts_dir)?;
        if !args.quiet {
            println!(
                "[build-index] 扫描 {}：{}",
                posts_dir.display(),
                report.summary()
            );
        }
        if docs.is_empty() {
            return Err(format!("{} 下没有可发布的文章", posts_dir.display()));
        }
        (docs, format!("{}/（直读文章）", posts_dir.display()))
    } else {
        let input = args.input.as_ref().expect("parse_args 已保证二者必有其一");
        let raw = std::fs::read_to_string(input)
            .map_err(|e| format!("读取 {} 失败：{e}", input.display()))?;
        let records: Vec<JsonRecord> = serde_json::from_str(&raw)
            .map_err(|e| format!("解析 {} 失败：{e}", input.display()))?;
        if records.is_empty() {
            return Err(format!("{} 里没有任何文档", input.display()));
        }
        let docs = records
            .into_iter()
            .map(|r| SearchDocument {
                path: r.path,
                title: r.title,
                date: r.date,
                tags: r.tags,
                categories: r.categories,
                text: r.text,
            })
            .collect();
        (docs, format!("{}（JSON 输入）", input.display()))
    };

    let (bytes, stats) = build_index(&docs);
    if let Some(parent) = args.output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录 {} 失败：{e}", parent.display()))?;
        }
    }
    std::fs::write(&args.output, &bytes)
        .map_err(|e| format!("写入 {} 失败：{e}", args.output.display()))?;

    if !args.quiet {
        println!(
            "[build-index] {source_label} → {}（{}）耗时 {} ms",
            args.output.display(),
            stats.summary(),
            started.elapsed().as_millis()
        );
        println!(
            "[build-index] 索引 {:.0} KiB（词典 {:.0} / postings {:.0} / 文档表 {:.0} / 字符串池 {:.0} / 分面 {:.0} KiB）",
            stats.total_bytes as f64 / 1024.0,
            stats.terms_bytes as f64 / 1024.0,
            stats.postings_bytes as f64 / 1024.0,
            stats.docs_bytes as f64 / 1024.0,
            stats.pool_bytes as f64 / 1024.0,
            stats.facets_bytes as f64 / 1024.0,
        );
    }

    if args.verify {
        // 索引字节直接移交自检（它只需要读一次），避免再复制一份 654 KiB
        verify_recall(bytes, &docs, args.quiet)?;
    }
    Ok(())
}

/// 文档级召回自检：从语料自动抽真实中文词，逐个查索引，统计「应命中的文档有多少被找回」。
///
/// 抽样策略：取正文里所有长度为 `VERIFY_TERM_CHARS` 的 CJK 子串，按**被多少个不同文档包含**降序，
/// 只保留出现于 ≥2 篇文档的候选（真实词会复现，跨词边界的碎片一般不会），再取前 N 个。
/// 这样不需要人工维护词表，也能在语料更新后继续生效。
fn verify_recall(index_bytes: Vec<u8>, docs: &[SearchDocument], quiet: bool) -> Result<(), String> {
    let index = load_index(index_bytes).map_err(|e| format!("自检加载索引失败：{e}"))?;

    // 与索引构建共用同一套「文档 → 归一化文本」：自检的 ground truth 不能比索引窄
    let normalized: Vec<String> = docs.iter().map(indexed_text).collect();

    // 候选词 → (最后出现的 doc id, 出现于多少个不同文档)。
    // key 用 (char, char) 而不是 String：词长固定为 2，于是约 50 万个窗口**零分配**
    // （旧实现为每个窗口建一个 String 再插 BTreeSet，自检比建索引本身还慢）。
    debug_assert_eq!(VERIFY_TERM_CHARS, 2, "直方图 key 假定抽样词长恰为 2");
    let mut freq: HashMap<(char, char), (u32, u32)> = HashMap::new();
    for (doc_id, text) in normalized.iter().enumerate() {
        let doc_id = doc_id as u32;
        let mut prev: Option<char> = None;
        for ch in text.chars() {
            if let Some(left) = prev {
                if is_cjk_like(left) && is_cjk_like(ch) {
                    // 首次出现就记 1，不要写成 0：`or_insert` 之后紧接着的
                    // `entry.0 != doc_id` 对刚插入的条目为假，会把这一篇漏掉，
                    // 于是出现于 N 篇的词条只记 N-1 次——小语料上整批候选被
                    // VERIFY_MIN_DOC_FREQ 滤掉，自检直接报「抽样为空」。
                    let entry = freq.entry((left, ch)).or_insert((doc_id, 1));
                    // 文档是逐篇扫的：同一篇里重复出现只算一次
                    if entry.0 != doc_id {
                        entry.0 = doc_id;
                        entry.1 += 1;
                    }
                }
            }
            prev = Some(ch);
        }
    }

    let mut candidates: Vec<((char, char), u32)> = freq
        .into_iter()
        .filter(|(_, (_, count))| *count as usize >= VERIFY_MIN_DOC_FREQ)
        .map(|(pair, (_, count))| (pair, count))
        .collect();
    // 出现文档数多的优先（更可能是真实词）；同频按 (char, char) 升序，
    // 对 2 字符而言该顺序与 UTF-8 字节序一致，确定性不变
    candidates.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let sample: Vec<(String, usize)> = candidates
        .into_iter()
        .take(VERIFY_SAMPLE_SIZE)
        .map(|((left, right), count)| (format!("{left}{right}"), count as usize))
        .collect();
    if sample.is_empty() {
        return Err("自检抽样为空：语料太小或没有重复出现的中文词".to_string());
    }

    let mut expected = 0usize;
    let mut found = 0usize;
    let mut worst: Vec<(String, usize, usize)> = Vec::new();
    for (term, _) in &sample {
        let needle = term.as_str();
        let want: Vec<usize> = normalized
            .iter()
            .enumerate()
            .filter(|(_, text)| text.contains(needle))
            .map(|(i, _)| i)
            .collect();
        if want.is_empty() {
            continue;
        }
        let hits = index.query(term, None, None, SEARCH_LIMIT_MAX);
        let got = want
            .iter()
            .filter(|&&i| hits.iter().any(|h| h.path == docs[i].path))
            .count();
        expected += want.len();
        found += got;
        if got < want.len() {
            worst.push((term.clone(), got, want.len()));
        }
    }

    let recall = if expected == 0 {
        0.0
    } else {
        found as f64 / expected as f64 * 100.0
    };
    if !quiet {
        println!(
            "[build-index] 召回自检：抽样 {} 词，文档级召回 {found}/{expected} = {recall:.1}%",
            sample.len()
        );
        if !worst.is_empty() {
            let detail = worst
                .iter()
                .take(8)
                .map(|(t, got, want)| format!("{t}({got}/{want})"))
                .collect::<Vec<_>>()
                .join(" ");
            println!("[build-index] 未完全命中：{detail}");
        }
    }

    if recall < RECALL_THRESHOLD {
        return Err(format!(
            "召回自检未通过：{recall:.1}% < {RECALL_THRESHOLD:.0}%。分词或索引写入很可能坏了，请勿发布此索引"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(path: &str, title: &str, text: &str) -> SearchDocument {
        SearchDocument {
            path: path.to_string(),
            title: title.to_string(),
            date: "2026-01-01".to_string(),
            tags: Vec::new(),
            categories: Vec::new(),
            text: text.to_string(),
        }
    }

    /// 直方图必须按「出现于多少个**不同**文档」计数。
    ///
    /// 这条测试来自一次真实故障：首次出现被初始化成 0，于是出现于 N 篇的词条只记 N-1 次，
    /// 4 篇的文章 fixture 上候选被全部滤掉，`--verify` 直接报「抽样为空」。
    #[test]
    fn histogram_counts_distinct_documents() {
        let docs = vec![
            doc("/a", "性能优化指南", "性能优化最开始的写法"),
            doc("/b", "性能优化实践", "性能优化 又一次"),
            doc("/c", "性能优化", "还是性能优化"),
        ];
        let (bytes, _) = build_index(&docs);

        // 出现于 3 篇的词条必须留下候选：自检要能跑通，而不是「抽样为空」
        verify_recall(bytes, &docs, true).expect("三篇共享 bigram 时应能完成自检");
    }

    /// 同一篇里重复出现只算一次，不能把词频当成文档数
    #[test]
    fn histogram_ignores_repeats_within_one_document() {
        let docs = vec![
            doc("/a", "孤独词条", "孤独词条 孤独词条 孤独词条 孤独词条"),
            doc("/b", "另一篇", "完全无关的内容"),
        ];
        let (bytes, _) = build_index(&docs);

        // 「孤独」只出现在 1 篇里，低于 VERIFY_MIN_DOC_FREQ，没有可抽样词
        let err = verify_recall(bytes, &docs, true).unwrap_err();
        assert!(err.contains("抽样为空"), "实际：{err}");
    }
}
