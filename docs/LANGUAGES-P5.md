# Second-phase languages and Rust macro items / 新增语言与 Rust 宏内 item

运行时代码：本分支（引擎规则版本 **10**，存储格式仍为 **2**）。"按 README 方式与上游 oracle 对比"
一节是最新一轮的结果；其后各节保留了更早一轮（规则版本 5，基线 `890912c`）的样例仓库自测记录。

## 按 README 方式与上游 oracle 对比

上游 `colbymchenry/codegraph` @ `d2e0389` 作为 oracle，两边索引同一组文件，按严格边键
（源、目标、类型、行、列）计算精确率 P 与召回率 R，门槛 P ≥ 97%、R ≥ 95%。

| 仓库 | 语言 | P | R | cgr / 上游 / 共同 | 门槛 |
|---|---|---:|---:|---|---|
| cJSON | C | 99.84 | 99.60 | 3,757 / 3,766 / 3,751 | 通过 |
| MediatR | C# | 98.17 | 97.28 | 5,685 / 5,737 / 5,581 | 通过 |
| Slim | PHP | 98.21 | 95.39 | 4,527 / 4,661 / 4,446 | 通过 |
| Sinatra | Ruby | 98.62 | 96.30 | 2,613 / 2,676 / 2,577 | 通过 |
| fmt | C++ | 94.07 | 92.79 | 20,638 / 20,923 / 19,414 | 未通过 |
| Alamofire | Swift（+文档里的 JS） | 96.46 | 93.78 | 12,164 / 12,512 / 11,734 | 未通过 |

未通过的两个仓库，差距有相当一部分是两边在同一调用位置选了不同目标。其中明确是上游选错的，
没有为了对齐去照搬：

- **fmt**：cgr 独有的 1,224 条边里，680 条是同一位置的不同选择：
  - 约 340 条明确是上游的错误：
    - 210 条 `fmt::format(..)` / `fmt::print(..)` 被上游连到 `color.h` 的 `text_style` 重载；
    - 约 130 条构造调用，上游取第一个声明的构造函数而不看参数个数，如 `bigint(42)` 连到
      `bigint()`，`uint128()` 连到 `uint128(hi, lo)`，`basic_string_view(s, n)` 连到无参构造。
  - 另约 340 条是模板重载的不同选择（`fill_n`、`format_decimal`、`write2digits` 等），两边都可能对。
  - 门槛下 cgr 独有边最多约 615 条。即使只把那约 340 条明确错误算作无法避免，也要在共同边 +463 的同时，
    把其余 cgr 独有边从约 884 条降到约 275 条。
- **Alamofire**：
  - 只看 Swift 是 P 97.41 / R 96.29，单独可以通过。
  - 差距在 `docs/` 自带的 `jquery.min.js` 与 `typeahead.jquery.js`。压缩代码里有大量嵌套作用域的同名函数：
    约 126 条是两边在同一位置选了不同目标（没有逐条核对谁对），约 130 条是上游连了、本项目没有解析。
  - 同一文件有多个同名方法时，`this.m()` 上游取行号最近的一个。

本轮新增的规则：
- C++：宏打开的命名空间里的基类（`fmt::detail::buffer<T>`）按限定名解析；
  多级成员调用（`pipe.write_end.close()`）保留字段链再按类型解析。
- JS：`this.m()` 所在的函数既不在类里、也没有 prototype 归属时（如 `_.mixin(X.prototype, {..})` 的
  对象字面量方法、传给 `defineGetter` 的 getter），若该文件恰有一个名为 `m` 的函数就连到它。
  具名接收者仍不按唯一名猜测。

对照当前 main（`2c35e7a`）：
- Express、Gin、Zod 与 main 逐边一致，只差上面这条 JS 规则带来的边：Express +6 条共同边、
  +2 条上游没有但核对正确的 getter 边；Zod +13 条共同边、+2 条正确的方法引用。
- Tokio 精确率 87.40 → 78.84、召回 86.52 → 88.30。原因是本分支提取了 `cfg_*!` 包装宏里的 Rust 条目
  （见下文），上游不提取它们，所以多出的边不在 oracle 里。
- OkHttp、Django 的差别在 0.05 个百分点以内。

## 做了什么

### C/C++、C#、PHP、Ruby、Swift

