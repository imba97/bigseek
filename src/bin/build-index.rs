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

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use blog_search::build::{build_index, SearchDocument};
use blog_search::tokenize::{is_cjk_like, normalize};
use blog_search::{load_index, SEARCH_LIMIT_MAX};

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
    input: PathBuf,
    output: PathBuf,
    quiet: bool,
    verify: bool,
}

fn usage() -> &'static str {
    "用法: build-index --input <search-full.json> --output <search.idx> [--verify] [--quiet]"
}

fn parse_args() -> Result<Args, String> {
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut quiet = false;
    let mut verify = false;
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--input" | "-i" => input = argv.next().map(PathBuf::from),
            "--output" | "-o" => output = argv.next().map(PathBuf::from),
            "--quiet" | "-q" => quiet = true,
            "--verify" => verify = true,
            "--help" | "-h" => return Err(usage().to_string()),
            other => return Err(format!("未知参数：{other}\n{}", usage())),
        }
    }
    Ok(Args {
        input: input.ok_or_else(|| format!("缺少 --input\n{}", usage()))?,
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

    let raw = std::fs::read_to_string(&args.input)
        .map_err(|e| format!("读取 {} 失败：{e}", args.input.display()))?;
    let records: Vec<JsonRecord> = serde_json::from_str(&raw)
        .map_err(|e| format!("解析 {} 失败：{e}", args.input.display()))?;
    if records.is_empty() {
        return Err(format!("{} 里没有任何文档", args.input.display()));
    }

    let docs: Vec<SearchDocument> = records
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
            "[build-index] {} → {}（{}）耗时 {} ms",
            args.input.display(),
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
        verify_recall(&bytes, &docs, args.quiet)?;
    }
    Ok(())
}

/// 文档级召回自检：从语料自动抽真实中文词，逐个查索引，统计「应命中的文档有多少被找回」。
///
/// 抽样策略：取正文里所有长度为 `VERIFY_TERM_CHARS` 的 CJK 子串，按**被多少个不同文档包含**降序，
/// 只保留出现于 ≥2 篇文档的候选（真实词会复现，跨词边界的碎片一般不会），再取前 N 个。
/// 这样不需要人工维护词表，也能在语料更新后继续生效。
fn verify_recall(index_bytes: &[u8], docs: &[SearchDocument], quiet: bool) -> Result<(), String> {
    let index = load_index(index_bytes.to_vec()).map_err(|e| format!("自检加载索引失败：{e}"))?;

    let normalized: Vec<String> = docs
        .iter()
        .map(|d| {
            format!(
                "{}\n{}\n{}",
                normalize(&d.title),
                normalize(&d.text),
                normalize(&d.tags.join(" "))
            )
        })
        .collect();

    // 候选词 → 出现的文档数
    let mut freq: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    for (doc_id, text) in normalized.iter().enumerate() {
        let chars: Vec<char> = text.chars().collect();
        for window in chars.windows(VERIFY_TERM_CHARS) {
            if !window.iter().all(|c| is_cjk_like(*c)) {
                continue;
            }
            let term: String = window.iter().collect();
            freq.entry(term).or_default().insert(doc_id);
        }
    }

    let mut candidates: Vec<(String, usize)> = freq
        .into_iter()
        .filter(|(_, ids)| ids.len() >= VERIFY_MIN_DOC_FREQ)
        .map(|(term, ids)| (term, ids.len()))
        .collect();
    // 出现文档数多的优先（更可能是真实词），同频按字典序保证确定性
    candidates.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let sample: Vec<(String, usize)> = candidates.into_iter().take(VERIFY_SAMPLE_SIZE).collect();
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
