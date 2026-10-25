# 移植规范（Go → Rust）

本文件是「功能对等移植」的施工规范。所有参与移植的 agent / 开发者都必须遵守；
目标不是写一个「类似」的网关，而是让 Rust 版在行为、线格式、审计语义上与 Go 版
可互换。

## 0. 两个仓库

| 角色 | 路径 | 说明 |
|---|---|---|
| 行为基准（只读） | `/home/dy/ProjectSpace/CustomAPI/custom-api/src/customapi` | Go 实现，含全部测试与文档 |
| 落点（本仓库） | `/home/dy/ProjectSpace/CustomAPI/Custom-API` | Rust 实现 |

Go 版是最新的事实标准：它已经实现并验证了「单点插入的自建海关网关」。
本仓库根目录里旧的 `PROJECT_PLAN.md` / `DESIGN_NOTES.md` 描述的「双海关 + new-api」
是历史方案，**不作为实现依据**，后续会被改写。

## 1. 包映射

| Go 包 | Rust 模块 | 行数（Go，含测试） |
|---|---|---|
| `internal/version` | `customapi_core::version` | 19 |
| `internal/config` | `customapi_core::config` | 1690 |
| `internal/protocol` | `customapi_core::protocol` | 3666 |
| `internal/policy` | `customapi_core::policy` | 1137 |
| `internal/metering` | `customapi_core::metering` | 672 |
| `internal/meter` | `customapi_core::meter` | 404 |
| `internal/audit` | `customapi_core::audit` | 1353 |
| `internal/store` | `customapi_core::store` | 600 |
| `internal/intel` | `customapi_core::intel` | 1209 |
| `internal/discovery` | `customapi_core::discovery` | 784 |
| `internal/route` | `customapi_core::route` | 751 |
| `internal/gateway` | `customapi_core::gateway` | 3300 |
| `cmd/customsd` | `crates/customsd` | 235 |
| `cmd/benchd` | `crates/benchd` | 546 |
| `cmd/customsctl` | `crates/customsctl` | 634 |

模块目录结构镜像 Go 包内的文件划分，例如
`src/protocol/{mod.rs,types.rs,sse.rs,rewrite.rs,translate.rs,responses.rs,stream_translate.rs}`。

## 2. 已完成 / 归属

- `config` 与 `version` 已由主控完成，**不要重写**；需要增补行为时先报告。
- `lib.rs` 与各 `Cargo.toml` 由主控维护：**不要自行修改**，有依赖需求在报告里提。
- 每个 agent 只修改自己负责的模块目录（以及该模块内的 `#[cfg(test)]`）。

## 3. 语言与工程约定

- 工具链由 `rust-toolchain.toml` 固定为 stable（当前 1.99），edition 2021。
- 异步运行时是 tokio；HTTP 服务端 axum 0.7；上游客户端 reqwest 0.12（rustls）。
- 序列化 serde/serde_json；哈希 sha2；时间 chrono；正则 regex；CLI clap 4。
- 公共 API 命名用 snake_case；类型用 CamelCase；与 Go 导出名对应（`RecountUsage` →
  `recount_usage`，`Ledger.Append` → `ledger.append`）。
- 错误：库内用 `thiserror` 定义具体错误枚举，消息文本尽量与 Go 一致（测试与运维日志
  会比对）。不要用 `anyhow` 出现在库公共 API 里；可执行文件里可以用。
- 日志用 `log` crate（`log::info!` 等），不要引入 tracing。
- 注释写「为什么」，与 Go 注释同等信息量；Go 里的设计说明要翻译保留，不要丢。

## 4. JSON 与线格式对等

Go 的 `encoding/json` 行为必须在 Rust 侧复现：

- 字段名逐字照抄 JSON tag；结构体字段顺序与 Go 声明一致（保证输出字节稳定）。
- `omitempty` → `#[serde(default, skip_serializing_if = "...")]`；
  数值/字符串/切片/映射的判空用 `crate::json` 里的 `is_zero_i64` /
  `is_zero_f64` / `is_zero_i32` / `String::is_empty` / `Vec::is_empty` /
  `HashMap::is_empty`。
- `*bool` → `Option<bool>`；`time.Duration` → `crate::json::Duration`；
  `time.Time` → `chrono::DateTime<Utc>`（零值用 `config::zero_time()`）。
- 配置结构体一律 `#[serde(deny_unknown_fields)]`。
- 枚举若 Go 用 `type X string` + 常量且允许任意字符串（配置/审计读取），Rust 侧用
  `String` + 常量 + 校验函数，不要用封闭 enum（否则旧数据无法解析）。
- 手写 JSON 改写（`protocol/rewrite.go`）必须保持「不改变未触碰字段的原始字节」这一
  性质：不要先反序列化成结构体再整体序列化。

## 5. 测试要求

- 把 Go 的每个测试用例都翻译成 Rust 测试，放在同模块的 `#[cfg(test)] mod tests`，
  或用例较多时放 `tests/` 子模块；测试函数名保留 Go 用例名的语义。
