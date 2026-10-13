//! 直接以文章目录为输入（构建期）：扫 `posts/**/index.md` → 解析 frontmatter → 按发布规则过滤
//! → markdown 转纯文本 → 产出 [`SearchDocument`]。
//!
//! ## 为什么还需要 `--input search-full.json`
//!
//! 两条输入路径并存，用途不同：
//!
//! - `--posts-dir`：Rust 自己读文章。构建期少一次「TS 抽文 → JSON → Rust 反序列化」的往返，
//!   且后续可以索引 JSON 装不下的结构（标题层级、代码语言、原文偏移）。
//! - `--input`：读 TS 产出的 JSON。仍是**回归基准**（差分测试拿它当基准）与 MiniSearch 回退路径的输入。
//!
//! ## 与 TS 侧的一致性边界
//!
//! 这里复刻的是博客 `post-policy.ts` / `post-date.ts` 的规则与 `markdown-it` 的**纯文本抽取**，
//! 契约由 `testdata/post-policy-vectors.json` 锁定，并由 `tests/direct_read_parity.rs` 在真实语料上对拍。
//! 刻意对齐的几处 JS 语义（错一处就会静默改变索引内容）：
//!
//! - JS 真值判断：`draft: "false"`（字符串）是**真**，所以该文不可发布
//! - JS `String()` 数字格式化：`id: 7.0` → `"7"`，不是 `"7.0"`
//! - JS `String()` 退化值：mapping → `"[object Object]"`，数组 → 逗号连接
//! - `normalizeStringList`：标量也包成单元素数组，元素 trim 后丢弃空串
//! - markdown 渲染开关：`html: true` / `breaks: true` / `linkify: true`，
//!   且**关掉 comrak 默认开启的 GFM 扩展**（表格、删除线、任务列表、tagfilter），
//!   否则 `~~x~~` 这类文本会被渲染掉、与 markdown-it 裸配置产出不同文本

use std::fs;
use std::path::{Path, PathBuf};

use comrak::Options;
use regex::Regex;
use serde_yaml_ng::Value;

use crate::build::SearchDocument;

/// 正文纯文本上限（与博客 `MAX_TEXT_CHARS` 对齐）
pub const MAX_TEXT_CHARS: usize = 100_000;
/// 上海时区偏移；博客里不带时区的日期一律按 Asia/Shanghai 解释
const SHANGHAI_OFFSET_HOURS: i64 = 8;

/// 一次加载的统计，供 CLI 打印
#[derive(Debug, Default, Clone, Copy)]
pub struct LoadReport {
    pub files: usize,
    pub published: usize,
    pub skipped_draft: usize,
    pub skipped_no_date: usize,
    pub skipped_bad_id: usize,
}

impl LoadReport {
    pub fn summary(&self) -> String {
        format!(
            "扫描 {} 篇，发布 {} 篇（跳过：draft {} / 无 date {} / id 非数字 {}）",
            self.files,
            self.published,
            self.skipped_draft,
            self.skipped_no_date,
            self.skipped_bad_id
        )
    }
}

