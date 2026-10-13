# bigseek

[中文](./README_CN.md) · **English**

[![CI](https://img.shields.io/github/actions/workflow/status/imba97/bigseek/ci.yml?label=CI)](https://github.com/imba97/bigseek/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.82-blue.svg)](https://blog.rust-lang.org/)

Bigram full-text search in one binary index: build natively, query from WebAssembly.

`bigseek` ships two entry points backed by a single implementation. A native CLI reads a
markdown post directory or a document JSON and writes a compact binary index; a WebAssembly
module loads that index without deserializing it and answers substring-accurate queries.
Tokenization, scoring, and index semantics live in one crate, so the build-time and
run-time views of the corpus cannot drift apart.

Chinese, Japanese kana, and Hangul are indexed as **bigrams**, which gives exact substring
semantics without a segmentation dictionary. Latin and digits are indexed as whole
alphanumeric runs.

## Table of Contents

- [Background](#background)
- [Features](#features)
- [How it works](#how-it-works)
- [Index format](#index-format)
- [Install](#install)
- [Usage](#usage)
- [API](#api)
- [Determinism](#determinism)
- [Known trade-offs](#known-trade-offs)
- [Contributing](#contributing)
- [License](#license)

## Background

Whitespace-and-punctuation tokenizers treat a whole Chinese clause as a single term, so only
terms that happen to start a sentence are reachable. Measured on a 144-post Chinese technical
blog (50 real terms, 597 term × document pairs):

| Engine | Document-level recall | Terms fully retrieved | Cold index build |
| --- | --- | --- | --- |
| Whitespace tokenizer ([MiniSearch](https://github.com/lucaong/minisearch) default) | **39.7%** (237/597) | 0/48 | 79 ms |
| MiniSearch + CJK bigram | 98.3% (587/597) | 47/48 | 300 ms |
| **bigseek** (bigram + OR fallback + raw-text verification) | **100.0%** (597/597) | 48/48 | **1.2 ms** (zero-deserialization load) |

The failure mode is narrow: a term in the middle of a sentence returns nothing, while titles,
tags, and Latin words keep working — which is why it can go unnoticed for a long time.

## Features

- **One implementation, two runtimes** — the same tokenizer, scoring, and index semantics are
  used by the native builder and the wasm reader. No build-time/runtime drift.
- **CJK bigram tokenization** — `性能优化` → `性能` / `能优` / `优化`; single-character CJK queries
  expand through the term dictionary, so `性` is searchable too.
- **Two-pass retrieval** — AND first, then an OR fallback ranked by number of matched tokens.
  This is what absorbs the cross-word bigram artefacts (`性能优化` also produces the nonexistent
  `能优`, which a pure AND would turn into false negatives).
- **Raw-text verification** — candidates are confirmed with a substring check on the original
  field text, giving exact-substring semantics without a segmentation dictionary.
- **Compact binary format** — front-coded dictionary, sparse block index, delta + varint
  postings, fixed-size document records, deduplicated string pool, and facet (tag/category)
  tables. 654 KiB for 144 posts / 24154 terms / 53246 postings.
- **Zero-deserialization load** — loading validates the header and parses the facet tables
  only; terms and postings are read lazily from the byte slice. ~1.2 ms for the index above.
- **Quality gate built in** — `--verify` samples real terms from the corpus after building and
  fails the build if document-level recall drops below 95%.
- **No wasm-side JS** — the ABI contract is tested with `wasm-bindgen-test` in Rust.

## How it works

1. **Tokenize.** Normalization is a per-character 1:1 mapping (fullwidth ASCII → halfwidth,
   `U+3000` → space, ASCII uppercase → lowercase) and therefore never changes character
   offsets. CJK/kana/Hangul runs become bigrams (single-character runs stay unigrams); Latin
   runs are split on `char::is_alphanumeric`.
2. **Build.** Document-level postings only — `title` / `tags` / `categories` / `text` are
   concatenated with `\n` (which neither starts a bigram nor an alphanumeric run) and tokenized
   once per document.
3. **Locate.** The dictionary is front-coded in blocks of 16 terms with a sparse block index,
   so a query binary-searches block first terms and then decodes forward within one block.
4. **Retrieve.** Each token produces a document bit set (exact term plus a prefix expansion for
   the last token and for single-character CJK). Sets are intersected, and the union is used
   only when the intersection is empty.
5. **Score.** Field weights (title 4 / tags 3 / categories 2 / text 1, plus 6 for an exact full
   match) are computed at query time from the raw text, so no per-field posting lists are
   stored.
6. **Snippet.** The hit is located in the normalized field text and the character offset is
   mapped back to the original text, which needs no position table thanks to the 1:1 mapping.

## Index format

Format version 1. All integers are little-endian; variable-length integers are unsigned LEB128.

```text
header 80 B   magic "BSSI" | version | flags | doc_count | term_count | block_count
              terms / blocks / postings / docs / pool / facets, each with off + len
terms         front-coded term records in lexicographic order, 16 per block
              varint prefix_len | varint suffix_len | suffix | varint postings_len | varint doc_freq
blocks        sparse block index, 16 B per block:
              first_term_off | term_count | postings_off | reserved
              the first term of a block always has prefix_len 0, so decoding can start there
postings      doc ids ascending, delta + varint, concatenated in term order
docs          fixed 48 B records: (off, len) for path / title / date / text / tags / categories
pool          raw strings in write order, duplicates deduplicated
              tags and categories are joined with U+001F
facets        varint count + (name, ascending delta-varint doc ids)*
```

## Install

The Rust crate is consumed by path or git dependency:

```toml
[dependencies]
bigseek = { git = "https://github.com/imba97/bigseek" }
```

The WebAssembly artifacts (`bigseek_bg.wasm`, `bigseek.js`, plus a prebuilt `build-index` and
the contract vectors) are attached to every [GitHub release](https://github.com/imba97/bigseek/releases).
There is no crates.io release yet.

## Usage

### Build an index

```bash
# A) Read a markdown post directory (recommended): scans **/index.md, parses frontmatter,
#    applies the publish rules, converts markdown to plain text.
cargo run --release --bin build-index -- \
  --posts-dir ./posts \
  --output ./public/search/search.idx \
  --verify

# B) Read a documents JSON (array of { path, title, date, tags, categories, text }).
cargo run --release --bin build-index -- \
  --input ./documents.json \
  --output ./public/search/search.idx \
  --verify
```

Both inputs produce a byte-identical index for the same corpus (verified by a differential test
over 144 real posts, comparing SHA-256).

| | `--posts-dir` | `--input` JSON |
| --- | --- | --- |
| Source of truth for publish rules | Rust implementation, locked by shared vectors | the JSON producer |
| Build-time dependencies | none (reads the filesystem) | a JSON export step must run first |
| Ceiling on index content | can grow (heading levels, code languages, source offsets) | fixed by the flattening step |

### Query from WebAssembly

```bash
wasm-pack build --target web --release --out-dir pkg \
  --no-default-features --features wasm
```

```js
import init, { SearchIndex } from './bigseek.js'

await init({ module_or_path: wasmBytes }) // pass bytes explicitly to avoid MIME/path issues
const index = new SearchIndex(indexBytes)

index.query('性能优化', null, null, 40)
// → [{ path, title, date, tags, categories, snippet, score, exact }, ...]
//   tag / category: null means "unscoped"; limit: null means the default of 40
```

### Query from Rust

```rust
use bigseek::{build_index, load_index, SearchDocument};

let (bytes, stats) = build_index(&docs);
let index = load_index(bytes)?;
let hits = index.query("性能优化", None, None, 40);
```

## API

### CLI

| Flag | Description |
| --- | --- |
| `--posts-dir <dir>` | Read `**/index.md` recursively from a post directory |
| `--input <file>` / `-i` | Read a documents JSON array |
| `--output <file>` / `-o` | Write the binary index (required) |
| `--verify` | Load the written index and run a document-level recall self-check (fails below 95%) |
| `--quiet` / `-q` | Suppress progress output |

### `SearchIndex` (wasm)

| Member | Description |
| --- | --- |
| `new(bytes)` | Parse the index; throws on a bad magic, an unsupported version, or corrupt sections |
| `query(keywords, tag?, category?, limit?)` | Returns an array of hits; `tag` / `category` accept `null` for "unscoped" |
| `docCount()` / `termCount()` | Number of indexed documents / distinct terms |
| `facetCounts()` | `[tag count, category count]` |
| `formatVersion()` | Index format version, so clients can reject a mismatch early |

### Cargo features

| Feature | Default | Contents |
| --- | --- | --- |
| `cli` | yes | Markdown/YAML/JSON handling and the `build-index` binary |
| `wasm` | no | `wasm-bindgen` bindings |

The two features are independent. `comrak`, `serde_yaml_ng`, and `regex` are only pulled in by
`cli`, so a wasm build does not grow because of them. Because `cli` is a default feature,
wasm builds must pass `--no-default-features` (a wasm build that forgets it still works — LTO
drops the unused code, but the build gets slower).

## Determinism

Tokenizer and publish rules are locked by two cross-language vector files that both the Rust
and the JavaScript implementations are tested against:

| File | Locks |
| --- | --- |
| `testdata/tokenize-vectors.json` | Normalization and tokenization (bigram boundaries, fullwidth forms, case folding, single characters, kana) |
| `testdata/post-policy-vectors.json` | Publish rules (`draft` truthiness, empty dates, numeric ids, tag normalization) |

The vectors deliberately include JavaScript semantics traps, which are the places where an
implementation silently drifts: `draft: "false"` is truthy in JavaScript, `id: 7.0` stringifies
to `"7"`, and `tags: ["  a  ", "", "   ", b]` normalizes to `["a", "b"]`.

Normalization is intentionally a 1:1 per-character mapping rather than full NFKC: it avoids the
`unicode-normalization` tables and it keeps character offsets identical between the normalized
and the original text, which is what makes snippets possible without a position table. Adding
traditional/combining-character support means extending the mapping table and bumping
`FORMAT_VERSION`.

## Known trade-offs

- **No stemming, synonyms, or pinyin.** Not needed for the target corpus; extending the
  tokenizer is constrained by the vector tests.
- **The 180 KiB term dictionary dominates the index size.** It is the necessary result of 24154
  bigram terms. A real FST or bigram-only-for-body indexing would shrink it by an estimated
  20–35%.
- **Queries are slower than a JSON-backed engine** (0.37–1.11 ms versus ~0.1 ms) because of the
  OR fallback and the raw-text verification. That buys recall and exactness; against a keystroke
  budget the difference is not perceptible.
- **No intra-process concurrency.** `Index` is read-only and has no interior mutability, so it
  is `Send + Sync`; the wasm build is used single-threaded.
- **`opt-level = 3`, not `"z"`.** Measured on the 144-post corpus: `"z"` gives a 74.7 KiB wasm
  and a 0.509 ms query; `3` gives 87.1 KiB and 0.369 ms. Since the index itself is 654 KiB,
  paying 12.4 KiB for ~30% faster queries was the better trade.

## Contributing

Issues and pull requests are welcome. `cargo fmt --check`, `cargo clippy --all-targets
--all-features -- -D warnings`, and `cargo test` must pass; the wasm ABI contract lives in
`tests/wasm_abi.rs` and runs via `wasm-pack test --node`.

## License

[MIT](./LICENSE)
