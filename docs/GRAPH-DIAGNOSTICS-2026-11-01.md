# 六仓库图谱差异调查 / Six-repository graph investigation

本调查基于 [六仓库复测](BENCHMARKS-SIX-REPO-2026-11-01.md) 留存的图：`cgr` 运行时代码为
`edb403e75c2c4d44f25ac0c6be2b59a7109537a1`，上游为 `d2e0389b519be398f9bc616b6f7bea8a7edfbc6a`。
文档提交 `580a486` 不改变被测运行时。两边输入路径及源文件字节相同；每条边按来源稳定 ID、目标稳定 ID、
类型、行号和 UTF-16 列号比较，NULL 与 0 不等价。包括语法错误标记文件、路由和 `contains`；不按差异剔除样本。

这些数据是**相对于原版的一致性，而不是有人工真值的绝对精确率和召回率**。下列全量分桶来自全部去重边；
具体正确与否只对列出的源码样例进行了判定。未逐条人工审核全部差异。数据及每个仓库的固定提交见
[机器可读结果](benchmarks/2026-11-01-six-repo-retest.json)。

## 六个仓库的差异

| 仓库 | 全边匹配 / Rust 独有 / 原版独有 | 调用边匹配 / Rust 独有 / 原版独有 | 主要结构差异与抽查 |
|---|---:|---:|---|
| Express | 2,062 / 1,447 / 272 | 1,097 / 1,130 / 183 | Rust 独有调用中 661 条指向 `application.js::use`；其中部分是原版漏连，部分接收者错误。另有 277 条额外路由 `contains`。 |
| Gin | 7,793 / 535 / 280 | 3,583 / 9 / 224 | 231 条 Rust 独有边是文件到路由的 `contains`。例如原版连出了 [`(JSON{data}).Render(w)`](https://github.com/gin-gonic/gin/blob/0f09c3a9b4626d9fe9979cf4d0521d6ae32ab646/render/render_test.go#L36) 到 [`JSON.Render`](https://github.com/gin-gonic/gin/blob/0f09c3a9b4626d9fe9979cf4d0521d6ae32ab646/render/json.go#L57)，Rust 图缺失此边。 |
| Tokio | 34,334 / 4,952 / 5,348 | 10,754 / 963 / 4,851 | 调用、引用和导入均有差异。不能把原版独有边都当成漏连：[`opcode::Read::new(...).offset(...).build()`](https://github.com/tokio-rs/tokio/blob/b499257ef62bd631b83a988d371f4330f545670c/tokio/src/io/uring/read.rs#L112) 被原版连到 Tokio runtime 的 `Builder.build`，接收者并非该类型；Rust 将其标为未解析。 |
| Django | 174,372 / 10,740 / 19,733 | 44,022 / 4,176 / 12,804 | 路由、Python 调用、导入、引用均有差异。原版把 [MySQL introspection 的 `cursor.execute`](https://github.com/django/django/blob/a05e16a7230fe33d3f43f4a3aaed103cebb42efc/django/contrib/gis/db/backends/mysql/introspection.py#L17) 连到了 Oracle 后端的 `execute`；JavaScript 部分还发现作用域错连，见下文。 |
| Zod | 22,088 / 1,982 / 13,628 | 6,399 / 806 / 11,741 | 大量调用由于包清单被忽略而标为外部；同名类型/值、重载和有错误的语法树也产生差异。 |
| OkHttp | 43,522 / 5,665 / 9,413 | 15,258 / 3,705 / 8,371 | Rust 侧 50 个文件的解析树带错误标记；原版对错误树可能回退到 WASM。原版也有错误：将 [HTTP 调用的 `execute`](https://github.com/square/okhttp/blob/16fb6aef3aee1c4645fc8d24af01f8a4dc128ebf/android-test/src/androidTest/java/okhttp/android/test/AndroidDnsTest.kt#L70) 连到 Gradle 的 [`BndBuildAction.execute`](https://github.com/square/okhttp/blob/16fb6aef3aee1c4645fc8d24af01f8a4dc128ebf/build-logic/src/main/kotlin/BndBuildAction.kt#L83)。 |

路由 `contains` 计数是结构差异，不自动表示程序行为错误。各类自动计数只表示边的差集；
同一调用位置可以有多个目标，故两侧的“同位置不同目标”边数不必相同。

源语言拆分也说明不能把 Express 的结果直接当成 JavaScript 的整体精确率：

| 仓库 / 来源语言 | 匹配 / Rust 边 / 原版边 | 相对精确率 | 相对召回率 |
|---|---:|---:|---:|
| Express / JavaScript | 2,062 / 3,509 / 2,334 | 58.76% | 88.35% |
| Django / JavaScript | 563 / 590 / 657 | 95.42% | 85.69% |
| Zod / JavaScript | 79 / 79 / 80 | 100.00% | 98.75% |
| Zod / TypeScript | 21,657 / 23,599 / 35,243 | 91.77% | 61.45% |
| Zod / TSX | 352 / 392 / 393 | 89.80% | 89.57% |

这只是当前样本，不是全语言质量估计。Zod 的主要问题首先是**召回率**，不是 61% 的精确率。

## Zod：包清单被忽略，仓库内导入被当成外部

Zod 的 [`packages/zod/.gitignore`](https://github.com/colinhacks/zod/blob/2554b70c3d4a80273381e61d527d820720626440/packages/zod/.gitignore#L7)
包含 `**/package.json`，但 [`packages/zod/package.json`](https://github.com/colinhacks/zod/blob/2554b70c3d4a80273381e61d527d820720626440/packages/zod/package.json)
本身已由 Git 跟踪，其中声明了包名和入口。当前 [`find_config_files`](../crates/cg-scan/src/walk.rs)
对配置文件应用忽略规则，而 [`ProjectInfo::load`](../crates/cg-resolve/src/project.rs) 依赖其结果；
因此包清单未进入项目配置。与源代码是否被索引是两个不同的问题。

实际索引的 `debug unresolved` 将
[`import * as z from "zod/v4"` 及 `z.object()` / `z.string()`](https://github.com/colinhacks/zod/blob/2554b70c3d4a80273381e61d527d820720626440/packages/bench/compile-codec-direction.ts#L1)
标为 `External`，尽管工作区有相应入口。直接使用原索引的模块解析器验证：不加载该清单时
`zod`、`zod/v4`、`zod/v4/core` 均为 `External`；仅将该清单加入配置文件集合后，三者分别指向
`packages/zod/src/index.ts`、`src/v4/index.ts`、`src/v4/core/index.ts`。

另做一次**诊断性消融**：在独立副本的该 `.gitignore` 末尾添加 `!package.json`，不改解析器；
重新索引后，双方仍是相同的 523 个源文件，所有被索引文件的字节与原样本一致。与同一原版图比较：

| 指标 | 原始测评 | 仅放行包清单 |
|---|---:|---:|
| 全边交集 / Rust / 原版 | 22,088 / 24,070 / 35,716 | 32,980 / 35,069 / 35,716 |
| 全边相对精确率 / 召回率 | 91.77% / 61.84% | 94.04% / 92.34% |
| 调用边交集 / Rust / 原版 | 6,399 / 7,205 / 18,140 | 16,728 / 17,633 / 18,140 |
| 调用边相对精确率 / 召回率 | 88.81% / 35.28% | 94.87% / 92.22% |

例如 `z.object()`、`z.string()` 在原图中没有调用边，放行后分别连到实际的
`classic/schemas.ts::object` 和 `::string`。这证明清单遗漏是本样本的大型影响因素；不证明新增边
全部正确，也不代表 `main` 已修复。正式基准仍是未修改忽略规则的原始结果，诊断运行没有做性能比较。

清单问题不是唯一缺口：Zod 入口先 `import * as z`，再 `export { z, z as default }`。即使放行清单，
[`import { z } from "zod"` 后的 `z.string()`](https://github.com/colinhacks/zod/blob/2554b70c3d4a80273381e61d527d820720626440/packages/integration/fixtures/internal-types/cjs-test.cts#L3)
仍为 `NoCandidate`。[导出表构造](../crates/cg-resolve/src/engine.rs)处理无来源的重导出时只查文件本地节点，
没有追踪同名导入绑定。一个单独的三文件对照复现了这一点；在该最小例子里原版也没有产出调用边。
原版在真实 Zod 图中另有同一调用被连到测试文件 [`Mocker.string` getter](https://github.com/colinhacks/zod/blob/2554b70c3d4a80273381e61d527d820720626440/packages/zod/src/v3/tests/Mocker.ts#L12)
的例子，该目标不是导入的 `z.string()` 实现。不能通过全局同名猜测来“修复”召回率。

## Zod：类型/值命名空间、重载与语法树

1. **同名类型和运行时值发生混淆。** [`core.ts`](https://github.com/colinhacks/zod/blob/2554b70c3d4a80273381e61d527d820720626440/packages/zod/src/v4/core/core.ts#L8)
   有 `interface $constructor`，第 51 行另有同名运行时函数。原图中 255 条 Rust 独有调用指向接口；
   例如 [`core.$constructor(...)`](https://github.com/colinhacks/zod/blob/2554b70c3d4a80273381e61d527d820720626440/packages/zod/src/v4/classic/errors.ts#L76)
   应调用函数。两文件最小对照在 Rust 图中指向接口、原版指向函数。
   [`build_exports`](../crates/cg-resolve/src/engine.rs) 当前遇到先出现的类型和后出现的非类型时保留前者，
   而 [`Accept::Callable`](../crates/cg-resolve/src/resolve.rs) 也允许类型节点作为调用目标。修复需要区分
   TypeScript 的类型与值命名空间；简单地始终丢弃类型会破坏类型引用。
2. **重载选择是指标差异，不应一律算 Rust 错误。** 原图有 189 条 Rust 独有调用指向
   [`Doc.write(line: string)` 声明](https://github.com/colinhacks/zod/blob/2554b70c3d4a80273381e61d527d820720626440/packages/zod/src/v4/core/doc.ts#L25)，
   原版常指向前一个 `write(fn: ModeWriter)` 声明。抽查 [`doc.write(模板字符串)`](https://github.com/colinhacks/zod/blob/2554b70c3d4a80273381e61d527d820720626440/packages/zod/src/v4/core/compile.ts#L364)
   时 Rust 的签名选择与参数更吻合；两者都不是第 27 行的运行时实现。图的目标约定需明确，不能单凭严格差集判错。
3. **语法树有错误标记。** Zod 四个核心 TypeScript 文件的 Rust 解析树带 `PARSE_ERROR`。
   直接用当前内核语法解析时，合法的 [`out` 变型标注](https://github.com/colinhacks/zod/blob/2554b70c3d4a80273381e61d527d820720626440/packages/zod/src/v4/classic/schemas.ts#L34)
   和 [`in` 标注](https://github.com/colinhacks/zod/blob/2554b70c3d4a80273381e61d527d820720626440/packages/zod/src/v4/core/checks.ts#L28)
   出现 `ERROR` 节点；微型 `interface Box<out T>` 也复现，而去掉标注的对照不报错。
   上游内核遇错误树时可能转到 WASM 恢复路径；Rust 继续使用原生错误树。当前尚未隔离这部分对最终指标的净影响，
   也不能把全部错误标记文件中的差异归因于变型语法。

## JavaScript：接收者兜底与词法作用域

Express 的 1,130 条 Rust 独有调用中，1,122 条在原版图中没有相同来源与位置的调用；其中 661 条
目标是 `lib/application.js::use`，按调用文本分为 573 次 `app.use`、61 次 `router.use` 和其他接收者。
这些是按文本统计，不是 661 条逐条语义裁决。

- [示例中的 `app.use`](https://github.com/expressjs/express/blob/1c6f1b0fe4000977b9a7b4f34097e4d527dd9824/examples/auth/index.js#L21)
  有证据指向 application 的 `use`：`app` 来自 `express()`，而工厂将 application 原型混入 app。
  原版缺这条调用边；同一行原版反而把 `express.urlencoded()` 连到 `createApplication`，但该属性实际来自 body-parser。
- [application 内部的 `router.use`](https://github.com/expressjs/express/blob/1c6f1b0fe4000977b9a7b4f34097e4d527dd9824/lib/application.js#L216)
  是 `this.router` 的方法，当前 Rust 图把它连回 `app.use`，形成错误自调用。
  [`fallback_method`](../crates/cg-resolve/src/resolve.rs) 在接收者未知时按语言内唯一成员名称兜底，
  名字唯一并不能证明对象身份。独立最小对照中，未标注的 `router.use()` 被 Rust 错连到唯一的 `app.use`；
  原版未产出该边。另有 21 条 Rust 独有 `get` 调用指向示例文件中的 `users.get`；其中
  [`app.resource` 里的 `this.get(...)`](https://github.com/expressjs/express/blob/1c6f1b0fe4000977b9a7b4f34097e4d527dd9824/examples/resource/index.js#L13)
  和 [`application.js` 的 `this.get('env')`](https://github.com/expressjs/express/blob/1c6f1b0fe4000977b9a7b4f34097e4d527dd9824/lib/application.js#L155)
  接收者都不是该示例中的 users 对象。修复应在缺少接收者证据时保守处理，并保留有证据的应用/框架边。
- Django [`inlines.js` 的第二个闭包](https://github.com/django/django/blob/a05e16a7230fe33d3f43f4a3aaed103cebb42efc/django/contrib/admin/static/admin/js/inlines.js#L346)
  在第 421–423 行调用自己的三个局部函数，Rust 图却指向第一个闭包里的同名函数；原版在此例中指向正确的第二组。
  两个闭包、各自定义同名 `const helper = function ...` 的最小例子同样复现：第二次调用被连到第一处声明。
  [`var_decl`](../crates/cg-extract/src/facts/js.rs) 为函数表达式绑定创建 `LocalFn` 时没有记录节点行，
  [`resolve_name_args`](../crates/cg-resolve/src/resolve.rs) 随后按文件内同名节点取第一个，丢失了绑定身份。

## 建议顺序及边界

1. 修复项目自身**受跟踪**配置清单的发现，同时保留用户显式排除、忽略目录和外部依赖边界；覆盖正常索引、增量失效、配置删除/改名和跨平台情况。
2. 为 TypeScript 类型与值分别建模；补充同名接口/函数、类/值与调用/类型引用的断言，以及命名空间导入再导出链。
3. 为局部函数表达式保留精确绑定节点；覆盖相邻闭包、嵌套回调、同名遮蔽和增量重建。
4. 收紧未知接收者的唯一名称兜底，分别覆盖真实 Express app、外部 Router、同名对象和 Django 全局对象。
5. 独立评估 TypeScript 语法兼容和 Kotlin 错误树恢复：OkHttp 的 `fun interface` 也能在原生语法树中复现错误，另有 `has_error` 但没有显式 `ERROR` 节点的文件。升级语法或恢复路径都需要验证节点 ID、位置和两边图的兼容性。

本调查未修改运行时代码，也未声称上述建议均已修复。最小复现及完整图差异保留在测评机器的仓库外；
这里的计数和样例用于定位，不取代后续有真实语义预期的回归测试。
