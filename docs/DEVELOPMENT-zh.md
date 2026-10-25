# 开发指南

本文件面向改 Rust 代码的人。行为基准是 Go 版实现（`../custom-api`），移植约定写在
[`PORTING.md`](../PORTING.md)；这里讲**怎么在本仓库里干活**。

## 1. 环境要求

| 用途 | 要求 |
|---|---|
| 构建网关 | Rust stable，由 `rust-toolchain.toml` 固定（组件 `rustfmt`、`clippy`；实测 rustc 1.99.1）；`Cargo.toml` 的 `rust-version = "1.89"` 是最低要求 |
| 交叉构建 FreeBSD | `rustup target add x86_64-unknown-freebsd`，外加**目标平台的 C 工具链**：`ring` 会编译 C 与汇编，`export CC_x86_64_unknown_freebsd=<freebsd-cc>`（或用 `cargo-zigbuild`，或在 FreeBSD guest 里原生构建） |
| 管理控制台 | Node.js + npm（Vue 3 + Vite + TypeScript，产物 `web/dist`） |
| 构建测试虚拟机 | libvirt、QEMU、`mkisofs`（seed ISO 用它生成）、`xz`、`curl` |
| 目标机验收 | `curl`、`python3`（mock 上游），`jq` 可选（没有时验收脚本退到 python3） |
| 可选 | `cargo-llvm-cov`（覆盖率；未装时 `make test-cover` 退化为跑一遍测试） |

国内镜像：cargo 走 crates.io（guest 里如需加速，写 `/root/.cargo/config.toml` 的源替换，
那是 cargo 配置项而不是环境变量）；pkg 与 FreeBSD 镜像走 USTC，见 `vm/create-vm.sh`
与 `docs/VM-zh.md`。

---

## 2. 仓库结构

```
Custom-API/
├── crates/
│   ├── customapi-core/       网关内核（库）
│   │   └── src/
│   │       ├── lib.rs        模块声明 + crate::json 的 Go 兼容 JSON 助手
│   │       ├── version.rs    构建标识（option_env! 注入）
│   │       ├── config/       mod.rs 配置与校验、envfile.rs 密钥文件、provider.rs 提供商展开
│   │       ├── protocol/     types.rs 线协议类型、sse.rs、rewrite.rs 原地改写、
│   │       │                 translate.rs 跨协议转换、responses.rs、stream_translate.rs
│   │       ├── policy/       engine.rs 规则引擎、rules.rs 内置规则表、evaluate.rs 判定
│   │       ├── metering/     tokenizer.rs 估算器、counter.rs usage 比对
│   │       ├── meter/        price.rs 价格归一化与优惠建模
│   │       ├── audit/        ledger.rs 审计账与锚点、rehash.rs 链重建、testutil.rs
│   │       ├── store/        配额、周期重置、聚合视图
│   │       ├── discovery/    模型目录：上游发现与配置声明合并、io.rs 缓存读写
│   │       ├── route/        渠道择优、健康熔断、诚信降权、route.rs 打分
│   │       ├── intel/        指纹散度、跑分套件、取价、评分装配、baseline/probe/grade
│   │       └── gateway/      mod.rs 服务与路由、proxy.rs 代理管线、stream.rs 流式、
│   │                         protocols.rs 失败分类与按协议 IO、api.rs 用户/管理端点、
│   │                         admin_providers.rs 提供商视图、web.rs 控制台静态文件
│   ├── customsd/             海关代理守护进程
│   │   └── src/              main.rs 组装与信号处理、cli.rs 参数、args.rs 参数兼容层
│   ├── benchd/               情报守护进程
│   │   └── src/              main.rs、cli.rs、tasks.rs（prices/fingerprint/bench/score）、io.rs
│   └── customsctl/           运维 CLI
│       └── src/              main.rs 命令分发、args.rs、commands/{config,audit,users,prices}.rs、
│                             ui.rs（对齐 Go 的 tabwriter 输出）、util.rs
├── web/                      管理控制台（Vue 3 + Vite + TS，独立 npm 工程）
│   ├── src/                  main.ts / App.vue / router.ts / api.ts / auth.ts / store.ts
│   │                         components/（展示组件）、views/（9 个页面）
│   └── scripts/              mock-admin.mjs（假管理 API）、check-auth.mjs（前端测试）
├── deploy/                   rc.d、安装器、部署驱动、验收脚本、mock 上游、串口工具
├── vm/                       libvirt 测试虚拟机定义与创建脚本
├── lab/                      在实验室 libvirt 主机上驱动 guest 的脚本
├── docs/                     部署、开发、虚拟机、实验部署文档
├── dist/                     构建产物（发布目录 + SHA256SUMS）
└── target/                   cargo 缓存（不随发布交付）
```