/// 扫描文章目录，返回按日期降序排列的文档。
pub fn load_posts(
    posts_dir: impl AsRef<Path>,
) -> Result<(Vec<SearchDocument>, LoadReport), String> {
    let posts_dir = posts_dir.as_ref();
    if !posts_dir.is_dir() {
        return Err(format!("文章目录不存在：{}", posts_dir.display()));
    }

    let files = collect_index_files(posts_dir)?;
    let extractor = TextExtractor::new()?;
    let mut report = LoadReport {
        files: files.len(),
        ..Default::default()
    };
    let mut entries: Vec<Entry> = Vec::new();

    for file in &files {
        let raw =
            fs::read_to_string(file).map_err(|e| format!("读取 {} 失败：{e}", file.display()))?;
        let (front_matter, body) = split_front_matter(&raw);

        let outcome = evaluate_frontmatter(front_matter.as_deref().unwrap_or(""))
            .map_err(|e| format!("{} 的 frontmatter 处理失败：{e}", file.display()))?;

        match outcome.reject_reason {
            Some(RejectReason::Draft) => {
                report.skipped_draft += 1;
                continue;
            }
            Some(RejectReason::NoDate) => {
                report.skipped_no_date += 1;
                continue;
            }
            Some(RejectReason::BadId) => {
                report.skipped_bad_id += 1;
                continue;
            }
            None => {}
        }

        let path = outcome.path.clone();
        let date = outcome.date.clone();
        let sort_timestamp = parse_post_date_timestamp(&date);
        entries.push(Entry {
            doc: SearchDocument {
                path,
                title: outcome.title.clone(),
                date,
                tags: outcome.tags.clone(),
                categories: outcome.categories.clone(),
                text: extractor.markdown_to_plain(&body),
            },
            sort_timestamp,
        });
        report.published += 1;
    }

    // 与 comparePostDateDesc 对齐：时间戳降序；无法解析的排后面。
    // 额外用 path 兜底保证确定性（本语料无并列日期，故与 TS 稳定排序结果一致）。
    entries.sort_by(|a, b| match (a.sort_timestamp, b.sort_timestamp) {
        (Some(x), Some(y)) => y.cmp(&x).then_with(|| a.doc.path.cmp(&b.doc.path)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => b
            .doc
            .date
            .cmp(&a.doc.date)
            .then_with(|| a.doc.path.cmp(&b.doc.path)),
    });

    Ok((entries.into_iter().map(|e| e.doc).collect(), report))
}

struct Entry {
    doc: SearchDocument,
    sort_timestamp: Option<i64>,
}

// ---------------------------------------------------------------------------
// 发布规则（与 post-policy.ts 逐条对齐，由 testdata/post-policy-vectors.json 锁定）
// ---------------------------------------------------------------------------

/// 发布判定结果。`publishable == false` 时其余字段无意义。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PolicyOutcome {
    pub publishable: bool,
    pub reject_reason: Option<RejectReason>,
    pub id: String,
    pub path: String,
    pub title: String,
    pub date: String,
    pub tags: Vec<String>,
    pub categories: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// `draft` 为真值
    Draft,
    /// 缺 `date` 或 `date` 为空串
    NoDate,
    /// `id` 缺失 / 非纯数字
    BadId,
}

