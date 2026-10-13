# blog-search

`imba97-blog-vite` 站内搜索的独立检索引擎：**构建期用 native CLI 产出紧凑二进制索引，运行期用 WebAssembly 查询**，
两端共用同一份分词实现与同一份倒排语义。

本仓库是 `https://github.com/imba97/imba97-blog-vite` 的搜索依赖，由博客的构建流程拉取、编译，
把产物放进 `public/search/`（wasm + glue + 索引）后随站点一起部署。

---

## 为什么需要它

博客原先用 [MiniSearch](https://github.com/lucaong/minisearch)，它的默认分词器按空白 + 标点切词，
中文「一对标点之间的整句话」会成为一个词条。实测（50 个真实中文技术词、597 个「词 × 文档」对）：

| 引擎 | 文档级召回 | 完全命中的词 | 冷启动建索引 |
| --- | --- | --- | --- |
| MiniSearch 默认分词 | **39.7%**（237/597） | 0/48 | 79 ms |
| MiniSearch + CJK bigram | 98.3%（587/597） | 47/48 | 300 ms |
| **本库（bigram + OR 回退 + 原文校验）** | **100.0%**（597/597） | 48/48 | **1.2 ms**（零反序列化加载） |

失败模式很具体：只有落在句子开头的词能被搜到，像 `性能`（4 篇）、`重写`（1 篇）这种整词落在句中的查询直接 0 结果。
标题、标签、英文因为不会跨标点，所以「看起来能用」——这也是它长期没被发现的原因。

实测数据完整版见博客仓库的 [`ai-docs/design/site-search.md`](https://github.com/imba97/imba97-blog-vite/blob/main/ai-docs/design/site-search.md)。

## 设计要点

1. **一份实现，两处运行**。分词、打分、索引语义都在这个 crate 里：native CLI 建索引，wasm 查索引。
   不存在「构建期一套、运行期一套」的漂移（旧方案里这是最容易出错的点）。
2. **中英分词**：CJK / 假名 / 谚文走 **bigram**（`性能优化` → `性能`/`能优`/`优化`），拉丁按 `is_alphanumeric` 切。
   单字符 CJK 查询走词典前缀展开，所以 `性` 也能搜到。
3. **两段式检索**：先 AND（所有 token 都命中），为空再退化为 OR 并按命中 token 数排序。
   这一步专门解决 bigram 的跨词边界问题——`性能优化` 会生成不存在的 `能优`，纯 AND 会误伤。
4. **候选 + 原文子串校验**：候选集只有几十个，逐个用原文 `contains` 验证，拿到「精确子串」语义，
   既不需要中文分词词典，也不会因 bigram 碎片而误报。
5. **字段权重不建多份倒排**：只在文档级建倒排，`title`/`tags`/`categories` 的权重在查询期现算。
   代价是几十次 `contains`，收益是词典体积不 ×4。
6. **零反序列化**：加载时只校验头部、解析分面表（标签/分类），词条与 postings 按需从字节切片读。
   654 KiB / 144 篇的索引加载 **1.2 ms**。

## 索引格式（v1）

```
header 80 B   magic "BSSI" | version | flags | doc_count | term_count | block_count
              terms/blocks/postings/docs/pool/facets 各自的 off/len
terms         按字典序前端编码的词条记录，每 16 条为一块
              varint prefix_len | varint suffix_len | suffix | varint postings_len | varint doc_freq
blocks        稀疏块索引（16 B/块）：first_term_off | term_count | postings_off | reserved
              块内首词共享前缀置 0 → 可从任意块直接开始顺序解码
postings      doc id 升序 delta + varint，按词条顺序连续存放
docs          定长 48 B：path/title/date/text/tags/cats 的 (off, len)
pool          所有原始字符串（tags/cats 用 U+001F 连接），相同串去重
facets        varint 计数 + (名称, doc id 列表)
```

本语料实测体积：词典 180 KiB + postings 54 KiB + 文档表 7 KiB + 字符串池 388 KiB + 分面 3 KiB = **654 KiB**。

> ⚠️ **不要用「减小下载体积」当立项理由。** 654 KiB 比原来的 `search-full.json`（401 KiB）更大，
> gzip 后也基本持平。本库的收益是**冷启动**（300 ms → 1.2 ms）、**内存**（JS 堆 14.5 MiB → wasm 线性内存）
> 和**召回质量**，不是流量。

## 用法

### 构建期（native CLI）

两种输入方式，二选一：

```bash
# A) 直读文章目录（推荐）——Rust 自己扫 posts/**/index.md、解析 frontmatter、按发布规则过滤、markdown 转纯文本
cargo run --release --bin build-index -- \
  --posts-dir ../imba97-blog-vite/posts \
  --output ../imba97-blog-vite/public/search/search.idx \
  --verify

# B) 读博客 TS 侧产出的 search-full.json（回归基准 / 兼容旧流程）
cargo run --release --bin build-index -- \
  --input ../imba97-blog-vite/public/search-full.json \
  --output ../imba97-blog-vite/public/search/search.idx \
  --verify
```

两条路径在真实语料上产出**字节级完全相同**的索引（实测 144 篇、SHA256 一致），
由 `tests/direct_read_parity.rs` 持续对拍。

直读模式的取舍：

| | 直读 `--posts-dir` | 读 `--input` JSON |
| --- | --- | --- |
| 发布规则真源 | Rust 复刻（由共享向量锁定） | TS `post-policy.ts` |
| 构建期依赖 | 无（Rust 读文件系统） | 必须先跑一遍 TS 抽文 |
| 索引内容上限 | 可扩到标题层级/代码语言/原文偏移 | 被 `stripHtml` 拍平后固定 |
| 回归基准 | — | 仍是差分测试的基准 |

> ⚠️ 直读模式要求 Rust 侧与 TS 侧的抽取规则一致。**别只靠文档约束**：
> `testdata/post-policy-vectors.json` 锁规则、`tests/direct_read_parity.rs` 对拍真实语料、
> 博客构建里还有一道「两条路径产出索引哈希必须相同」的门禁（见下）。

`--verify` 会在写完索引后立刻加载它，自动从语料抽样 ≥40 个中文词（取跨文档重复出现的 2 字 CJK 串，
真实词会复现、跨词边界的碎片一般不会），做**文档级召回**自检，低于 95% 直接非 0 退出。
这样每次博客构建都自带质量门禁，语料更新后依然有效。

### 运行期（wasm）

```bash
wasm-pack build --target web --release --out-dir pkg --no-default-features --features wasm
```

```js
import init, { SearchIndex } from './blog_search.js'

await init({ module_or_path: wasmBytes })   // 显式传字节，绕开 MIME / 路径问题
const index = new SearchIndex(indexBytes)

index.query('性能优化', null, null, 40)
// → [{ path, title, date, tags, categories, snippet, score, exact }, ...]
//    tag / category 传 null 表示不限范围；limit 传 null 用默认 40
```

产物（本语料）：`blog_search_bg.wasm` **75.4 KiB**、glue JS 12.4 KiB。

> 是否带 `wasm` feature 是编译期开关：`cli` 与 `wasm` 互不依赖，wasm 产物里不会链接 `serde_json`。

## 测试

**仓库里没有任何 JS 测试代码**——wasm 侧的 ABI 契约测试也是用 Rust 写的（`tests/wasm_abi.rs`）。

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test                                    # 原生：分词向量、发布规则向量、fixture 直读、往返查询

# wasm ABI 契约（纯 Rust 编写，跑在 wasm32 上；需要 Node 作为 wasm 宿主）
wasm-pack test --node --no-default-features --features wasm

# 产物能按部署方式加载
wasm-pack build --target web --release --out-dir pkg --no-default-features --features wasm

# 真实语料的差分对拍（opt-in，需要博客仓库）
BLOG_POSTS_DIR=../imba97-blog-vite/posts \
BLOG_SEARCH_DOCS=../imba97-blog-vite/public/search-full.json \
cargo test --test direct_read_parity -- --nocapture

# 真实语料召回回归（opt-in）
BLOG_SEARCH_DOCS=../imba97-blog-vite/public/search-full.json \
BLOG_SEARCH_INDEX=../imba97-blog-vite/public/search/search.idx \
cargo test --test real_corpus -- --nocapture
```

`tests/wasm_abi.rs` 验证的是 native 上**执行不到**的那部分（wasm-bindgen 在 native 上是会 panic 的桩）：
`query()` 是否返回 JS 数组、命中项的 key 是否恰好是约定的那 8 个、`tags` 是否为 JS 字符串数组、
纯范围浏览的 `snippet` 是否为空、非法字节流是否抛可捕获的 JS 异常而不是 panic。

两个注意点：

- **`cargo test` 不要加 `--release`**：`[profile.release]` 里为了 wasm 体积设了 `panic = "abort"`，
  与测试目标要求的 unwind 不兼容。两个 opt-in 测试在 debug 下跑 144 篇也只要 0.2 s。
- **Windows + Node 24 下 `wasm-pack test --node` 会在测试跑完后崩**（runner 退出时触发 libuv 断言
  `src/win/async.c`，测试本身全部通过但 exit code 为 1）。实测 Node 20.8.1 正常退出；
  这是 Windows 专有代码路径的问题，Linux/CI（Node 22）不受影响。
  本机规避：把 Node 20/22 的目录前置到 `PATH` 再跑。

### 本机网络提示

如果 cargo 拉不到新依赖（企业代理 / 镜像环境），用环境变量让它走代理即可，不需要改全局配置：

```bash
# PowerShell
$env:HTTPS_PROXY='http://127.0.0.1:7897'; cargo fetch
# bash
HTTPS_PROXY=http://127.0.0.1:7897 cargo fetch
```

## 分词与发布规则的一致性契约

两份向量文件是**跨语言契约**，Rust 与博客 TS 侧读同一份：

| 文件 | 锁定的语义 | Rust 校验 | 博客 TS 校验 |
| --- | --- | --- | --- |
| `testdata/tokenize-vectors.json` | 归一化 + 分词（bigram 边界、全角、大小写、单字、假名） | `tests/tokenize_vectors.rs` | `pnpm run search:check-parity` |
| `testdata/post-policy-vectors.json` | 发布规则（draft 真值、date 判空、id 数字、标签归一化） | `tests/post_policy_vectors.rs` | `pnpm run search:check-parity` |

规则向量里刻意放了几个「JS 语义陷阱」用例，它们是最容易静默漂移的地方：

- `draft: "false"`（字符串）在 JS 里是**真值** → 文章不可发布
- `id: 7.0` → JS `String()` 得 `"7"`，不是 `"7.0"`
- `tags: ["  a  ", "", "   ", b]` → trim 后丢弃空串，得 `["a","b"]`
- `id: abc` / 缺 `date` / `date: ""` → 分别对应三种拒绝原因

归一化刻意只做**逐字符 1:1 替换**（全角 ASCII → 半角、`U+3000` → 空格、ASCII 大写 → 小写），不做完整 NFKC：
一是省掉 unicode-normalization 的表体积，二是「归一化文本的字符下标 == 原文的字符下标」，
摘要（snippet）可以直接把命中位置映射回原文，不需要额外位置表。若日后要支持繁体/组合字符，
在 `normalize` 里补映射表并把 `FORMAT_VERSION` +1 即可。

### markdown 抽取的一个坑（实测踩过）

博客用的 `markdown-it` 是**默认 preset**，它自带 **table 与 strikethrough**（不是 CommonMark）。
comrak 默认不开这两项，第一版移植因此有 4 篇文章文本不一致（表格管道符、`~~删除线~~`）。
现在 `posts.rs` 里显式打开 table/strikethrough、关闭 tasklist/tagfilter/footnotes 等 markdown-it 没有的扩展，
并由 `tests/direct_read_parity.rs` 守住。

## 目录

```
src/tokenize.rs        归一化 + bigram 分词（两端共用）
src/format.rs          格式常量、varint、头部字段偏移
src/build.rs           索引构建（BTreeMap 倒排 → 前端编码词典 → 各段拼装）
src/query.rs           索引加载 + 检索（块二分 → 候选 → 校验 → 打分 → 摘要）
src/posts.rs           直读文章（frontmatter/YAML → 发布规则 → markdown 转纯文本），仅 cli feature
src/wasm.rs            wasm-bindgen 边界（手工拼 js_sys 对象，不引 serde）
src/bin/build-index.rs 构建期 CLI（--posts-dir / --input）+ 召回自检
testdata/              跨语言契约向量（分词、发布规则）
tests/                 向量回归、fixture 直读、往返查询、wasm ABI 契约、真实语料对拍/召回（后两者 opt-in）
```

**依赖隔离**：`cli` 与 `wasm` 是互不依赖的 feature。`comrak` / `serde_yaml_ng` / `regex` 只挂在 `cli` 上，
wasm 产物不会因为直读文章而变大（实测 `blog_search_bg.wasm` 恒为 75.4 KiB）。

`cli` 被放进了 **default**，所以 `cargo build` / `cargo run` / `cargo test` 以及编辑器都开箱可用。
代价只有一处：**wasm-pack 必须显式 `--no-default-features`**：

```bash
wasm-pack build --target web --release --out-dir pkg --no-default-features --features wasm
```

忘了加也不会坏——实测 wasm 只大 5 KiB（LTO 会消掉未被引用的 comrak/regex），但构建会变慢。

### 编辑器报 `unresolved import blog_search::posts`？

那是 rust-analyzer 按默认 feature 分析、而某个模块被 `#[cfg(feature = ...)]` 关掉了，
**不是真的编译错误**（`cargo check` / `cargo test` 都是过的）。仓库自带 `.vscode/settings.json`：

```jsonc
"rust-analyzer.cargo.features": ["cli", "wasm"],
```

一次性打开 `cli` 与 `wasm`，让 `posts` 与 `wasm` 两个模块都能解析。
用其他编辑器请手动启用这两个 feature（或 `--all-features`）。

## 已知取舍

- **不做词干化 / 同义词 / 拼音**：当前语料不需要。要加的话在 `tokenize` 里扩，同样受向量测试约束。
- **词典 180 KiB 是主要体积开销**：24154 个 bigram 词条的必然结果。若日后要压，
  可以只对 `body` 用 bigram、`title`/`tags` 保持整词，或改用真正的 FST（预计再省 20~35%）。
- **查询比 MiniSearch 慢一点**（0.06~2 ms vs 0.3 ms）：代价来自 OR 回退 + 原文校验，换来的是召回与精确性。
  相对一次按键的帧预算，这个量级没有观感差异。
- **单进程内不并发**：`Index` 只读、无内部可变状态，`Send + Sync` 天然满足；wasm 侧按单线程使用。

## License

MIT