### 分层依赖方向

```
protocol ← metering ← gateway
protocol ← policy   ← gateway
config   ← 所有层
meter    ← route    ← gateway
store, audit, discovery ← gateway
intel    ← benchd
```

`discovery` 只依赖 `config`：它做的判断是"这个模型名该不该出现在目录里"，
不需要知道价格、配额或安全策略，因此网关之外的调用者（管理 API、测试）也能直接用它。

**刻意避免的依赖**：`route` 不依赖 `metering`。若需要表达"用量偏差"，`route` 自带一个
`route::Deviation` 值类型，由 `gateway` 在调用处做转换；否则会形成
`metering → protocol`、`route → metering` 的耦合，把"选路"与"分词"这两个本应独立的
关注点绑在一起。

### 几条与 Go 版不同的内部约定

- **配置是不可变快照**。Go 用 `sync.RWMutex` 包住 `*config.Config`，调用方拿指针就能改；
  Rust 侧 `Server::config()` 返回 `Arc<Config>`，重载或测试里改上游地址走
  `Server::update_config()`。凡是"读配置顺手改一下"的写法都要换掉。
- **库内错误用 `thiserror` 的具体枚举**，消息文本尽量与 Go 一致（运维日志与测试会比对）；
  可执行文件里才允许 `anyhow`。
- **日志用 `log` crate**（`log::info!` 等），不要引入 `tracing`；日志级别来自配置，
  不读 `RUST_LOG`（rc.d 用固定环境启动服务）。
- **命令行兼容层**：三个二进制都在解析前调用 `args::normalize_argv`，把**已知长选项**的
  单横线写法补成双横线（`-config` → `--config`），其它 token 原样透传。新增选项时必须
  同时把它登记进 `LONG_OPTIONS`，否则旧写法的 `-选项` 不会被识别。
- **JSON 兼容是硬约束**：字段名、字段顺序、`omitempty` 语义、`time.Duration` 的字符串
  形态、`time.Time` 的零值，都要与 Go 对齐，配置与账本才能互读。改写请求体时必须保持
  「未触碰字段的原始字节不变」，不要先反序列化再整体序列化。

---

## 3. 本地开发循环

```sh
cargo build --workspace                       # 全量构建
cargo fmt --all -- --check                    # 格式（gofmt 的对应物）
cargo clippy --workspace --all-targets --locked   # 静态检查（go vet 的对应物）
cargo test --workspace --locked               # 单元 + 集成
cargo test -p customapi-core policy::         # 单个模块
cargo test -p customapi-core --lib -- --list  # 列出所有用例

# 覆盖率（需要 cargo-llvm-cov）
cargo llvm-cov --workspace --locked
```

一键（与 CI/交付同一套判定）：

```sh
make local-test          # == ./deploy/scripts/deploy.sh local-test
```

Rust 没有竞态检测器可开，`make test-race` 就是跑一遍 `cargo test --workspace`
（历史目标名保留，让运维习惯与文档不至于断掉）。

当前基线（本机实测）：`cargo test --workspace --locked` **733 个测试全部通过**。
其中 `customapi-core` 637 个，按模块分布：

| 模块 | 用例数 | 模块 | 用例数 |
|---|---|---|---|
| `protocol` | 139 | `audit` | 39 |
| `gateway` | 126 | `meter` | 36 |
| `intel` | 55 | `discovery` | 30 |
| `policy` | 51 | `config` | 37 |
| `metering` | 45 | `store` | 32 |
| `route` | 40 | `json` / `version` | 7 |

