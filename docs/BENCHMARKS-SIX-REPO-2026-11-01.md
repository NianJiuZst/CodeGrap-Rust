# Six-repository main retest / 六仓库 main 复测（部分完成）

A subsequent [graph difference investigation](GRAPH-DIAGNOSTICS-2026-11-01.md)
traces the JavaScript and TypeScript gaps and includes a controlled Zod manifest experiment.

The six-repository retest covers full indexing and strict graph agreement at `edb403e`,
with upstream `d2e0389`. It adds TypeScript (Zod) and Kotlin/Java (OkHttp) to the
previous four repositories. Index speedup geometric mean is **4.77×** and median peak
RSS is **78.3%–88.7% lower** across these samples. This is not an equal-graph or semantic
correctness guarantee. Six repositories do not pass the joint 97% precision / 95% recall
strict upstream-agreement gate. Zod call-edge recall is 35.28%; OkHttp call-edge recall is
64.57%. The table below includes exact revisions and the complete per-repository results.

The graph key is `(source stable ID, target stable ID, kind, line, UTF-16 column)` with
NULL distinct from zero; precision is the intersection divided by cgr edges and recall by
upstream edges. Upstream is a reference, not semantic ground truth. Both sides have identical
indexed paths and source-byte hashes; error-marked files and all edge kinds are retained.
Both copies exclude YAML/XML; OkHttp also excludes nine `.properties` files that only upstream
would index. Its first, unmatched-input timing samples were discarded and all timings rerun.

This is a **partial retest**: only Express incremental sync was sampled; other sync and all
query measurements are unfinished after the next preflight refused due to insufficient
available memory. No after-restoration graph equality, standard-tier, with-readers or agent
A/B claim is made. The [earlier report](BENCHMARKS.md) retains the full sync and query
results at its older revision. [Structured evidence](benchmarks/2026-11-01-six-repo-retest.json)
contains the completed samples and per-kind, per-language graph counts.

被测 main：`edb403e75c2c4d44f25ac0c6be2b59a7109537a1`；原版 `d2e0389b519be398f9bc616b6f7bea8a7edfbc6a`。两者均使用生产构建、原版原生内核；限制为 2 个工作线程。六个仓库均预热一次、计时三次，每条运行间隔至少 30 秒。峰值 RSS 为每次 /usr/bin/time 测量的中位数。缓存为热缓存。

| 仓库 | 提交 | 相同文件数 | 原版索引 ms | Rust 索引 ms | 倍数 | RSS MiB 原版 → Rust | 全边精确率 | 全边召回率 | 调用边精确率 | 调用边召回率 |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| [express](https://github.com/expressjs/express/tree/1c6f1b0fe4000977b9a7b4f34097e4d527dd9824) | `1c6f1b0` | 142 | 536.2 | 136.0 | 3.94× | 225.6 → 48.9 | 58.76% | 88.35% | 49.26% | 85.70% |
| [gin](https://github.com/gin-gonic/gin/tree/0f09c3a9b4626d9fe9979cf4d0521d6ae32ab646) | `0f09c3a` | 99 | 620.8 | 130.5 | 4.76× | 246.9 → 37.0 | 93.58% | 96.53% | 99.75% | 94.12% |
| [tokio](https://github.com/tokio-rs/tokio/tree/b499257ef62bd631b83a988d371f4330f545670c) | `b499257` | 814 | 2592.2 | 555.3 | 4.67× | 733.6 → 83.3 | 87.40% | 86.52% | 91.78% | 68.91% |
| [django](https://github.com/django/django/tree/a05e16a7230fe33d3f43f4a3aaed103cebb42efc) | `a05e16a` | 2984 | 10597.8 | 1934.0 | 5.48× | 1896.8 → 237.4 | 94.20% | 89.83% | 91.34% | 77.47% |
| [zod](https://github.com/colinhacks/zod/tree/2554b70c3d4a80273381e61d527d820720626440) | `2554b70` | 523 | 2287.0 | 474.1 | 4.82× | 627.2 → 93.8 | 91.77% | 61.84% | 88.81% | 35.28% |
| [okhttp](https://github.com/square/okhttp/tree/16fb6aef3aee1c4645fc8d24af01f8a4dc128ebf) | `16fb6ae` | 690 | 3136.6 | 613.9 | 5.11× | 814.2 → 109.0 | 88.48% | 82.22% | 80.46% | 64.57% |

精确率 = 交集 / Rust 边数，召回率 = 交集 / 原版边数；键包括来源与目标稳定 ID、边类型、行号和 UTF-16 列号，NULL 不视为 0，保留所有边、路由及有错误标记的文件。原版不是人工标注真值，这些数值是相对原版的一致性。六仓库均未同时达到 97% / 95% 的设计目标。各仓库双方路径和源文件字节摘要一致。

Zod 有 4 个、OkHttp 有 50 个 Rust 文件被标记为解析或提取异常；它们保留在分母中。OkHttp 原版会额外收集 9 个 `.properties` 文件，已在双方一致排除后重新计时与比较；此前采样无效。所有仓库统一排除上游独有的 YAML/XML 文件。

Express 的增量同步测试已完成：

- 无变更：原版 170.8 ms → Rust 40.8 ms（三次中位数）。
- 注释修改：原版 304.4 ms → Rust 60.5 ms（三次中位数）。
- 单文件重命名：原版 299.6 ms → Rust 175.4 ms（三次中位数）。

其余仓库的同步以及全部查询测评尚未完成：重启此阶段时可用内存不足 6 GiB，原始保护脚本拒绝运行。未声称恢复后全量图谱或增量一致性全部通过。本次测量的样本、边计数、按语言和类型统计及错误文件见 [机器可读结果](benchmarks/2026-11-01-six-repo-retest.json)。原始数据库与导出留在测评机器，未作为仓库文件提交。