- 通过新的内核补丁 **P5** 直接 vendor 上游的五个 walker 和 Swift 语法（`scripts/sync-kernel.sh`
  会重新 vendor；每种语言各有一个 cargo feature）。walker 本身没有改动，只接入了 P2（允许错误树）
  和 P4（解析钩子）。
- `.h` 头文件按上游 `looksLikeCpp` 的规则做内容检测：含命名空间、class、模板、访问区段、
  `virtual`、`using` 或基类子句的头文件按 C++ 语法解析；含 `@interface`/`@protocol` 等
  Objective-C 标记的头文件识别为 `objc`，只记录文件节点，不用 C 语法硬解析。
- 为每种语言新增 facts 提取器（作用域、带类型的参数/局部变量/字段、导入、命名空间），各自带定向测试
  （C/C++ 28 项、C# 14 项、PHP 29 项、Ruby 38 项、Swift 17 项），另有 18 项跨文件解析的端到端测试。
- 解析规则：
  - `#include "a/b.h"` 先按包含者所在目录解析，再按路径后缀解析；`<...>` 只按后缀，找不到视为外部。
  - C# 命名空间作为 package；同命名空间和**外层命名空间**的类型无需 `using` 即可见；`using X = A.B.C`
    和 `using static` 解析到具体声明。
  - PHP `use A\B\C` 解析到类/接口/trait/函数或命名空间；`$this->m()`（内核输出为裸名）按隐式 `this` 成员查找；
    `parent::`、`self::`、`static::`、`->`、`?->` 归一化为通用形状。
  - Ruby `require`/`require_relative` 按 `lib/` 优先的路径后缀解析；`@ivar.m` 视为 `self.@ivar.m`；
    `self.class.m` 视为 `self.m`；module 也作为成员持有者。
  - Swift 按 SwiftPM target（`Sources/<T>/`、`Tests/<T>/`）划分模块，模块内声明无需导入可见；
    `Data()`、`String(..)` 等标准类型的构造调用判为 builtin，即使项目里有 `extension Data`。
  - 其他文件中声明的成员（C# partial class、Swift extension、Ruby 重开类、C++ 类外定义）可被成员查找命中。
  - 有已知接收者类型但找不到成员时**不再**退化为全局唯一同名方法（保持"宁缺毋错"）。
  - 每个语言族各有 builtin 与常见方法名列表，避免无类型接收者连到同名项目方法。

### Rust：条件 item 包装宏

Tokio 的 `cfg_rt! { pub struct X; impl X { .. } }` 这类宏体对 tree-sitter 是不透明的 token 树，
内核既不提取里面的声明也不提取调用。现在在内核运行前，用一个**保持位置不变**的视图把包装宏的
`name! {` 和闭合 `}` 替换成空格（换行保留，其他字节不动），其余所有节点的行列与 id 不变。
若替换后出现语法错误，回退到原文。`cfg_if!`、宏定义和任意其他宏保持不透明，不展开任何宏。

## 样例仓库结果

同一台机器，同一个二进制，`cgr index` 后 `export --sqlite` 统计 `refs` 表中 `kind='calls'` 的状态。
"仅 walker"一列是只有内核 walker、没有 facts 和解析规则时的数字，用来说明本轮规则带来的变化；
它不是上游的数字。

| 仓库 | 语言 | 文件 | 调用引用 | 仅 walker：已解析 | 本轮：已解析 | 本轮：NoCandidate | 本轮：Ambiguous |
|---|---|---:|---:|---:|---:|---:|---:|
| cJSON | C（+Ruby 脚本） | 68 | 3,743 | 2,552 | 2,637 | 208 | 2 |
| fmt | C++ | 80 | 19,835¹ | 7,689 | 9,571 | 6,386 | 277 |
| MediatR | C# | 159 | 3,381 | 303 | 572 | 1,833 | 27 |
| Slim | PHP | 125 | 5,183 | 344 | 1,060 | 2,299 | 3 |
| Sinatra | Ruby | 147 | 2,794 | 389 | 589 | 914 | 39 |
| Alamofire | Swift | 111 | 18,060 | 1,026 | 4,703 | 3,752 | 398 |

¹ fmt 的调用引用数在 `.h` 按 C++ 解析后由 18,905 增加到 19,835。

