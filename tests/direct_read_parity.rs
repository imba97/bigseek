#![cfg(not(target_arch = "wasm32"))]

//! 直读文章 vs 读 `search-full.json` 的**差分对拍**（opt-in）。
//!
//! ```bash
//! BLOG_POSTS_DIR=../imba97-blog-vite/posts \
//! BLOG_SEARCH_DOCS=../imba97-blog-vite/public/search-full.json \
//! cargo test --release --features cli --test direct_read_parity -- --nocapture
//! ```
//!
//! `search-full.json` 是 TS 侧（`post-policy.ts` + 裸 `markdown-it`）的既有产出，作为本次移植的**基准**。
//! 对比维度：文档集合 / 顺序 / title / date / tags / categories 逐字一致，正文文本逐篇比对相似度。
//!
//! 这份测试是「Rust 也能直接吃文章」这个能力的**准入条件**：只要它红着，就说明两条路径会产出不同的索引。

use blog_search::posts::load_posts;

#[derive(serde::Deserialize)]
struct JsonDoc {
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

fn env_path(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// 逐字符相似度（1.0 = 完全一致）。用公共前缀 + 后缀夹逼，避免引入 diff 依赖。
fn similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    if ac.is_empty() && bc.is_empty() {
        return 1.0;
    }
    let mut head = 0;
    while head < ac.len() && head < bc.len() && ac[head] == bc[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < ac.len() - head
        && tail < bc.len() - head
        && ac[ac.len() - 1 - tail] == bc[bc.len() - 1 - tail]
    {
        tail += 1;
    }
    let common = head + tail;
    // 以较长串为分母：差异越大分数越低
    common as f64 / ac.len().max(bc.len()) as f64
}

fn first_divergence(a: &str, b: &str) -> String {
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    let mut i = 0;
    while i < ac.len() && i < bc.len() && ac[i] == bc[i] {
        i += 1;
    }
    let start = i.saturating_sub(40);
    let ctx = |chars: &[char]| -> String {
        let end = (i + 60).min(chars.len());
        chars[start..end].iter().collect::<String>()
    };
    format!(
        "第 {i} 个字符处分叉\n      JSON: {}…\n      直读: {}…",
        ctx(&ac).replace('\n', "⏎"),
        ctx(&bc).replace('\n', "⏎")
    )
}

#[test]
fn direct_read_matches_json_extraction() {
    let (Some(posts_dir), Some(docs_path)) =
        (env_path("BLOG_POSTS_DIR"), env_path("BLOG_SEARCH_DOCS"))
    else {
        eprintln!("跳过：未设置 BLOG_POSTS_DIR / BLOG_SEARCH_DOCS");
        return;
    };

    let raw = std::fs::read_to_string(&docs_path).expect("读取 search-full.json 失败");
    let json_docs: Vec<JsonDoc> = serde_json::from_str(&raw).expect("解析 search-full.json 失败");

    let (docs, report) = load_posts(&posts_dir).expect("直读文章失败");
    println!("\n直读统计：{}", report.summary());
    println!("JSON 基准：{} 篇", json_docs.len());

    // 1. 集合与顺序
    let json_paths: Vec<&str> = json_docs.iter().map(|d| d.path.as_str()).collect();
    let rust_paths: Vec<&str> = docs.iter().map(|d| d.path.as_str()).collect();
    assert_eq!(rust_paths.len(), json_paths.len(), "文档数量不一致");
    if rust_paths != json_paths {
        let mut diffs = Vec::new();
        for (i, (a, b)) in rust_paths.iter().zip(json_paths.iter()).enumerate() {
            if a != b {
                diffs.push(format!("  第 {i} 位：直读 {a} vs JSON {b}"));
            }
        }
        panic!("文档顺序不一致：\n{}", diffs.join("\n"));
    }
    println!("✅ 文档集合与顺序一致（{} 篇）", rust_paths.len());

    // 2. 元数据逐字比对
    let mut meta_failures = Vec::new();
    for (r, j) in docs.iter().zip(json_docs.iter()) {
        if r.title != j.title {
            meta_failures.push(format!("{} title：{:?} vs {:?}", r.path, r.title, j.title));
        }
        if r.date != j.date {
            meta_failures.push(format!("{} date：{:?} vs {:?}", r.path, r.date, j.date));
        }
        if r.tags != j.tags {
            meta_failures.push(format!("{} tags：{:?} vs {:?}", r.path, r.tags, j.tags));
        }
        if r.categories != j.categories {
            meta_failures.push(format!(
                "{} categories：{:?} vs {:?}",
                r.path, r.categories, j.categories
            ));
        }
    }
    assert!(
        meta_failures.is_empty(),
        "元数据不一致：\n{}",
        meta_failures.join("\n")
    );
    println!("✅ title / date / tags / categories 全部逐字一致");

    // 3. 正文文本
    let mut identical = 0usize;
    let mut worst: Vec<(f64, String, String)> = Vec::new();
    let mut len_delta_total = 0i64;
    let mut differ = 0usize;
    for (r, j) in docs.iter().zip(json_docs.iter()) {
        if r.text == j.text {
            identical += 1;
            continue;
        }
        differ += 1;
        len_delta_total += r.text.chars().count() as i64 - j.text.chars().count() as i64;
        let score = similarity(&r.text, &j.text);
        if worst.len() < 6 || score < worst.last().map(|w| w.0).unwrap_or(1.0) {
            worst.push((score, r.path.clone(), first_divergence(&j.text, &r.text)));
            worst.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            worst.truncate(6);
        }
    }

    println!(
        "\n正文：完全一致 {identical}/{} 篇；有差异 {differ} 篇；总字符数差 {len_delta_total:+}",
        docs.len()
    );
    if !worst.is_empty() {
        println!("\n差异最大的几篇：");
        for (score, path, detail) in &worst {
            println!("  {path}  相似度 {:.4}\n      {detail}", score);
        }
    }

    // 4. 门槛
    let identical_ratio = identical as f64 / docs.len() as f64;
    let min_similarity = worst.first().map(|w| w.0).unwrap_or(1.0);
    assert!(
        min_similarity >= 0.995,
        "存在相似度低于 0.995 的文章（最低 {min_similarity:.4}），两条路径产出的索引会明显不同"
    );
    println!(
        "\n完全一致率 {:.1}%（最低相似度 {:.4}，门槛 0.995）",
        identical_ratio * 100.0,
        min_similarity
    );
}