- 表驱动测试 → `for (name, case) in [...] { ... }` 并在失败信息里带上名字。
- 除翻译测试外，补充 Rust 侧边界测试（空输入、超长、非法 UTF-8 边界、溢出）。
- 验收标准：`cargo test -p customapi-core <模块>` 全绿，`cargo build` 无错误，
  `cargo clippy --all-targets` 没有明显警告（允许 `too_many_arguments` 之类结构性的）。
- 测试不能依赖外网、不能依赖固定端口、不能 sleep 等超时（用可控时间/注入时钟）。

## 6. 依赖与网络

- 依赖清单在根 `Cargo.toml` 的 `[workspace.dependencies]`，按需在
  `crates/customapi-core/Cargo.toml` 里 `{ workspace = true }` 引用。
  确实需要新依赖时先在报告里说明理由，由主控统一加。
- crates.io 可用但很慢；`cargo` 会缓存，不要并行下载同一批依赖。
- 不要访问外网做测试；需要上游时用 `httpmock` 风格的手写 `TcpListener` 或注入
  `reqwest::Client`。

## 7. 交付报告格式

完成后用中文报告，包含：

1. 新增/修改的文件清单；
2. 公共 API 一览（签名）；
3. 已翻译的 Go 测试数量与新增测试；
4. `cargo test` 结果（原样粘贴最后几行）；
5. 与 Go 行为的偏差（如果有，必须显式列出原因），以及未完成项；
6. 需要主控处理的依赖/Cargo.toml 变更。

## 8. 已知偏差（实现完成后的记录）

移植过程中刻意保留、或无法等价的差异，按影响面排序：

1. **JSON 数值与时间写法**：core 的 `json::{GoFloat, GoTime, format_go_f64}` 已让整数浮点写
   `40` 而不是 `40.0`、时间写 RFC3339Nano 去尾零（`.5Z`），审计账本、state、目录、
   管理 API 都已挂上，因此 Rust 写出的账本能被 Go 版 `customsctl audit verify` 校验，
   反之亦然。残余差异：NaN/Inf 在 Go 里是序列化错误，这里写 `null`。
   **时间戳时区**：Go 用 `time.Now()` 写本地偏移（`+08:00`），Rust 统一写 UTC（`Z`）。
   同一瞬间、同样的 RFC3339 语义，读取端（`customsctl audit tail` 与管理 API 的消费方）
   按存储里的偏移呈现，因此迁移前后混在一本账里也能正确排序与显示；但逐字节对比会看到
   偏移写法不同。选择 UTC 是为了让证据不随部署机时区变化。
2. **空集合与 `null`**：Go 的 nil slice 序列化成 `null`、带 `omitempty` 的字段直接省略；
   Rust 侧已按字段逐一对齐（`/admin/providers` 的 `retry_on` 与直连通道 `protocols` 写
   `null`，`/admin/config` 的 `models` 为空时省略），其余带 `omitempty` 的字段用
   `skip_serializing_if` 省略。
3. **HTML 转义**：Go 的 `encoding/json` 把 `<` `>` `&` 转成 `\u003c` 等，serde_json 不转。
   只影响"翻译产物"，转发路径不重新编码，用户收到的字节不受影响。
4. **JSON 解码宽松度**：Go 字段名大小写不敏感、重复键后者胜；serde 精确匹配、重复键报错。
   标准客户端不受影响。
5. **`metadata` 的 nil 与空 map**：Go 用 nil 表示"直连通道信任任意协议"、显式 `{}` 表示
   "声明为空"；Rust 的 `HashMap` 无法区分，按"缺失=信任"处理（与常见配置一致）。
6. **错误文本内层**：外层前缀、错误码、HTTP 状态逐字对齐；内层细节来自 serde_json /
   reqwest / std::io，与 encoding/json / net/http / os 的措辞不同。
7. **时区（优惠窗口）**：`chrono::FixedOffset` 只支持固定偏移，Go 的 `*time.Location` 支持夏令时；
   优惠的低谷时段窗口按固定偏移计算。
8. **负 Duration**：Go 的 `ParseDuration` 接受 `-30s`，Rust 配置层拒绝（负超时无意义）。
9. **CLI 长选项**：规范写法是双横线（`--config`），并保留一层窄兼容：已知长选项的单横线
   写法会被补成双横线，所以 Go 时代的脚本仍然可用。
10. **管理面门禁依赖 peer 地址**：core 的 `Server::admin_handler()` 从
   `ConnectInfo<SocketAddr>` 取调用者地址，服务端必须用
   `into_make_service_with_connect_info::<SocketAddr>()`（`customsd` 已这样做），
   否则所有 `/admin/*` 会被当成远程调用者拒绝。
11. **`customsctl audit rehash`**：Go 版在"链损坏 + 锚点过期"组合下会把 anchor 移动两次而
    以 rc=1 结束（修复已完成但退出码误导）；Rust 按实际结果返回 rc=0 并打印新的锚点位置。
12. **健康熔断只统计传输失败**：Go 的 `forward` 只在"请求构造失败 / 连接失败"时调用
    `health.Failure`，可重试的 HTTP 状态（5xx/429）只触发本次请求的 failover，不会给渠道
    记失败。Rust 保持一致（补齐的单测把这条语义钉死）；若希望 5xx 也开熔断，需要同时改
    Go 与 Rust，属行为变更而不是移植。