未解析（NoCandidate）的大头是外部库调用（.NET BCL、PSR 接口、Foundation、标准库）和无类型接收者，
按设计不连边。fmt 的 `namespace fmt` 由宏 `FMT_BEGIN_NAMESPACE` 打开，tree-sitter 看不到，
`fmt::format(..)` 这类调用的限定名根未知；自审后不再按末段名回退，约 1,976 条由 Ambiguous 变为
NoCandidate。两种状态都不连边，边数不受影响。

每个仓库各抽 14 条已解析调用人工核对源码行，发现并修复了两类误连：Ruby 已知模块上缺失成员时的全局回退
（`Delegator.target` 连到测试类方法），以及 Swift 对标准类型 extension 的构造调用（Alamofire 中 164 条）。
修复后对应计数为 0。这只是抽样，不是精确率估计。

### 自审修复

提交前做了一轮代码自审（34 条候选问题），修复后把六个样例仓库**自审前后的每一条引用逐条对比**
（不抽样），对每条新增和目标变化回到源码核对：

- 健壮性：五个 facts 提取器加深度上限（400）并把递归辅助函数改为迭代，深层嵌套不再有栈溢出风险；
  宏包装预处理只处理真正闭合的 `name! { .. }`，并且只在屏蔽后的语法错误数不多于原文时才采用；
  预处理超时按超时文件记录。
- 确定性：跨文件成员和限定名在多个候选间按（调用文件 → 被包含/导入文件 → 路径、行号）选择，
  按"邻近"无法区分时不再猜。
- C/C++：`::a::f` 按绝对限定名查找；`::open()` 只认当前文件及其（间接）包含文件的全局声明，
  不再连回包装它的 `test::open`；`::std::..` 视为标准库；C++ 文件里未限定的 `get`/`swap`/`move`
  等在作用域和同文件都找不到时视为标准库（C 文件不受影响）；类外成员只在类型名唯一或存在包含关系时跨文件匹配；
  `#include <a/b.h>` 能对应到项目内头文件。
- PHP：`parent::__construct()` 连到父类；保留命名空间（`\A\f()`、`namespace\f()`）；
  内置函数前缀收窄，`get_`/`is_`/`str_` 这类项目常见前缀改为精确名单。
- Ruby：单段 `require 'x'` 只在 `lib/`、`spec/`、`test/`、`app/` 和根目录下找，`require 'logger'`
  不会连到 `lib/mygem/logger.rb`；变量重新赋值后类型跟着更新。
- Swift：标准类型判断改为显式名单（含 `Result`、`Error`）；同名的项目 struct/enum/protocol/typealias
  会遮蔽标准类型，但嵌套声明只在其外层声明内部生效（Alamofire 的 `PathMonitor.Result` 不影响
  `Result { try .. }`）；`throw Error()` 在 `Throwing` 内部连到嵌套的 `Throwing.Error`。
- C#：查询表达式 `join .. into`；Swift 枚举 case 模式绑定。

逐条对比的净结果（调用引用，自审前 → 自审后）：fmt 新增 24 条（gtest 的 `::testing::..`、
`::FMT_POSIX` → `include/fmt/os.h`；另有 13 条 `#include <fmt/..>` 解析到项目头文件），去掉 114 条，
其中 102 条核对为错边（`fmt::arg` → `scan_context::arg` 69 条、`fmt::scan` → 测试里同名的成员方法 21 条、
`fmt::ptr` → `scan_buffer::ptr` 8 条、`::fstat`/`::GetFileSize`/`::CreateThread` 连到同名包装函数 4 条），
其余 12 条多为构造函数调用（`fmt::bytes(..)`、gtest 的 `TestResult()` 等），因不再按唯一名猜测而变为未解析，
属于真实的召回损失；Alamofire 新增 1 条（嵌套 `Throwing.Error`），去掉 10 条错边（`NSNumber(..)`、
`CharacterSet(..)`、`Notification(..)` 连到项目 extension）；Slim 的 `parent::__construct()` 等 4 条改连到正确的父类方法；
cJSON、MediatR、Sinatra 不变。

`cgr bench sync --edits 40 --verify` 在六个仓库上均满足 **incremental == full rebuild**
（随机编辑规则已扩展到新语言的注释、新增函数、重命名和导入）。

### Tokio（Rust）