三个入口程序另有 96 个：`customsctl` 30 单元 + 20 集成、`benchd` 17 + 10、
`customsd` 10 + 9。集成测试在各自 crate 的 `tests/cli.rs`，用
`env!("CARGO_BIN_EXE_<name>")` 启动**真实二进制**，比对 stdout 与 Go 版逐字节一致的
文本（`--check` 摘要、`audit verify` 报告、帮助与退出码）——部署脚本会解析这些文本，
所以它们属于接口的一部分。

### 用 curl 手工打网关

```sh
mkdir -p /tmp/dev
cat > /tmp/dev.json <<'EOF'
{
  "server": {"listen":"127.0.0.1:8080","admin_listen":"127.0.0.1:8081",
             "audit_path":"/tmp/dev/audit.jsonl","state_path":"/tmp/dev/state.json",
             "log_level":"debug"},
  "channels": [{"name":"mock","base_url":"http://127.0.0.1:18080","protocol":"openai",
                "models":{"mock-basic":"mock-basic"}}],
  "users": [{"name":"dev","token":"dev-token","quota":0}],
  "prices": [{"channel":"mock","model":"mock-basic",
              "price":{"input_per_m":10,"output_per_m":30,"currency":"CNY"}}]
}
EOF

# 起一个 mock 上游（复用验收测试用的那个）
python3 deploy/scripts/mock-upstream.py 18080 &

# 起网关（注意 Rust 版是双横线长选项）
cargo run -p customsd -- --config /tmp/dev.json &

curl -sS http://127.0.0.1:8080/v1/chat/completions \
  -H "Content-Type: application/json" -H "Authorization: Bearer dev-token" \
  -d '{"model":"mock-basic","messages":[{"role":"user","content":"hi"}]}' | jq .

cargo run -p customsctl -- audit tail --config /tmp/dev.json --n 5
cargo run -p customsctl -- audit verify --config /tmp/dev.json
```

mock 上游的行为可以用控制接口切换：

```sh
curl -X POST http://127.0.0.1:18080/admin/control -d '{"reply":"自定义回复"}'
curl -X POST http://127.0.0.1:18080/admin/control -d '{"force_status":500}'
```

模型名选择行为：`mock-basic` 诚实、`mock-inflate` 虚报 20 倍、`mock-inject` 返回注入载荷、
`mock-toolcall` 返回危险工具调用、`mock-discovered-only` 只在上游模型列表里出现
（用于验证发现结果与配置声明的合并）。mock 同时讲三种协议，可以直接验证
`/v1/chat/completions`、`/v1/responses`、`/v1/messages`。

---

## 4. 增加一条检测规则

规则表在 `crates/customapi-core/src/policy/rules.rs`。两类写法：

```rust
// 关键词规则：大小写不敏感的子串匹配（helper 会把词表转小写）
keywords(
    "default.my_new_rule",
    INBOUND,
    SEV_HIGH,
    "response",
    "一行给运维看的说明",
    &["触发短语", "another phrase"],
),

// 正则规则：需要容忍空白/变体时使用
Rule {
    id: "default.my_regex_rule".to_string(),
    direction: INBOUND.to_string(),
    severity: SEV_CRITICAL.to_string(),
    field: "response".to_string(),
    detail: "...".to_string(),
    re: Some(must_compile(r"(?i)\bfoo\b[^\n]{0,80}\bbar\b")),
    ..Rule::default()
},
```

**规则 ID 是稳定契约**：它会原样出现在审计记录里，运维的日志查询依赖它，
重命名是破坏性变更。

然后：

1. 在 `policy/tests.rs` 的表驱动用例里加入**正例与反例**——反例（良性文本不应命中）
   和正例同样重要，否则规则会退化成"什么都拦"。
2. 若规则涉及流式，加一条 `stream_detector…` 用例把短语**拆成多块**验证滑动窗口能重组。
3. 跑 `cargo test -p customapi-core policy::`。

内置规则用 `must_compile` 编译，写错正则会在构造引擎时 panic（内置规则的错误必须在
开发期暴露，而不是在运行时静默失效）；运维在配置里写的正则则走 `PolicyError`，
带上 `policy: outbound.block_patterns:` 之类的前缀。

---

## 5. 增加一个供应商的价格形态

