# Custom-API「海关」

一个**双向审计 + 独立计量 + 自动择优路由**的自用 LLM 模型中转网关，Rust 实现。
它插在用户侧网关（例如 new-api）与上游接入点之间，把「中转站」当作需要过海关的
边境，而不是一根管道。

```
用户客户端 (OpenAI / Anthropic 兼容)
        │  用户密钥 = 网关用户令牌
        ▼
   用户侧网关（可选，例如 new-api：令牌 / 用户 / 配额 / 渠道）
        │  透传请求与响应原文
        ▼
   customsd ← 本仓库：海关代理（单点插入）
        │  A 出站：输入审查 · 滥用检测 · 危险 tool 拦截 · 脱敏留痕
        │  B 入站：usage 核验 · 模型指纹 · 注入检测 · 敏感信息过滤
        │  C 计量：本地 token 重算 · 定价 · 独立防篡改审计账
        ▼
   上游接入点 A / B / C（官方 API · 中转站 · 免费额度）
        ▲
        │  价格 · 跑分 · 指纹基线 · 性价比分
   benchd（情报层）
```

## 移植说明

本仓库是 Go 版 [`custom-api`](../custom-api) 的 **Rust 功能对等移植**：

- 行为基准是 Go 实现（协议、端点、配置格式、审计语义、错误码、流式策略），
  不是早期「双海关 + new-api」计划文档；
- 配置文件、`CUSTOMS_*` 环境变量、审计账本格式（含哈希链与锚点）保持兼容，
  Go 版写下的账本可以在 Rust 版里继续校验；
- 与 Go 版的差别（工具链、依赖、构建方式、**CLI 用双横线长选项**）记录在
  [`PORTING.md`](PORTING.md) 与 [`docs/DEPLOYMENT-zh.md`](docs/DEPLOYMENT-zh.md)；
- 当前状态：`customapi-core` 的 11 个模块 + 三个入口程序（`customsd` / `benchd` /
  `customsctl`）均已移植，`cargo test --workspace` 706 项通过；FreeBSD 上的
  89 项验收待用 Rust 二进制重跑，见
  [`docs/DEVELOPMENT-zh.md`](docs/DEVELOPMENT-zh.md) 第 10 节。

## 仓库结构

```
Custom-API/
├── crates/
│   ├── customapi-core/    网关内核（config / protocol / policy / metering /
│   │                      meter / audit / store / discovery / route / intel /
│   │                      gateway）
│   ├── customsd/          海关代理守护进程
│   ├── benchd/            情报守护进程：取价 / 指纹 / 跑分 / 性价比评分
│   └── customsctl/        运维 CLI
├── web/                   管理控制台（Vue 3 + Vite + TypeScript）
├── deploy/                rc.d、安装器、部署驱动、验收测试、mock 上游
├── vm/ lab/               测试虚拟机定义与驱动脚本
└── docs/                  部署、开发、虚拟机文档
```

## 构建

工具链由 `rust-toolchain.toml` 固定（stable），也可以用 `cargo +stable`：

```sh
cargo build --release            # 本机
cargo test --workspace           # 单元 + 集成测试
cargo clippy --workspace --all-targets --locked   # 静态检查
cargo fmt --all -- --check       # 格式

# FreeBSD 目标（静态、无 C 依赖，rustls 而非 OpenSSL）
rustup target add x86_64-unknown-freebsd
cargo build --release --target x86_64-unknown-freebsd
```

管理控制台单独构建，产物由网关直接从磁盘提供（`CUSTOMS_WEB_DIR` 可覆盖位置）：

```sh
cd web && npm ci && npm run build   # -> web/dist
```

部署与验收（FreeBSD 为目标机）：

```sh
make help                    # 目标地图
./deploy/scripts/deploy.sh local-test     # cargo fmt/clippy/test
./deploy/scripts/deploy.sh all            # 构建 -> 组装 -> 拷贝 -> 安装 -> 验收
```

## 快速上手

```sh
cp deploy/etc/customs.json /tmp/customs.json    # 按需修改 channels/providers/users
cp deploy/etc/customsd.env.example /tmp/customsd.env
cargo run -p customsd -- --config /tmp/customs.json --env-file /tmp/customsd.env
```

> 参数是**双横线长选项**（`clap`）：`--config` / `--env-file` / `--log-level` …
> 这是与 Go 版（`-config`）最直观的差异。旧写法仍能用——解析前会把已知长选项的单横线
> 补成双横线——但新写的脚本请用双横线。完整说明见
> [`docs/DEPLOYMENT-zh.md`](docs/DEPLOYMENT-zh.md) 4.1。

```sh
# 用户面：三种协议都可以用
curl http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer $CUSTOMS_USER_TOKEN" \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}'

curl http://127.0.0.1:8080/v1/responses  -H "Authorization: Bearer $CUSTOMS_USER_TOKEN" -d '{"model":"gpt-4o","input":"hi"}'
curl http://127.0.0.1:8080/v1/messages   -H "x-api-key: $CUSTOMS_USER_TOKEN" -H "anthropic-version: 2023-06-01" \
  -d '{"model":"gpt-4o","max_tokens":256,"messages":[{"role":"user","content":"hi"}]}'

# 管理面（默认只监听 127.0.0.1，远程访问需要 server.admin_remote + admin_token）
curl http://127.0.0.1:8081/admin/stats
customsctl users list                        # 默认 --admin http://127.0.0.1:8081
customsctl audit verify --config /tmp/customs.json
```

## 安全边界（摘要）

- 真实上游 API key 只从 env 文件读取（`--env-file`），不进入可 world-readable 的配置；
  凭证永不写入审计账，审计里只有加盐的 `excerpt_hash`。
- 被拦截请求的原文不落盘；只有被放行的交换才可能带脱敏样本。
- 计量记账、配额扣减、审计留痕对任何对象、任何结果都不跳过。
- 管理面能封禁用户、改写配额、读取整本审计账，因此默认只监听环回地址，
  处理器本身也拒绝非环回调用者。

## 文档

| 文档 | 内容 |
|---|---|
| [`PORTING.md`](PORTING.md) | Go → Rust 移植规范与偏差记录 |
| [`PROJECT_PLAN.md`](PROJECT_PLAN.md) | 项目现状与计划（信任模型、里程碑、与旧计划的差异） |
| [`DESIGN_NOTES.md`](DESIGN_NOTES.md) | 设计备忘（请求走查、流式、审计链、计费权威） |
| `docs/DEPLOYMENT-zh.md` | 部署、CLI 差异、上线前检查清单 |
| `docs/DEVELOPMENT-zh.md` | 开发与测试、模块边界、已知工程债 |
| `docs/VM-zh.md` | 测试虚拟机 |
| `docs/LAB-DEPLOYMENT-zh.md` | 实验环境部署实录 |
| `docs/MAKE-HELP.txt` | `make help` 的目标地图 |