| | 基线 | 本轮 |
|---|---:|---:|
| 节点 | 14,460 | 15,199 |
| 边 | 39,114 | 44,231 |
| 已解析引用 | 21,520 / 62,945 | 25,301 / 65,414 |
| 已解析调用 | 11,608 | 13,824 |
| 调用 NoCandidate | 11,636 | 10,886 |
| 调用 Ambiguous | 1,076 | 607 |

`cgr bench sync --edits 60 --verify` 满足 incremental == full rebuild（50 次增量、10 次完整重建），
`cgr debug diff` 一致，`CGR_THREADS=1` 与多线程得到相同的规范图。

与基线逐引用对比：共同引用 36,425 条，新增解析 1,973 条，丢失 74 条，目标变化 69 条。丢失的 74 条
多为 `reader.read_exact()` 这类此前靠全局唯一名连到 `tokio-util` 的 `SyncIoBridge::read_exact`
的边；它们本就与接收者类型无关，现在接收者有了类型信息后不再误连。Express、Gin、Django 的图逐引用
与基线**完全一致**（0 变化）。

## 验证范围

- 工作区 `cargo test --workspace`：全部通过（含随机编辑序列 6 × 100）；`cargo clippy --workspace
  --exclude cg-kernel --all-targets -- -D warnings` 与 CI 范围的 `cargo fmt --check` 通过。
- 自审修复后：六个样例仓库各 40 次随机编辑、Tokio 60 次随机编辑均满足 incremental == full rebuild；
  Tokio、Express、Gin、Django 与自审前逐引用一致；Tokio 单线程与多线程图一致。最后两处 C++/Swift 修复
  （只作用于 C++ 与 Swift 代码路径）之后，重跑了全部测试、clippy，以及 fmt 与 Alamofire 的 40 次随机编辑校验。
- Windows：`x86_64-pc-windows-gnu`（mingw-w64）交叉编译通过，包括 vendor 的 Kotlin 与 Swift C 语法；
  本环境没有 Wine，未能运行生成的 `cgr.exe`，msvc 目标与 `install.ps1` 需要在真实 Windows 上验证。
- 上游 oracle 对比见本页开头一节（最新一轮）；更早一轮的环境无法构建 oracle，那一轮的数字只是自测。
  Express 的全图 P、Django 路由等 v4 遗留问题没有处理。

## English summary

Kernel patch P5 vendors the upstream C/C++, C#, PHP, Ruby and Swift walkers and the Swift
grammar; `.h` headers with C++ constructs use the C++ grammar. Each language gets a facts
extractor with focused tests and resolution rules (includes, namespaces incl. enclosing C#
namespaces, PHP `use`, Ruby `require`, SwiftPM modules, implicit-`this` members, call-name
normalisation, members declared in other files, per-family builtins). A known receiver type with
no such member no longer falls back to a unique global name, and Swift constructor calls of
standard types are builtins even when the project extends them.

Rust items inside conditional wrapper macros (`cfg_rt! { .. }`) are extracted from a
position-preserving view with only the wrapper tokens blanked; on Tokio this adds 739 nodes and
2,216 resolved call references while Express, Gin and Django are unchanged reference-for-reference.

Against the upstream oracle (`d2e0389`, strict edge keys), cJSON, MediatR, Slim and Sinatra meet
the P ≥ 97% / R ≥ 95% gate; fmt (94.07 / 92.79) and Alamofire (96.46 / 93.78) do not. Much of the
gap is call sites where the two pick different overloads or scopes; in fmt about 340 of those are
clear upstream errors (the first declared constructor regardless of arity, `color.h` overloads). Tokio's oracle precision drops from 87.40 to 78.84
because the `cfg_*!` items extracted here are absent upstream. The earlier-round sample-repository
numbers above are self-measurements with random spot checks, not a precision estimate. `incremental == full rebuild` holds on
all six repositories over 40 random edits each. A self-review pass (34 candidates) added depth
bounds to the extractors, deterministic tie-breaking, C++ global-namespace and `::std` handling,
PHP `parent::` and namespaces, narrower Ruby `require` and an explicit Swift standard-type list; the
sample repositories were then diffed reference-by-reference against the pre-review graph and every
gained or retargeted edge was checked against source. Windows: a mingw cross-build succeeds; the
binary was not executed here.