在 `crates/customapi-core/src/meter/price.rs` 的 `normalize` 里加一个 `scheme` 分支，
并补测试。归一化的目标是产出一对「每百万 token 的输入/输出价」——只有落到这个形状，
不同上游才可比。

```rust
"my_scheme" => {
    pm.input_per_m = 从原始字段换算(&r) * rate;
    pm.output_per_m = pm.input_per_m * cr;
}
```

要求：

- **无法可靠换算时返回 `MeterError`，不要猜**。错误的价格会污染路由决策，
  比缺失价格危险得多。
- 保留 `source` 与 `scheme` 字段，让价格可追溯、可审计。

---

## 6. 测试分层

| 层次 | 目的 | 位置 |
|---|---|---|
| 单元 | 纯函数、规则表、换算、判分、SSE、改写 | `crates/customapi-core/src/<module>/` 内的 `#[cfg(test)] mod tests`，用例多时独立成 `tests.rs` |
| 集成 | 网关全管线：鉴权、配额、拦截、虚报、流式、故障转移、管理面门禁 | `gateway/tests.rs`（真起三个 HTTP 服务） |
| mock 上游 | 在线验收用的可控上游（三种协议 + 虚报/注入/工具调用） | `deploy/scripts/mock-upstream.py` |
| 前端 | 令牌处理（Node 跑真实模块，不装浏览器） | `web/scripts/check-auth.mjs`（`npm run test`） |
| 在线验收 | 真实 FreeBSD + 真实二进制（89 项 / 16 组） | `deploy/scripts/smoke-test.sh` |
| 情报闭环 | 取价 / 指纹 / 跑分 / 评分 / 路由反馈（13 项） | `deploy/scripts/intel-test.sh` |

集成测试**不用 `httptest` 式进程内调用**：Rust 侧没有既跑 hyper 又伪造 `RemoteAddr`
的等价物，所以 `gateway/tests.rs` 在环回随机端口上起三个真服务（mock 上游、用户面、
管理面），请求经 `reqwest` 走真实 HTTP，帧、流式与头部处理都被真实验证。
唯一无法用环回地址表达的是"远端调用者"，所以管理面门禁直接断言
`Server::admin_gate` 的判定——这与 Go 测试通过 `AdminHandler` 断言的是同一组决策。

> 前端测试用 `typescript`（已是 devDependency）把 `src/auth.ts` 剥类型后在 Node 里
> 加载，并注入 `sessionStorage` / `window.history` 等最小浏览器全局。
> 这一段代码只在部署后的控制台里跑，用浏览器做自动化成本高、还不稳定；
> 用 Node 跑真实模块，成本和稳定性都更好。

几条本仓库坚持的约定：

1. **安全属性要有专门的反向测试。** 例如"诚实上游不被误报"与"虚报必被抓"成对存在。
   只测"能抓到"会诱使阈值越调越紧，最终把正常流量全部误伤。
2. **测试要能发现真问题，而不是复述实现。** 移植过程中写入测试后被发现并修复的真实
   缺陷包括：`store` 回调重入死锁、`route` 的健康表与 RNG 数据竞争、`Content.Parts`
   字段遮蔽方法、无限偏差导致审计账无法序列化、`Content-Length` 未随改写后的 body
   更新、缓冲流式在校验前提交了 200 状态、`sed` 未处理行尾导致 JSON 路径转换失效。
3. **改了存储格式就要有"旧数据仍可校验"的测试。** 哈希改为对**存储的字节**计算，
   并且必须证明改名前后"篡改仍然能被发现"——只证明"现在能验证"是不够的。
4. **以 root 跑的运维动作要有归属与权限的测试。** `customsctl audit rehash` 重建文件时
   用临时文件 + rename，会把账本变成 root 所有，导致**服务下一次启动报权限拒绝**。
   相关用例在非 root 下跳过、在 root 下验证属主与权限被保留（用 `sudo` 跑一次）。
5. **验收测试必须能在目标机上独立运行**，不依赖开发机、不依赖网络（用 mock 上游）。
6. **不要为了让测试通过而放宽断言。** 如果一条断言反复失败，先确认是被测代码错了
   还是断言错了——移植过程中两者都发生过。
