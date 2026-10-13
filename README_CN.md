# bigseek

**中文** · [English](./README.md)

[![CI](https://img.shields.io/github/actions/workflow/status/imba97/bigseek/ci.yml?label=CI)](https://github.com/imba97/bigseek/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.82-blue.svg)](https://blog.rust-lang.org/)

一个二进制索引里的 bigram 全文检索：构建期原生编译，运行期用 WebAssembly 查询。

`bigseek` 对外只有两个入口，背后是同一份实现：原生 CLI 读取 markdown 文章目录或文档 JSON，
写出一份紧凑二进制索引；WebAssembly 模块加载这份索引时不反序列化，直接回答精确子串查询。
分词、打分、索引语义都在同一个 crate 里，构建期与运行期对语料的理解不可能漂移。

中文、假名、谚文按 **bigram** 建索引，因此不需要分词词典也能拿到精确子串语义；
拉丁与数字按完整的字母数字段切分。

## 目录

- [背景](#背景)
- [特性](#特性)
- [工作原理](#工作原理)
- [索引格式](#索引格式)
- [安装](#安装)
- [用法](#用法)
- [API](#api)
- [确定性](#确定性)
- [已知取舍](#已知取舍)
- [参与贡献](#参与贡献)
- [License](#license)

## 背景

按空白与标点切词的方案会把「一整句中文」当成一个词条，于是只有恰好落在句首的词能被搜到。
在 144 篇中文技术文章上实测（50 个真实词、597 个「词 × 文档」对）：

| 引擎 | 文档级召回 | 完全命中的词 | 冷启动建索引 |
| --- | --- | --- | --- |
| 空白切词（[MiniSearch](https://github.com/lucaong/minisearch) 默认） | **39.7%**（237/597） | 0/48 | 79 ms |
| MiniSearch + CJK bigram | 98.3%（587/597） | 47/48 | 300 ms |
| **bigseek**（bigram + OR 回退 + 原文校验） | **100.0%**（597/597） | 48/48 | **1.2 ms**（零反序列化加载） |

失败模式很窄：句子中间的词搜不到，而标题、标签、英文都正常——这正是它长期不被发现的原因。

## 特性

- **一份实现，两处运行**：原生构建器与 wasm 读取端共用同一份分词、打分与索引语义，
  不存在「构建期一套、运行期一套」的漂移。
- **CJK bigram 分词**：`性能优化` → `性能` / `能优` / `优化`；单字符 CJK 查询走词典前缀展开，
  所以 `性` 也能搜到。
- **两段式检索**：先 AND，为空再退化为 OR 并按命中 token 数排序。这一步专门吸收 bigram
  的跨词边界产物（`性能优化` 也会生成不存在的 `能优`，纯 AND 会把它变成漏检索）。
- **原文校验**：候选逐个用原文子串确认，于是既有精确子串语义，又不需要分词词典。
- **紧凑二进制格式**：前端编码词典、稀疏块索引、delta + varint 倒排、定长文档记录、
  去重字符串池、分面（标签/分类）表。144 篇 / 24154 词条 / 53246 倒排项共 **654 KiB**。
- **零反序列化加载**：加载只校验头部与解析分面表，词条与倒排按需从字节切片读；
  上述索引约 **1.2 ms**。
- **自带质量门禁**：`--verify` 建完索引后从语料抽样真实词自检，文档级召回低于 95% 直接失败。
- **wasm 侧没有 JS**：ABI 契约用 Rust 的 `wasm-bindgen-test` 覆盖。

## 工作原理

1. **分词**：归一化是逐字符 1:1 映射（全角 ASCII → 半角、`U+3000` → 空格、ASCII 大写 → 小写），
   因此字符偏移永不变动。CJK/假名/谚文段出 bigram（单字符段出 unigram），拉丁段按
   `char::is_alphanumeric` 切分。
2. **建索引**：只在文档级建倒排——`title` / `tags` / `categories` / `text` 用 `\n` 连接
   （它既不是 bigram 也不是字母数字段的一部分），每篇只分词一次。
3. **定位**：词典按前端编码，每 16 个词条一块并有稀疏块索引；查询先二分块首词，
   再在单块内顺序解码。
4. **检索**：每个 token 生成一个文档位图（精确词条 + 针对最后一个 token 与单字符 CJK 的
   前缀展开）；位图求交，交集为空时才用并集。
5. **打分**：字段权重（标题 4 / 标签 3 / 分类 2 / 正文 1，整串精确命中额外 +6）在查询期用
   原文现算，因此不存任何分字段倒排表。
6. **摘要**：在归一化文本里定位命中，再把字符下标映射回原文——因为归一化是 1:1，
   这件事不需要位置表。

## 索引格式

格式版本 1。所有整数小端序；变长整数为无符号 LEB128。

```text
header 80 B   magic "BSSI" | version | flags | doc_count | term_count | block_count
              terms / blocks / postings / docs / pool / facets 各自的 off + len
terms         按字典序前端编码的词条记录，每 16 条一块
              varint prefix_len | varint suffix_len | suffix | varint postings_len | varint doc_freq
blocks        稀疏块索引（16 B/块）：first_term_off | term_count | postings_off | reserved
              块内首词的 prefix_len 恒为 0，因此可以从任意块开始解码
postings      doc id 升序，delta + varint，按词条顺序连续存放
docs          定长 48 B：(off, len) × path / title / date / text / tags / categories
pool          原始字符串按写盘顺序拼接，相同串去重
              tags 与 categories 用 U+001F 连接
facets        varint 计数 + (名称, doc id 升序 delta-varint)*
```

## 安装

Rust crate 目前按路径或 git 依赖使用：

```toml
[dependencies]
bigseek = { git = "https://github.com/imba97/bigseek" }
```

WebAssembly 产物（`bigseek_bg.wasm`、`bigseek.js`，以及预编译的 `build-index` 与契约向量）
挂在每个 [GitHub release](https://github.com/imba97/bigseek/releases) 上。尚未发布到 crates.io。

## 用法

### 构建索引

```bash
# A) 直读文章目录（推荐）：递归扫 **/index.md、解析 frontmatter、套用发布规则、markdown 转纯文本
cargo run --release --bin build-index -- \
  --posts-dir ./posts \
  --output ./public/search/search.idx \
  --verify

# B) 读文档 JSON（{ path, title, date, tags, categories, text } 的数组）
cargo run --release --bin build-index -- \
  --input ./documents.json \
  --output ./public/search/search.idx \
  --verify
```

同一份语料下两种输入产出**字节级相同**的索引（在 144 篇真实文章上做过 SHA-256 差分对拍）。

| | `--posts-dir` | `--input` JSON |
| --- | --- | --- |
| 发布规则真源 | Rust 实现（由共享向量锁定） | JSON 的生产方 |
| 构建期依赖 | 无（直接读文件系统） | 必须先跑一遍 JSON 导出 |
| 索引内容上限 | 可扩展（标题层级、代码语言、原文偏移） | 被拍平步骤固定 |

### 用 WebAssembly 查询

```bash
wasm-pack build --target web --release --out-dir pkg \
  --no-default-features --features wasm
```

```js
import init, { SearchIndex } from './bigseek.js'

await init({ module_or_path: wasmBytes }) // 显式传字节，绕开 MIME / 路径问题
const index = new SearchIndex(indexBytes)

index.query('性能优化', null, null, 40)
// → [{ path, title, date, tags, categories, snippet, score, exact }, ...]
//   tag / category 传 null 表示不限范围；limit 传 null 用默认值 40
```

### 用 Rust 查询

```rust
use bigseek::{build_index, load_index, SearchDocument};

let (bytes, stats) = build_index(&docs);
let index = load_index(bytes)?;
let hits = index.query("性能优化", None, None, 40);
```

## API

### CLI

| 参数 | 说明 |
| --- | --- |
| `--posts-dir <dir>` | 递归读取文章目录下的 `**/index.md` |
| `--input <file>` / `-i` | 读取文档 JSON 数组 |
| `--output <file>` / `-o` | 写出二进制索引（必填） |
| `--verify` | 写完立刻加载并做文档级召回自检（低于 95% 失败） |
| `--quiet` / `-q` | 不打印进度 |

### `SearchIndex`（wasm）

| 成员 | 说明 |
| --- | --- |
| `new(bytes)` | 解析索引；magic 不对、版本不支持、段损坏时抛错 |
| `query(keywords, tag?, category?, limit?)` | 返回命中数组；`tag` / `category` 传 `null` 表示不限范围 |
| `docCount()` / `termCount()` | 文档数 / 去重后的词条数 |
| `facetCounts()` | `[标签数, 分类数]` |
| `formatVersion()` | 索引格式版本，客户端可据此尽早拒绝不匹配的索引 |

### Cargo features

| feature | 默认 | 内容 |
| --- | --- | --- |
| `cli` | 是 | markdown/YAML/JSON 处理与 `build-index` 二进制 |
| `wasm` | 否 | `wasm-bindgen` 绑定 |

两个 feature 互不依赖。`comrak`、`serde_yaml_ng`、`regex` 只挂在 `cli` 上，wasm 产物不会因此变大。
因为 `cli` 是默认 feature，构建 wasm 必须显式 `--no-default-features`（忘了也不会坏——
LTO 会消掉未被引用的代码，只是构建变慢）。

## 确定性

分词与发布规则由两份跨语言向量文件锁定，Rust 与 JavaScript 两侧实现都按它们测试：

| 文件 | 锁定的语义 |
| --- | --- |
| `testdata/tokenize-vectors.json` | 归一化与分词（bigram 边界、全角、大小写、单字符、假名） |
| `testdata/post-policy-vectors.json` | 发布规则（`draft` 真值、空日期、id 数字、标签归一化） |

向量里刻意放了几个 JavaScript 语义陷阱，它们是最容易静默漂移的地方：
`draft: "false"` 在 JS 里是真值、`id: 7.0` 字符串化为 `"7"`、
`tags: ["  a  ", "", "   ", b]` 归一化后是 `["a", "b"]`。

归一化刻意只做逐字符 1:1 映射而不是完整 NFKC：既省掉 `unicode-normalization` 的表体积，
又保证「归一化文本的字符下标 == 原文的字符下标」——摘要在没有位置表的情况下才成立。
要支持繁体或组合字符，扩展映射表并把 `FORMAT_VERSION` +1 即可。

## 已知取舍

- **不做词干化、同义词、拼音**：目标语料不需要；扩展分词器同样受向量测试约束。
- **180 KiB 的词典是索引体积的主要开销**：24154 个 bigram 词条的必然结果。改用真正的 FST、
  或只对正文用 bigram，预计还能省 20~35%。
- **查询比基于 JSON 的引擎慢**（0.37~1.11 ms vs 约 0.1 ms）：代价来自 OR 回退与原文校验，
  换来的是召回与精确性；相对一次按键的帧预算，这个量级没有观感差异。
- **进程内不并发**：`Index` 只读、无内部可变状态，天然 `Send + Sync`；wasm 侧按单线程使用。
- **`opt-level = 3` 而非 `"z"`**：在 144 篇语料上实测，`"z"` 得到 74.7 KiB wasm 与 0.509 ms 查询，
  `3` 得到 87.1 KiB 与 0.369 ms。索引本身已有 654 KiB，为快 30% 付 12.4 KiB 更划算。

## 参与贡献

欢迎提 issue 与 PR。要求 `cargo fmt --check`、`cargo clippy --all-targets --all-features -- -D warnings`
与 `cargo test` 全绿；wasm ABI 契约在 `tests/wasm_abi.rs`，用 `wasm-pack test --node` 执行。

## License

[MIT](./LICENSE)