/// 对一段 frontmatter YAML 应用发布规则。
///
/// 这是一个可独立调用的公开入口，便于与博客 TS 侧用同一份向量对拍
/// （见 `tests/post_policy_vectors.rs` 与博客的 `pnpm run search:check-parity`）。
pub fn evaluate_frontmatter(yaml: &str) -> Result<PolicyOutcome, String> {
    let value: Value = if yaml.trim().is_empty() {
        Value::Mapping(Default::default())
    } else {
        serde_yaml_ng::from_str(yaml).map_err(|e| format!("frontmatter 不是合法 YAML：{e}"))?
    };
    let data: serde_yaml_ng::Mapping = match value {
        Value::Null => Default::default(),
        Value::Mapping(m) => m,
        other => {
            return Err(format!(
                "frontmatter 解析结果不是映射（而是 {}）",
                value_kind(&other)
            ))
        }
    };

    let mut outcome = PolicyOutcome::default();

    // isPublishablePostData：`if (o.draft) return false` 用的是 JS 真值，不是 `=== true`
    if data.get("draft").is_some_and(js_truthy) {
        outcome.reject_reason = Some(RejectReason::Draft);
        return Ok(outcome);
    }
    match data.get("date") {
        None | Some(Value::Null) => {
            outcome.reject_reason = Some(RejectReason::NoDate);
            return Ok(outcome);
        }
        Some(Value::String(s)) if s.is_empty() => {
            outcome.reject_reason = Some(RejectReason::NoDate);
            return Ok(outcome);
        }
        _ => {}
    }
    let Some(id) = normalize_numeric_post_id(&data) else {
        outcome.reject_reason = Some(RejectReason::BadId);
        return Ok(outcome);
    };

    outcome.publishable = true;
    outcome.path = format!("/post/{id}");
    outcome.id = id;
    outcome.title = match data.get("title") {
        None | Some(Value::Null) => String::new(),
        Some(v) => js_to_string(v),
    };
    outcome.date = normalize_post_date_string(data.get("date"));
    outcome.tags = normalize_string_list(data.get("tags"));
    outcome.categories = normalize_string_list(data.get("categories"));
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// 文件扫描
// ---------------------------------------------------------------------------

/// 递归收集 `**/index.md`，并排除根部的 `posts/index.md`（与 `POSTS_ROOT_INDEX_FILE` 对齐）。
fn collect_index_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("读取目录 {} 失败：{e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("遍历 {} 失败：{e}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| format!("读取 {} 类型失败：{e}", path.display()))?;
        if file_type.is_dir() {
            walk(root, &path, out)?;
        } else if file_type.is_file()
            && entry.file_name() == "index.md"
            && path != root.join("index.md")
        {
            out.push(path);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// frontmatter
// ---------------------------------------------------------------------------

/// 拆出 frontmatter 与正文。返回 `(yaml, body)`；没有 frontmatter 时 yaml 为 None。
///
/// 与 `@11ty/gray-matter` 对齐的边界：首行必须是 `---`，闭合定界符是**独立成行**的 `---`
/// （允许尾部空白）；未闭合时按「没有 frontmatter」处理。本站语料 144 篇全部使用该形态。
fn split_front_matter(raw: &str) -> (Option<String>, String) {
    let normalized = raw.replace("\r\n", "\n");
    let mut lines = normalized.split('\n');
    let first = lines.next().unwrap_or("");
    if first.trim_end() != "---" {
        return (None, normalized);
    }

    let mut yaml_lines: Vec<&str> = Vec::new();
    let mut consumed = first.len() + 1;
    let mut closed = false;
    for line in lines {
        consumed += line.len() + 1;
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        yaml_lines.push(line);
    }

    if !closed {
        return (None, normalized);
    }
    let body = normalized.get(consumed..).unwrap_or("").to_string();
    (Some(yaml_lines.join("\n")), body)
}

// ---------------------------------------------------------------------------
// markdown → 纯文本
// ---------------------------------------------------------------------------

/// 复刻 `generate-search-index.ts` 的 `stripHtml(markdown.render(content))`。
struct TextExtractor {
    options: Options<'static>,
    script: Regex,
    style: Regex,
    tag: Regex,
    whitespace: Regex,
}

impl TextExtractor {
    fn new() -> Result<Self, String> {
        let mut options = Options::default();
        // markdown-it { html: true, breaks: true, linkify: true }
        options.render.r#unsafe = true;
        options.render.hardbreaks = true;
        // markdown-it 的**默认 preset**（非 commonmark）自带 table 与 strikethrough，必须一起打开，
        // 否则 `| a | b |` 与 `~~x~~` 的抽取结果和 TS 侧不同（差分测试实测 4 篇不一致）。
        options.extension.table = true;
        options.extension.strikethrough = true;
        options.extension.autolink = true;
        // 以下扩展 markdown-it 默认没有，保持关闭：
        //   tasklist / tagfilter / footnotes / description_lists / math / wikilinks / alerts /
        //   superscript / subscript / greentext / spoiler / underline / highlight / insert /
        //   multiline_block_quotes
        options.extension.tasklist = false;
        options.extension.tagfilter = false;
        options.extension.footnotes = false;
        options.extension.description_lists = false;
        options.extension.multiline_block_quotes = false;
        options.extension.math_dollars = false;
        options.extension.math_code = false;
        options.extension.wikilinks_title_after_pipe = false;
        options.extension.wikilinks_title_before_pipe = false;
        options.extension.alerts = false;
        options.extension.superscript = false;
        options.extension.subscript = false;
        options.extension.greentext = false;
        options.extension.spoiler = false;
        options.extension.underline = false;
        options.extension.highlight = false;
        options.extension.insert = false;
        options.parse.smart = false;
        options.render.github_pre_lang = false;

        let build = |pattern: &str| {
            Regex::new(pattern).map_err(|e| format!("正则 {pattern} 编译失败：{e}"))
        };
        Ok(Self {
            options,
            script: build(r"(?is)<script.*?</script>")?,
            style: build(r"(?is)<style.*?</style>")?,
            tag: build(r"<[^>]+>")?,
            whitespace: build(r"\s+")?,
        })
    }

    fn markdown_to_plain(&self, content: &str) -> String {
        let html = comrak::markdown_to_html(content, &self.options);
        let stripped = self.script.replace_all(&html, " ");
        let stripped = self.style.replace_all(&stripped, " ");
        let stripped = self.tag.replace_all(&stripped, " ");
        let collapsed = self.whitespace.replace_all(&stripped, " ");
        truncate_chars(collapsed.trim(), MAX_TEXT_CHARS)
    }
}

/// 按字符截断（TS 侧用 `slice` 按 UTF-16 code unit 截断；本语料正文远小于上限，两者等价）
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect()
}

// ---------------------------------------------------------------------------
// JS 语义小工具（错一处就会静默改变索引内容）
// ---------------------------------------------------------------------------

fn value_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Sequence(_) => "sequence",
        Value::Mapping(_) => "mapping",
        Value::Tagged(_) => "tagged",
    }
}