7. **安全相关的默认值要用测试钉住。** 管理面的远程访问默认关闭、空令牌不等于放行、
   伪造的 `X-Forwarded-For` 不能冒充环回——改动这类代码时它们会先炸。

测试不能依赖外网、不能依赖固定端口（验收脚本除外，它按预设的私有端口排队使用）、
不能靠 `sleep` 等超时（用注入时钟/可控时间）。

---

## 7. 交叉编译与部署

```sh
cd web && npm ci && npm run build && cd ..   # 先构建前端，stage 才会带上它

./deploy/scripts/deploy.sh build      # cargo build --release --target x86_64-unknown-freebsd
./deploy/scripts/deploy.sh stage      # dist/customapi-<version>/ + SHA256SUMS
./deploy/scripts/deploy.sh ship       # scp 到 $CUSTOMAPI_TARGET
./deploy/scripts/deploy.sh install
./deploy/scripts/deploy.sh test
```

`stage` 会把 `web/dist` 一起打进发布包。**前端不参与 cargo 构建**：网关从磁盘读
`web/dist`（默认搜索若干相对路径与 `/usr/local/share/customapi/web`，可用
`CUSTOMS_WEB_DIR` 覆盖），目录不存在时提供一个说明页而不是启动失败——所以干净检出
也能 `cargo build`，纯后端部署也能跑。

版本号来自 `Cargo.toml` 的 `[workspace.package].version`；commit、构建时间与 rustc 版本
由 `deploy.sh` 通过 `CUSTOMS_VERSION` / `CUSTOMS_COMMIT` / `CUSTOMS_BUILD_TIME` /
`CUSTOMS_RUSTC` 环境变量注入（`option_env!` 在**编译期**读取），细节见
`docs/DEPLOYMENT-zh.md` 2.0。

**不要引入 C 依赖**：上游客户端用 `reqwest` 的 `rustls-tls`（纯 Rust TLS），
发布二进制不带 OpenSSL 之类的 port 依赖。

---

## 8. 代码风格

- 注释解释**为什么**，不解释**是什么**。`// increment i` 是噪声；
  `// The floor is not optional: on a short reply one token is a large relative deviation`
  是信息。
- 公共类型与函数要有文档注释（Go 里有文档注释的，这里必须有等价的一份）。
- 错误信息要能直接指导操作，例如
  `chown root:customapi ... && chmod 640 ...` 而不是 `permission denied`。
- 审计记录里**绝不写入用户原文或密钥**，只写加盐哈希与规则名。
- 结构性取舍（例如容差默认值的推导）写进文档，不要只留在代码注释里。
- 不引入 `tracing`、`anyhow`（库公共 API）、异步执行器替换、框架级重构；
  依赖清单由 `[workspace.dependencies]` 统一维护。

---

## 9. 调试技巧

```sh
# 最详细的日志
cargo run -p customsd -- --config /tmp/dev.json --log-level debug

# 只看某类请求
cargo run -p customsctl -- audit tail --config /tmp/dev.json --n 50 --verdict over_billed
cargo run -p customsctl -- audit tail --config /tmp/dev.json --actor alice

# 审计记录里的样本（默认不下发，需显式请求）
curl 'http://127.0.0.1:8081/admin/audit?limit=5&include_samples=true'

# 路由为什么选了它
curl -s http://127.0.0.1:8081/admin/audit | jq -r '.records[0].route_reason'

# 分阶段延迟（全部毫秒，字段名自带单位：total_ms / outbound_audit_ms /
# upstream_ms / inbound_audit_ms / ttfb_ms）
curl -s http://127.0.0.1:8081/admin/audit | jq '.records[0].latency_ms'

# 这一次为什么被翻译了（客户端协议 vs 上游实际协议）
curl -s http://127.0.0.1:8081/admin/audit | jq '.records[] | select(.translated) | {protocol, upstream_protocol, model}'

# 提供商和它展开后的渠道（配置里的 supports 在这里叫 protocols）
curl -s http://127.0.0.1:8081/admin/providers | jq '.providers[] | {name, protocols, channels: [.channels[].channel]}'

# 模型目录（配置声明的与发现来的分别标记）
curl -s http://127.0.0.1:8081/admin/models | jq '.models[] | {model, configured, channels: [.channels[].channel]}'

# 在 FreeBSD 上看某次验收测试的全部产物
sudo ls -dt /tmp/customapi-e2e.* | head -1
sudo cat $(sudo ls -dt /tmp/customapi-e2e.* | head -1)/gateway.log
```

`log_level=debug` 时网关会为每个请求打印一行判定摘要（决策、渠道、尝试次数），
与审计记录一一对应；差别是审计记录落盘、日志随 newsyslog 轮转。

---

## 10. 移植状态

诚实记录当前进度，避免文档比代码跑得快：

| 项 | 状态 |
|---|---|
| `customapi-core` 全部模块 | 已移植，637 个测试通过 |
| Go 测试用例翻译 | 按模块随移植落地；用例名保留 Go 的语义 |
| `customsd` 入口 | 已接线：clap 参数、两个监听器、信号处理、配额周期重置、周期锚点、停机前补锚点 |
| `benchd` 入口 | 已接线：`run` / `prices` / `fingerprint` / `bench` / `score` 五个任务 |
| `customsctl` 入口 | 已接线：`config check`、`audit verify/rehash/tail/stats`、`users list/grant/add/suspend/resume`、`prices compare/normalize`；帮助文本里仍列着 `fingerprint baseline`（Go 版有，Rust 版未实现，指纹生成走 `benchd fingerprint`） |
| 配置/审计账格式兼容 | 已按 `PORTING.md` 第 4 节逐字段对齐，并有跨格式校验用例 |
| 三个入口的测试 | 共 96 项通过（`customsd` 19、`benchd` 27、`customsctl` 50，含启动真实二进制的进程级测试） |
| FreeBSD 验收（89 项） | 脚本已按 Rust CLI 改好，**尚未在 Rust 二进制上跑通** |
| 情报闭环（13 项） | 同上 |
| 控制台 | `web/` 已存在且可构建；网关侧静态服务（`gateway/web.rs`）已就绪 |

重跑验收时不需要改脚本：`deploy/scripts/*.sh`、`deploy/rc.d/*`、`deploy/scripts/install.sh`
都已经在用 Rust 的写法（双横线长选项、`benchd ... run`）。真正要盯的是本节最后两行——
首次在 FreeBSD 上跑 Rust 二进制时，最可能出问题的地方是交叉构建的 C 工具链
（`ring`）与 pkg 版 `rust` 的版本差异，而不是参数。

## 11. 已知的工程债

| 项 | 现状 | 影响 | 建议 |
|---|---|---|---|
| 分词器为无词表估算 | 误差 −46%…+17% | 严格核验受限；靠绝对下限抑制误报 | 若要 5% 级核验，接入真实 BPE 词表（约 1.5 MB 数据） |
| 跨协议翻译是"降级可用"而非等价 | 三种协议都能收发；`openai → responses` 与 `openai → anthropic` 有逐事件的流式翻译，其余组合退化为整包缓冲后转换 | 工具调用、思考字段、服务端状态在跨协议时会退化；因此默认"先同协议" | 需要更高保真时，优先让上游讲同一种协议，而不是加强翻译 |
| 管理面外壳不受门禁 | HTML/JS/CSS 对未认证调用者可见，数据接口受门禁 | 暴露"这里有个控制台"，不暴露任何数据 | 有意设计：否则浏览器拿不到输入令牌的页面 |
| 平台级限流未做 | 用户级限流已做；跨进程/集群级未做 | 单实例假设 | 多实例部署时前置共享限流 |
| 指纹特征为响应长度 | 依赖 JS 散度于长度分布 | 对"刻意模仿长度"的替代模型不敏感 | 有 logprob 权限时可加 token 分布维度 |
| `benchd` 任务是串行的 | 逐渠道逐模型顺序执行 | 渠道多时一轮耗时长 | 需要时加并发（注意上游限速） |
| 锚点默认写在本机 | `anchor.jsonl` 与账本同目录 | 本机被攻陷时两者可被一起改写 | 把锚点同步到异地存储或日志系统 |
| FreeBSD 重验收未做 | 入口已接线，但只在 Linux/本机验证过 | 发布前缺少"真机 89 项"这一层证据 | 按第 10 节重跑 `make all`，把结果记进发布说明 |