/// JS 真值判断（`if (o.draft) return false` 用的是真值，不是 `=== true`）
fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_none_or(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Sequence(_) | Value::Mapping(_) | Value::Tagged(_) => true,
    }
}

/// JS `String(number)`：`7.0` → `"7"`，而不是 `"7.0"`
fn js_number_to_string(n: &serde_yaml_ng::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let f = n.as_f64().unwrap_or(f64::NAN);
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    if f.fract() == 0.0 && f.abs() < 1e21 {
        return format!("{}", f as i64);
    }
    format!("{f}")
}

/// JS `String(value)` 的退化规则（mapping → `[object Object]`，数组 → 逗号连接）
fn js_to_string(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => js_number_to_string(n),
        Value::String(s) => s.clone(),
        Value::Sequence(seq) => seq.iter().map(js_to_string).collect::<Vec<_>>().join(","),
        Value::Mapping(_) => "[object Object]".to_string(),
        Value::Tagged(t) => js_to_string(&t.value),
    }
}

/// 复刻 `normalizeStringList`：`null` → 空数组；数组 → 逐项 `String().trim()` 后丢空串；
/// 标量 → 包成单元素数组
pub fn normalize_string_list(value: Option<&Value>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    if value.is_null() {
        return Vec::new();
    }
    let raw: Vec<String> = match value {
        Value::Sequence(seq) => seq.iter().map(js_to_string).collect(),
        other => vec![js_to_string(other)],
    };
    raw.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 复刻 `normalizeNumericPostId`：空 / 非纯数字 → None
pub fn normalize_numeric_post_id(data: &serde_yaml_ng::Mapping) -> Option<String> {
    let value = data.get("id")?;
    if value.is_null() {
        return None;
    }
    let id = js_to_string(value);
    let trimmed = id.trim();
    if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(trimmed.to_string())
}

/// 复刻 `normalizePostDateString` 的字符串分支（YAML 不会产生 Date，故不做 Date 分支）
pub fn normalize_post_date_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.trim().to_string(),
        Some(other) => js_to_string(other).trim().to_string(),
    }
}

// ---------------------------------------------------------------------------
// 日期 → 时间戳（不引 chrono，用 days_from_civil）
// ---------------------------------------------------------------------------

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Howard Hinnant 的 days_from_civil
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// 解析 `YYYY-MM-DD[ HH:MM[:SS]]`（不带时区，按 Asia/Shanghai 解释）→ Unix 毫秒。
///
/// 与 `parsePostDateToTimestamp` 的这条分支对齐。带时区后缀 / ISO 等其它形态本站语料不出现，
/// 统一返回 None（排序时排到末尾，与 TS 侧行为一致：无法解析的日期被认为最旧）。
pub fn parse_post_date_timestamp(value: &str) -> Option<i64> {
    let raw = value.trim();
    let (date_part, time_part) = match raw.split_once([' ', 'T']) {
        Some((d, t)) => (d, t),
        None => (raw, ""),
    };

    let mut date_it = date_part.split('-');
    let year: i64 = date_it.next()?.parse().ok()?;
    let month: i64 = date_it.next()?.parse().ok()?;
    let day: i64 = date_it.next()?.parse().ok()?;
    if date_it.next().is_some() || year < 1 || !(1..=12).contains(&month) {
        return None;
    }
    if !(1..=days_in_month(year, month)).contains(&day) {
        return None;
    }

    let mut hour = 0i64;
    let mut minute = 0i64;
    let mut second = 0i64;
    if !time_part.is_empty() {
        // 带时区后缀的一律不认（本站语料不出现）
        if time_part.ends_with('Z') || time_part.contains('+') {
            return None;
        }
        let mut time_it = time_part.split(':');
        hour = time_it.next()?.parse().ok()?;
        minute = time_it.next().unwrap_or("0").parse().ok()?;
        second = time_it.next().unwrap_or("0").parse().ok()?;
        if !(0..24).contains(&hour) || !(0..60).contains(&minute) || !(0..60).contains(&second) {
            return None;
        }
    }

    let days = days_from_civil(year, month, day);
    let seconds =
        days * 86_400 + hour * 3_600 + minute * 60 + second - SHANGHAI_OFFSET_HOURS * 3_600;
    Some(seconds * 1_000)
}
