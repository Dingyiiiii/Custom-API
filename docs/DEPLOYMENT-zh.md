# FreeBSD 部署指南

目标平台：**FreeBSD 15.1-RELEASE amd64**（4C/8G/119G UFS）。本系统也可在 FreeBSD
14.x 上运行，但只有 15.1 经过验收测试。

> Rust 版说明：发布二进制是静态的单文件（`rustls`，不含 OpenSSL），目标机上不需要
> 安装任何 port，也不需要 C 工具链。构建侧的要求（交叉工具链、cargo 缓存）见 2.0。
> 三个入口程序（`customsd` / `benchd` / `customsctl`）已接线，参数与 Go 版一一对应
> （见 4.1）；FreeBSD 上的 89 项 / 13 项验收目前仍沿用 Go 版的记录，Rust 版发布后需要
> 在同一台机器上重跑一遍，见 `docs/DEVELOPMENT-zh.md` 第 10 节。

---

## 1. 部署形态

```
                    ┌─ 用户客户端 ────────────────────────┐
                    │  OpenAI / Anthropic 兼容 SDK        │
                    └──────────────┬──────────────────────┘
                                   │  Bearer <用户令牌>
                    ┌──────────────▼──────────────────────┐
                    │  new-api        :3000               │  ← 可选：令牌/用户/配额 UI
                    │  （可省略；本系统能独立运行）        │
                    └──────────────┬──────────────────────┘
                                   │  透传
                    ┌──────────────▼──────────────────────┐
                    │  customsd       :8080 (用户面)      │  ← 本系统：审查/计量/审计/选路
                    │                 :8081 (管理面，本机) │
                    └──────────────┬──────────────────────┘
                                   │  Bearer <上游密钥>
                    ┌──────────────▼──────────────────────┐
                    │  上游接入点（官方 API / 中转站 / …）│
                    └─────────────────────────────────────┘

                    ┌─────────────────────────────────────┐
                    │  benchd  周期性：取价 / 指纹 / 跑分 │  ← 通过 customsd 打真实流量
                    └─────────────────────────────────────┘
```

**两种接法都可以：**

- **推荐**：渠道只配一个，指向 new-api；由 new-api 管理真实上游。此时本系统专注审计、
  计量与选路，令牌与配额 UI 复用 new-api。
- **直连**：把真实上游直接列为多个 channel（或 provider）。此时本系统同时承担渠道管理
  职责。两种方式的路由、计量、审计逻辑完全一致。

> 与早期「双海关 + new-api」计划不同，本系统是**单点插入**：用户面、管理面、上游
> 出口都在同一个进程里，没有 A/B 两段代理，也不依赖 new-api 存在。new-api 只是
> 可选的、可被替换的前置。

---

## 2. 构建与安装

### 2.0 构建（cargo）

本机（任意平台）：

```sh
cargo build --release                      # -> target/release/{customsd,benchd,customsctl}
cargo test --workspace --locked            # 单元 + 集成
```

FreeBSD（交叉编译）：

```sh
rustup target add x86_64-unknown-freebsd
cargo build --release --target x86_64-unknown-freebsd
# -> target/x86_64-unknown-freebsd/release/{customsd,benchd,customsctl}
```

交叉链接还需要**目标平台的 C 工具链**：`rustls` 栈里的 `ring` 会编译 C 与汇编，
只装一个 Rust target 是不够的。三种可行做法：

1. 导出 `CC_x86_64_unknown_freebsd=<freebsd-cc>`（FreeBSD 的 `cc` 或交叉工具链）；
2. 用 `cargo-zigbuild`；
3. **在 FreeBSD 上原生构建**——测试虚拟机的 `baseline` 就装了 `rust` 包，正是为了
   这条退路（见 `docs/VM-zh.md` 第 5 节）。

`deploy/scripts/deploy.sh build` 把上面这些包装成一条命令，并且**在缺少 target 时
退回宿主原生构建并明确警告**（"the binaries will NOT run on FreeBSD"）——这样
`stage`/`ship` 的演练仍然可做，但绝不可能把宿主二进制误当成 FreeBSD 发布。

#### 版本注入

Go 版用 `-ldflags -X` 在链接期塞版本号；Rust 没有等价的链接期注入，改由**编译期**
读取环境变量（`option_env!`），所以这些变量必须在 `cargo build` 的那一刻存在：

| 变量 | 来源 | 缺失时 |
|---|---|---|
| `CUSTOMS_VERSION` | `Cargo.toml` 的 `[workspace.package].version` | `0.1.0-dev`（开发构建会自报家门） |
| `CUSTOMS_COMMIT` | `git rev-parse --short HEAD` | `unknown` |
| `CUSTOMS_BUILD_TIME` | `date -u +%Y-%m-%dT%H:%M:%SZ` | `unknown` |
| `CUSTOMS_RUSTC` | `rustc --version` 的版本号 | `unknown` |

`customsd --version`（以及 `customsctl version`）输出形如：

```
customapi 0.1.0 (commit 6a1f2c3, built 2026-10-24T12:00:00Z, freebsd/amd64, rustc 1.99.1)
```

三点容易踩：

- **`option_env!` 对 cargo 的指纹不可见**。版本号变了但源码没变时，cargo 会认为
  `customapi-core` 不需要重编，于是发布包里的二进制仍然自称旧版本。`deploy.sh build`
  因此先执行 `cargo clean --release -p customapi-core`（只丢这一个 crate 的 release
  产物，依赖缓存与 `cargo test` 的 debug 构建都还在）。
- **发布二进制会被 strip**（Go 版是 `-s -w`）。`Cargo.toml` 的
  `[profile.release]` 已设 `strip = "symbols"`、`lto = "thin"`；`deploy.sh` 另外用
  `--config 'profile.release.strip="symbols"'` 显式覆盖一次，避免以后改 profile 时
  悄悄把调试符号带进发布包。
- **`panic = "unwind"` 是刻意保留的**：网关按请求捕获 panic 并继续服务，改成 abort
  会让一次上游异常演变成整进程退出。

### 2.1 从开发机一键部署

```sh
./deploy/scripts/deploy.sh build      # 交叉编译 FreeBSD 发布版
./deploy/scripts/deploy.sh stage      # 组装 dist/customapi-<version>/ + SHA256SUMS
./deploy/scripts/deploy.sh ship       # scp 到目标机 /tmp/customapi-release
./deploy/scripts/deploy.sh install    # 目标机上执行安装器
./deploy/scripts/deploy.sh test       # 89 项验收测试

# 环境变量 CUSTOMAPI_TARGET 或 --target user@host 指定目标
CUSTOMAPI_TARGET=root@llm.example ./deploy/scripts/deploy.sh all
./deploy/scripts/deploy.sh all --target root@10.0.0.9
```

`make` 只是这些命令的别名（目标地图见 `make help` / `docs/MAKE-HELP.txt`）：

```sh
make build stage ship install test
make all
```

`stage` 产出的目录：

```
dist/customapi-<version>/
├── bin/{customsd,benchd,customsctl}
├── etc/{customs.json,customsd.env.example}
├── rc.d/{customsd,benchd}
├── scripts/{install.sh,smoke-test.sh,intel-test.sh,verify-deployment.sh,
│            mock-upstream.py,customsd-wrapper.sh}
├── docs/            仓库 docs/*.md（供目标机上就地查阅）
├── web/             构建好的控制台（缺省时警告，网关退化为说明页）
├── RELEASE          version / commit / built / target / rustc
└── SHA256SUMS
```

`RELEASE` 里的 `target` 记录的是**实际出产物所用的三元组**，`dist/.build-triple`
把它传给下一次单独执行的 `stage`——所以宿主原生构建冒充 FreeBSD 发布这件事，
在发布元数据里看得见。

### 2.2 在目标机上手工安装

```sh
# 1. 把发布目录拷到目标机
scp -r dist/customapi-0.1.0 root@host:/tmp/customapi-release

# 2. 安装（幂等；重复执行等于升级）
ssh root@host 'sh /tmp/customapi-release/scripts/install.sh'

# 需要连配置模板一起覆盖时（会先备份）：
ssh root@host 'sh /tmp/customapi-release/scripts/install.sh --force-config'

# 装到别的前缀（默认 /usr/local）：
ssh root@host 'sh /tmp/customapi-release/scripts/install.sh --prefix /opt/customapi'
```

安装器做了什么：

| 动作 | 位置 |
|---|---|
| 创建服务用户 `customapi`（nologin） | `pw useradd` |
| 安装二进制 | `/usr/local/sbin/{customsd,benchd}`、`/usr/local/bin/customsctl` |
| 安装降权启动器 | `/usr/local/libexec/customapi/customsd-wrapper` |
| 安装 rc.d 脚本 | `/usr/local/etc/rc.d/{customsd,benchd}` |
| 安装配置模板（默认不覆盖；新版落成 `customs.json.new`，`--force-config` 时备份后覆盖） | `/usr/local/etc/customapi/customs.json` |
| 安装密钥模板（占位，权限 0640、属组 customapi） | `/usr/local/etc/customapi/customsd.env` |
| 创建数据/日志目录（0750，属主 customapi） | `/var/db/customapi{,/state}`、`/var/log/customapi` |
| 安装控制台 | `/usr/local/share/customapi/web` |
| 安装验收与自检脚本、mock 上游 | `/usr/local/share/customapi/`（`smoke-test.sh`、`intel-test.sh`、`verify-deployment.sh`、`mock-upstream.py`） |
| **校正运行时文件属主与权限**（修正被 root 改写过的账本） | `/var/db/customapi/{audit.jsonl,anchor.jsonl,catalogue.json}` |
| 写入 rc.conf 开关 | `sysrc` |
| 安装 newsyslog 轮转规则 | `/etc/newsyslog.conf.d/customapi.conf` |

### 2.3 卸载

```sh
sh /tmp/customapi-release/scripts/install.sh --uninstall
```

它会停服务、清 rc.conf 开关、删除二进制与 rc.d 脚本。**审计账、配置与日志会被保留**
（它们是证据与策略，不应被卸载动作删除）。需要彻底清除时手工删除
`/var/db/customapi`、`/usr/local/etc/customapi`、`/var/log/customapi`。

---

## 3. 配置

### 3.1 密钥（`/usr/local/etc/customapi/customsd.env`）

**配置文件中不放任何密钥。** 密钥只从该文件读取（`--env-file`），命名规则：

```sh
# 提供商凭据（推荐）
CUSTOMS_PROVIDER_<提供商名大写>_APIKEY=<上游密钥>                     # 单 key 提供商
CUSTOMS_PROVIDER_<提供商名大写>_KEY_<KEY名大写>_APIKEY=<上游密钥>      # 多 key 提供商，指定某个 key
CUSTOMS_PROVIDER_<提供商名大写>_BASEURL=<覆盖地址>

# 直连渠道凭据
CUSTOMS_CHANNEL_<渠道名大写>_APIKEY=<上游密钥>
CUSTOMS_CHANNEL_<渠道名大写>_BASEURL=<覆盖地址>

# 客户端令牌
CUSTOMS_USER_<用户名大写>_TOKEN=<用户令牌>

# 服务配置覆盖
CUSTOMS_SERVER_<字段>=<值>   # LISTEN / ADMIN_LISTEN / ADMIN_REMOTE / ADMIN_TOKEN
                            # AUDIT_PATH / METER_PATH / STATE_PATH / LOG_LEVEL / FAIL_MODE / ROUTE_STRATEGY
```

环境变量里不能出现的字符（`.`、`-` 等）一律在名字里变成下划线，所以以主机名命名的
提供商不需要改名：提供商 `naapi.cc` 的 `main` key 就是
`CUSTOMS_PROVIDER_NAAPI_CC_KEY_MAIN_APIKEY`。

示例：

```sh
CUSTOMS_PROVIDER_OPENAI_KEY_K1_APIKEY=sk-xxxxxxxx
CUSTOMS_PROVIDER_NEWAPI_KEY_MAIN_APIKEY=sk-yyyyyyyy
CUSTOMS_USER_ALICE_TOKEN=$(openssl rand -hex 32)
CUSTOMS_USER_ADMIN_TOKEN=$(openssl rand -hex 32)
CUSTOMS_USER_PROBE_TOKEN=$(openssl rand -hex 32)
```

> `keys[].name` 必须与 env 里的 `<KEY名>` 对得上（大写化后比较）。只写
> `CUSTOMS_PROVIDER_<P>_APIKEY` 只对"只有一个未命名 key"的提供商生效；多 key
> 提供商必须逐个 key 指定，否则那些 key 会**静默地没有凭据**，直到第一次调用才暴露。

权限必须是 `0640` 且属主为 `root:customapi`：

```sh
chown root:customapi /usr/local/etc/customapi/customsd.env
chmod 640 /usr/local/etc/customapi/customsd.env
```

> rc.d 会在启动前强制检查这两点（`stat -f '%Lp'` 必须是 600 或 640，并且确认服务
> 用户可读）。**故意如此**：一个服务用户读不到的密钥文件会让服务"启动了但无法认证"，
> 这比启动失败更难排查。

优先级：**配置文件 < 进程环境变量 < env 文件**。env 文件优先级最高，因为它是显式
指定的来源（加载器只是把它的键值追加到环境变量列表的末尾）。

### 3.1.1 三种上游协议

网关同时讲三种协议，客户端用哪一种是**客户端自己的选择**，不是配置项：

| 客户端协议 | 端点 | 上游要求 |
|---|---|---|
| OpenAI Chat Completions | `POST /v1/chat/completions` | 绝大多数中转站与厂商都支持 |
| OpenAI Responses | `POST /v1/responses` | 原生解析（独立的事件类型与状态机），不是转成 chat 再转发 |
| Anthropic Messages | `POST /v1/messages` | 原生解析，接受 `x-api-key` 与 `anthropic-version` |

选路顺序是 **先同协议、后翻译**：

1. 优先选一条**原生支持**该客户端协议的渠道（`supports` 里包含它，或 `protocol` 就是它）；
2. 没有直连渠道且 `routing.allow_translation` 为真时，才做跨协议转换；
3. 转换过的响应带 `X-Customs-Translated` 头（值是 `upstream->client`，例如
   `openai->anthropic`），审计记录里 `translated: true` 并同时记下 `protocol`
   （客户端用的）与 `upstream_protocol`（上游实际讲的）。

`supports` 的语义是**上游实际存在的接口面**，不是"允许翻译"的开关：

| 客户端协议 | `supports` 里有没有 | 会发生什么 |
|---|---|---|
| 与 `protocol` 相同 | — | 原样转发到该协议的端点 |
| 不同，但**在 `supports` 里** | 有 | **原样**发到那个接口面（例如 `protocol=openai` + `supports=[openai,responses]` 时，Responses 请求直接发 `/v1/responses`），不做任何转换 |
| 不同，且**不在 `supports` 里** | 没有 | 转换成 `protocol` 那种协议再发；转换后的回复会被**转回客户端协议**，并带 `X-Customs-Translated` 头 |

第二行是 `deploy/etc/customs.json` 模板里三个提供商的情况：它们同时提供 chat
completions 与 Responses 两种接口，所以 `supports: ["openai","responses"]` 让
`/v1/chat/completions` 与 `/v1/responses` 都**无损直连**，一个字节都不改写。

第三行才是真正的"翻译"：只有上游**没有**那个接口面时才会用到。所以 `/v1/messages`
会被转成 chat 发给这些提供商，客户端仍然收到 Messages 形状的回复。
（如果某个提供商其实也提供原生 Messages 接口，把它加进 `supports` 即可变成直连。）

**为什么默认"先同协议"**：翻译是有损的边界。流式事件可以映射，但工具调用、
思考字段、服务端状态（Responses 的 `previous_response_id`）在跨协议时会退化。
系统把它当"降级可用"，不当"等价"，所以 `prefer_same_protocol`（默认开）
把能直连的渠道排在需要转换的渠道之前，无论后者多便宜。

排错时先确认上游到底支持什么——`supports` 写错会导致本可以直连的请求被翻译。
管理 API 把这项回显为 `protocols`（`supports` 是配置里的名字）：

```sh
curl -s http://127.0.0.1:8081/admin/providers | python3 -m json.tool | grep -A4 '"protocols"'
```

### 3.2 主配置（`/usr/local/etc/customapi/customs.json`）

可执行 `customsctl config check --config …` 校验（`customsd --check` 等价）。
解析器**拒绝未知字段**，因此字段名写错会立刻报错而不是静默使用默认值；
文件顶部可用 `_comment` 写运维备注（支持字符串或字符串数组）。

#### 服务字段（`server`）

| 字段 | 模板默认 | 说明 |
|---|---|---|
| `listen` | `0.0.0.0:8080` | 用户面监听 |
| `admin_listen` | `127.0.0.1:8081` | 管理面监听（默认只给本机） |
| `admin_remote` | `false` | 是否允许非环回调用者访问 `/admin/*` |
| `admin_token` | 空 | 远程访问所需的 bearer 令牌（实际值放 env 文件） |
| `audit_path` | `/var/db/customapi/audit.jsonl` | 审计账（只追加） |
| `meter_path` | `/var/db/customapi/meter.jsonl` | 计量明细路径（当前仅保留配置项） |
| `state_path` | `/var/db/customapi/state.json` | 配额状态 |
| `price_path` | `/usr/local/etc/customapi/catalogue.json` | 价格目录 |
| `score_path` | `/var/db/customapi/scores.json` | 评分卡 |
| `upstream_timeout` | `300s` | 单次上游请求超时 |
| `fail_mode` | `closed` | 审计引擎故障时：`closed` 拒绝服务，`open` 放行 |
| `log_level` | `info` | `debug` / `info` / `warn` / `error` |
| `route_strategy` | `value` | `value` / `cheapest` / `best_score` / `weighted` / `fixed` |

> `fail_mode` 未配置时代码里的默认值是 `open`（与 Go 版一致），而模板写的是
> `closed`。**显式写出来**，不要依赖默认值——这两者的取舍是运维决定，不是实现细节。

#### 提供商（`providers`）——推荐的配置方式

一个 `provider` 是"一个上游厂商 + 它的若干 API key"。**每个启用的 key 都会展开成一个
独立可路由的渠道**，这是整个多 key 支持的全部机制：

* 只有一个 key 且未命名 → 渠道名 `<provider>`
* 有多个 key，或 key 有名字 → 渠道名 `<provider>/<key>`

因为展开后每个 key 就是一个渠道，**按 key 的故障转移不需要路由层知道"提供商"这个概念**：
某个 key 被限流或报错，它就是一条不健康的渠道，路由器自然会换下一条。

```json
{
  "name": "openai",
  "base_url": "https://api.openai.com",
  "protocol": "openai",
  "supports": ["openai", "responses"],
  "trust": "trusted",
  "weight": 2,
  "groups": ["premium"],
  "models": { "gpt-4o": "gpt-4o-2024-11-20" },
  "aliases": { "gpt-4-turbo": "gpt-4o" },
  "keys": [
    {
      "name": "k1",
      "weight": 2,
      "limits": { "rpm": 500, "tpm": 1000000, "monthly_budget": 100, "currency": "CNY" },
      "note": "主线路"
    },
    { "name": "k2", "weight": 1, "enabled": false }
  ],
  "discover": { "enabled": true, "exclude": ["*-audio-*"] }
}
```

| 字段 | 说明 |
|---|---|
| `protocol` | 与该提供商通信时优先使用的协议：`openai` / `responses` / `anthropic` |
| `supports` | 该提供商**实际支持**的全部协议。留空则等于只有 `protocol`。<br>它存在的意义是让跨协议路由是**刻意决定**的：只实现了 chat 接口的提供商，不能因为"都算 OpenAI 系"就塞给它一个 Responses 请求 |
| `models` | 规范模型名 → 该提供商自己的模型名。留空 `{}` 表示显式透传任意模型名；给出映射表则视为声明了目录，未列出的模型不会被路由到该提供商的渠道 |
| `aliases` | 额外的别名，让某个厂商的拼写能通过另一个名字暴露 |
| `trust` | `untrusted`（默认）或 `trusted`。**只放宽内容检查，不影响计量与审计** |
| `keys[].name` | key 的身份，会出现在审计记录里，便于回答"这笔账是哪个 key 花的" |
| `keys[].api_key` | **留空**，实际值由 env 文件提供（见 3.1） |
| `keys[].limits` | 运维**记录**的额度，前端只作为提示展示；网关不据此限流，因为真实额度只有厂商知道 |
| `groups` | 该提供商所属的路由分组，配合 `routing.groups` 使用 |
| `discover` | 模型发现策略，见下方"模型发现" |

`headers` / `timeout` / `weight` / `enabled` / `metadata` 与直连渠道含义相同。

#### 直连渠道（`channels`）——仍然支持

`channels` 没有被移除：不需要 key 分组的简单场景（一个上游一个 key）直接写
`channels` 更省事。两种写法可以**同时存在**，审计与路由看到的都是展开后的渠道列表。
`customsctl config check` 会把两者合并后的结果报出来。

```json
{
  "name": "legacy",
  "base_url": "http://10.0.0.9:8000",
  "protocol": "openai",
  "trust": "untrusted",
  "weight": 1,
  "enabled": true,
  "models": { "gpt-4o": "gpt-4o" },
  "headers": {},
  "timeout": "300s"
}
```

#### 故障转移：一个提供商不行就换下一个

这是多提供商配置的**全部意义**：只要还有一条能服务的渠道，请求就该成功。

```json
"routing": {
  "max_attempts": 3,
  "retry_on": ["5xx", "429", "timeout", "network", "quota"]
}
```

| 失败类别 | 含义 | 默认是否换渠道 |
|---|---|---|
| `5xx` | 上游服务端错误 | 是 |
| `429` | 限流 | 是 |
| `timeout` | 超时 | 是 |
| `network` | 连不上 / DNS / TLS 失败 | 是 |
| `quota` | **提供商自己的账户付不起这笔请求** | 是 |

`quota` 这一类值得单独说明。它通常以 **403** 出现，而 403 一般是终局错误（key 被封、
模型没权限），到处重试只会把同一个失败复制到每个提供商。所以只有当**响应体明说**
是提供商自己的账户问题时才归入 `quota`：

```
{"error":{"code":"insufficient_user_quota",
          "message":"预扣费额度扣费失败, 用户剩余额度: ¥0.017783,
                     需要预扣费额度: ¥0.086051"}}
```

识别是**窄**的、基于文本的（这些响应来自本项目不控制的中转软件，字段名各不相同），
认得 `insufficient_user_quota`、`insufficient_quota`、`insufficient balance`、
`exceeded your current quota`、`余额不足`、`预扣费额度失败` 等写法。认错的代价是浪费
一次尝试；漏认的代价是一个已经没钱的提供商永远不被跳过——后者才是运维真正会遇到的。

换成 `quota` 之后记录长这样，可以直接回答"这次是谁服务的、前面谁挂了"：

```
channel=healthy/main provider=healthy attempts=2 decision=allow
failover_note: failed over from "broke/main" to "healthy/main" after attempt 1
route_reason : strategy=fixed picked "broke/main" of 2 candidate(s) ...
```

> `provider` 与 `key` 会跟着渠道一起切换。早期版本只换了渠道名，于是记录里会出现
> "渠道是 healthy、提供商是 broke" 这种自相矛盾的行——偏偏在你要决定停掉哪家付费时
> 最需要它是对的。

`max_attempts` 数的是**上游调用次数**，不是提供商个数：每次尝试都可能计费，
所以它同时是成本上限。失败重试时会清掉本轮次的 usage/偏差，**审计记录描述的是
真正成功的那次尝试**，失败次数记在 `attempts` 与 `failover_note` 里。

#### 路由（`routing`）

```json
{
  "prefer_same_protocol": true,
  "allow_translation": true,
  "max_attempts": 3,
  "retry_on": ["5xx", "429", "timeout", "network"],
  "groups": [],
  "exclude_providers": []
}
```

| 字段 | 默认 | 说明 |
|---|---|---|
| `prefer_same_protocol` | `true` | 优先选原生讲客户端协议的那条渠道。翻译是有损的，所以它是退路而不是默认 |
| `allow_translation` | `true` | 是否允许把请求翻译成另一种协议。关掉后，没有任何候选渠道讲这个协议时**直接拒绝**而不是转换 |
| `max_attempts` | `3` | 一次请求最多尝试几条渠道。每次尝试都是一次**可能计费**的真实上游调用，所以这既是延迟控制也是成本控制 |
| `retry_on` | 五类全开 | 哪些失败类别值得换一条渠道再试：`5xx`、`429`、`timeout`、`network`、`quota`。留空即"五类全开"；认证失败、请求非法**不重试**，因为换渠道也一样会失败，只是多花钱 |
| `groups` | 空 | 只在属于这些分组的提供商内路由 |
| `exclude_providers` | 空 | 完全跳过这些提供商 |

#### 模型发现（`discovery`）

```json
{
  "enabled": true,
  "interval": "30m",
  "timeout": "20s",
  "cache_path": "/var/db/customapi/state/catalogue.json",
  "stale_after": "24h"
}
```

发现逻辑是**合并**而不是覆盖：对每个提供商请求它的模型列表端点（默认 `/v1/models`，
可用 `discover.path` 覆盖；`protocol=anthropic` 时改用 `x-api-key` 认证），
与配置里 `models` 声明的名字合并，**配置里声明的永远赢**（那是运维的明确意图），
两边都有则标记为 `configured`，只在发现结果里则标记为 `discovered`。
发现失败不会让网关停止服务，只是保留上一次的缓存并在 `/admin/models` 里报错。

`POST /admin/models/refresh` 可以立即触发一次（前端"刷新模型"按钮走的就是它）。

#### 用户（`users`）

```json
{
  "name": "alice",
  "token": "占位，实际由 env 文件提供",
  "quota": 50000000,
  "period": "monthly",
  "trust": "untrusted",
  "rate_limit": 2,
  "burst": 5,
  "max_concurrent": 4,
  "models": []
}
```

| 字段 | 说明 |
|---|---|
| `quota` | 配额单位；`0` 表示不限。1 单位 = 1e-6 计价货币（与 new-api 的 USD 刻度一致） |
| `period` | `none` / `daily` / `weekly` / `monthly`，到期自动重置**消耗**（终身累计不重置） |
| `models` | 允许的模型；支持 `gpt-*` 前缀通配；空数组表示全部允许 |
| `rate_limit` / `burst` | 令牌桶限速（请求/秒）与桶容量 |
| `max_concurrent` | 并发上限 |

#### 策略（`policy`）

```json
{
  "outbound": {
    "mode": "enforce",
    "block_keywords": [],
    "block_patterns": [],
    "deny_tools": ["run_command", "shell", "exec"],
    "allow_tools": [],
    "deny_path_patterns": [],
    "redact_patterns": []
  },
  "inbound": {
    "mode": "enforce",
    "tool_call_policy": "block",
    "injection_patterns": [],
    "secret_patterns": []
  },
  "metering": {
    "tolerance": 0.25,
    "min_absolute_delta": 8,
    "fingerprint_threshold": 0.35
  }
}
```

| 字段 | 说明 |
|---|---|
| `*.mode` | `enforce` 拦截 / `report` 只记录不拦截 |
| `outbound.deny_tools` | 拒绝的工具名（**精确匹配**） |
| `outbound.allow_tools` | **非空即切换为白名单模式**：只允许列出的工具，其余全部拒绝 |
| `deny_path_patterns` | 正则，匹配工具参数中的危险路径/命令 |
| `inbound.tool_call_policy` | 对**上游主动发起的**工具调用：`off` / `report` / `block`（默认）。注意即使渠道 `trusted`，此项为 `block` 时仍会拦截——执行远端下发的命令正是信任模型要防的事 |
| `metering.tolerance` | 相对偏差阈值（默认 0.25），见下方说明 |
| `metering.min_absolute_delta` | 绝对偏差下限（默认 8 token）。**必须与 tolerance 同时满足**才判为异常 |
| `metering.fingerprint_threshold` | JS 散度阈值，超过即判为"非同一模型" |

> 模板里还带一个 `fingerprint_sample_rate`（指纹采样率）。它目前只是配置字段，
> 判定与采样都不读它——改它不会有任何效果。

> **关于计量判据**：本地分词器是无词表估算器，Go 版实测误差在英文散文 +7%~+17%、
> 中文 +15%、代码 −29%、纯数字 −46%（Rust 版是同一定标算法的移植，尚未重新测量）。
> 因此异常判定要求**两条同时满足**：
>
> ```
> 相对偏差 > tolerance (0.25)   且   绝对偏差 ≥ min_absolute_delta (8 token)
> ```
>
> **绝对下限不是可选项。** 短回复上一个 token 的差异就是很大的相对偏差：
> 5 token 声明为 8 token 即 +60%。FreeBSD 验收测试实测发现，缺少该下限时一个完全
> 诚实的上游在 36 条跑分记录中被误报 6 次，渠道诚信分由 1.0 降至 0.58——而诚信分正是
> 路由用于判断渠道可信度的量。加入下限后同一批数据误报 0/36，而 20 倍虚报仍被抓住。
>
> 若你需要 5% 级别的严格核验，应给估算器接入真实 BPE 词表，**而不是把判据压到噪声以下**。

#### 价格（`prices`）

```json
{
  "channel": "newapi",
  "model": "gpt-4o",
  "price": {
    "input_per_m": 18.0,
    "output_per_m": 72.0,
    "currency": "CNY",
    "scheme": "multiplier",
    "completion_ratio": 4.0,
    "source": "manual-seed",
    "promotions": [
      { "kind": "off_peak", "multiplier": 0.5, "start_minute": 30, "end_minute": 480,
        "note": "00:30-08:00 半价" },
      { "kind": "volume", "multiplier": 0.8, "min_tokens": 1000000 },
      { "kind": "subscription", "monthly_fee": 100, "included_tokens": 5000000 }
    ]
  }
}
```

也可以用 `customsctl prices normalize` 从上游原始形态换算：

```sh
customsctl prices normalize --scheme multiplier --base 2 --model-ratio 1.5 \
  --group-ratio 0.8 --completion-ratio 3 --usd-cny 7.1
customsctl prices normalize --scheme per_request --per-request 0.5 \
  --typical-input 10000 --typical-output 2000
```

价格行的 `input_per_m` / `output_per_m` 为 0 表示"没有价格记录"，不是"免费"：
成本字段留空，`cheapest` 策略在这行上排不出名次，直到填入真实价格。

---

## 4. 服务管理

### 4.1 命令行（与 Go 版的差异，务必先读）

Rust 版用 `clap` 解析参数，**文档与脚本统一写双横线长选项**：`--config`、`--env-file`、
`--listen`、`--admin-listen`、`--log-level`、`--version`、`--check`、
`--anchor-interval`。这是与 Go 版最直观的差异：Go 的 `flag` 包习惯写单横线
（`-config`，它其实也接受 `--config`），Rust 侧把双横线定成规范写法。

为了让旧的 rc.d 行、cron 条目和运维肌肉记忆不至于立刻失效，三个二进制在解析前做了一次
**窄范围的改写**：只把"已知长选项"的单个横线补成两个（`-config` → `--config`），
其余一概不动。因此：

- `-config` / `-env-file` / `-log-level` 这类旧写法**仍然可用**；
- `-confg` 这种拼错的名字不会被改写，交给解析器报错（与 Go 一样会失败）；
- 短选项和负数原样透传：`-c` 是 `--config` 的**短别名**，`-n` 是 `audit tail --n` 的
  短别名，`-1` 不会被当成选项。

| 用途 | Rust 版（规范写法） | Go 版 |
|---|---|---|
| 配置文件 | `--config FILE`（短：`-c`） | `-config FILE` |
| 密钥文件 | `--env-file FILE` | `-env-file FILE` |
| 覆盖用户面监听 | `--listen ADDR` | `-listen ADDR` |
| 覆盖管理面监听 | `--admin-listen ADDR` | `-admin-listen ADDR` |
| 覆盖日志级别 | `--log-level LEVEL` | `-log-level LEVEL` |
| 打印构建标识 | `--version` | `-version` |
| 只校验配置后退出 | `--check` | `-check` |
| 锚点写入周期 | `--anchor-interval 5m`（默认 5m，`0` 关闭） | `-anchor-interval 5m` |

> 兼容层只是兼容层：新写的脚本、文档、rc.conf 里的 `customsd_extra_flags` 都请用双横线。
> 单横线写法在**未知选项**上和 Go 一样会失败，所以 `-config` 能用并不代表"单横线也行"。

```sh
customsd --config /usr/local/etc/customapi/customs.json \
        --env-file /usr/local/etc/customapi/customsd.env
customsd --version          # customapi 0.1.0 (commit …, built …, freebsd/amd64, rustc …)
customsd --check --config /usr/local/etc/customapi/customs.json
#   configuration OK: 3 channel(s), 3 user(s), 6 price row(s)

customsctl config check --config /usr/local/etc/customapi/customs.json
customsctl audit verify --config /usr/local/etc/customapi/customs.json
customsctl users grant --user alice --quota 50000000     # 默认 --admin 127.0.0.1:8081
```

`benchd` 的参数是 `--config` / `--env-file` / `--gateway` / `--token` / `--model` /
`--channel` / `--out` / `--log-level` / `--prices-url` / `--group` / `--outcomes`，
任务是**位置参数**：`run`（默认）/ `prices` / `fingerprint` / `bench` / `score`。
所以 rc.d 里的 `benchd ... run` 不是多余的一行，`run` 就是任务名。

日志级别取自配置（`server.log_level`）或 `--log-level`，**不读 `RUST_LOG`**：
服务由 rc.d 以固定环境启动，一个游离的环境变量不该改变服务的啰嗦程度。

### 4.2 rc.d 与 rc.conf

```sh
service customsd start | stop | restart | status
service benchd   start | stop | restart | status

tail -f /var/log/customapi/customsd.log
tail -f /var/log/customapi/benchd.log
```

两个服务都由 **`daemon(8)`** 托管：自动降权到 `customapi` 用户、写 pid 文件、重定向
日志、崩溃后延时重启。rc.conf 可调项：

```sh
customsd_enable="YES"
customsd_config="/usr/local/etc/customapi/customs.json"
customsd_service_user="customapi"
customsd_envfile="/usr/local/etc/customapi/customsd.env"
customsd_logdir="/var/log/customapi"
customsd_bin="/usr/local/sbin/customsd"
customsd_extra_flags=""          # 追加 CLI 参数，例如 "--log-level debug"

benchd_enable="YES"
benchd_config="/usr/local/etc/customapi/customs.json"
benchd_envfile="/usr/local/etc/customapi/customsd.env"
benchd_gateway="http://127.0.0.1:8080"
benchd_out="/var/db/customapi/state"
benchd_extra_flags=""
```

> `customsd_service_user` 这个名字是**刻意的**：`rc.subr` 会把 `${name}_user` 解释为
> "用 su(1) 切换用户"，从而绕过 `daemon(8)`，并把守护进程自己的参数传给网关二进制。
>
> `customsd` 的启动参数由 `/usr/local/libexec/customapi/customsd-wrapper` 组装：
> `daemon(8)` 只接受"自己的选项 + 子命令 + 子命令参数"这种形态，两段选项不能混在同一行，
> 所以把网关侧参数收进 wrapper。wrapper 接收 `-b`（二进制）、`-c`（配置）、
> `-e`（env 文件），其余参数原样透传。

进程行为（与 Go 版一致，排障时会用到）：

- 收到 `SIGINT` / `SIGTERM` 后停止接受新连接，**最多等 15 秒**让在途请求结束；
- 停机前**再写一次锚点**，保证最后几条记录也被钉进 `anchor.jsonl`（`--anchor-interval 0`
  时整条锚点链路关闭）；
- 每 60 秒做一次配额周期重置（`daily` / `weekly` / `monthly` 到期的那一下），
  不依赖是否有请求进来；
- 模型发现是后台任务，在监听器起来**之后**才启动：上游再慢也不会拖住启动；
- 两个监听器任一挂掉，进程退出（不要让一个"只能访问用户面"的半死状态长期存在）。

---

## 4.3 管理控制台（前端）

前端随发布一起安装到 `/usr/local/share/customapi/web`，由管理监听端口直接提供
（`customsd` 从磁盘读，不使用内嵌资源，因此干净检出也能构建；可用 `CUSTOMS_WEB_DIR`
指向别处）。它**不需要任何外部资源**：没有 CDN、没有外部字体、没有图表库。

```
http://127.0.0.1:8081/            # 概览
http://127.0.0.1:8081/providers   # 提供商 / 密钥 / 渠道健康度 / 模型发现
http://127.0.0.1:8081/models      # 合并后的模型目录
http://127.0.0.1:8081/routing     # 路由策略、协议支持、比价
http://127.0.0.1:8081/usage       # 按模型 / 渠道 / 用户聚合
http://127.0.0.1:8081/audit       # 审计账本、哈希链校验、单条详情
http://127.0.0.1:8081/users       # 配额发放、封禁、周期
http://127.0.0.1:8081/scores      # 性价比评分与指纹证据
http://127.0.0.1:8081/settings    # 配置摘要（不含密钥）
```

同一套页面也可以从 `/ui/` 前缀访问（两个入口指向同一个 SPA）。

### 远程访问（默认关闭）

管理面能封禁用户、改写配额、读取整本审计账，所以它的可达性是**两层收紧**的：

1. 默认只监听 `127.0.0.1`；
2. **即使把监听地址改成对外地址，处理器本身仍然拒绝非环回调用者**（HTTP 403）。
   这一层不看 `X-Forwarded-For`——那个头是调用方自己写的，信它等于允许任何人自称
   127.0.0.1。

要让别的机器访问，两步都要做：

```sh
# 1) customs.json
"admin_listen": "0.0.0.0:8081",
"admin_remote": true,

# 2) env 文件（令牌放这里，不要写进 customs.json）
CUSTOMS_SERVER_ADMIN_TOKEN=$(openssl rand -hex 32)
```

此后每个远程 `/admin/*` 调用都必须带 `Authorization: Bearer <token>`：

| 状态 | 含义 |
|---|---|
| `403 admin_forbidden` | 没开远程访问，换什么凭据都没用 |
| `401 admin_unauthorized` | 开了远程访问，但没给或给错了令牌（响应带 `WWW-Authenticate`） |
| `200` | 令牌正确 |

控制台对 401 的反应是弹出令牌输入框（令牌只存 `sessionStorage`，关标签页即失效），
也可以直接免手工打开：

```
http://<主机>:8081/#token=<server.admin_token>
```

令牌走 URL **fragment**，因此不会进入服务器日志，用完立即从地址栏抹掉。

### 外壳为什么不受门禁

控制台的**静态外壳**（HTML/JS/CSS）对未认证调用者可见，只有 `/admin/*` 数据接口受门禁。
这不是疏漏，而是必须的：如果外壳也被门禁挡住，浏览器就拿不到那个"用来输入令牌"的页面，
远程运维**永远无法登录**。外壳里没有配置、没有密钥、没有数据——每一个字节的实际内容
都来自受门禁的 `/admin/*`。

### 前端开发

```sh
cd web
npm ci
npm run dev            # 开发服务器 :5173，/admin 代理到 127.0.0.1:8081
npm run test           # 令牌处理逻辑的单元测试（Node 直接跑真实模块）
npm run typecheck
npm run build          # vue-tsc --noEmit && vite build，产物进 dist/
```

`deploy.sh stage` 会把 `web/dist` 打进发布包；如果它不存在，网关会提供一个说明页
而不是启动失败，因此**纯后端部署仍然可用**。

`scripts/mock-admin.mjs` 是一个假的 admin API，用于脱离网关开发前端：

```sh
node scripts/mock-admin.mjs                          # 127.0.0.1:8081
MOCK_ADMIN_TOKEN=secret node scripts/mock-admin.mjs  # 带令牌门禁，用来调试令牌流程
```

> 注意 `typescript` 被钉在 `~5.9.3`。npm 上的 `latest`（7.x）会让 `vue-tsc` 报错，
> 这不是本项目的问题，但会把构建卡住。

---

## 5. 运维日常

### 5.1 配额发放

```sh
customsctl users list
customsctl users grant --user alice --quota 50000000   # 设为指定值
customsctl users add   --user alice --quota 10000000   # 追加
customsctl users suspend --user alice
customsctl users resume  --user alice
```

`users` 系列默认访问 `http://127.0.0.1:8081`，管理面在别处时用 `--admin URL`。
也可以直接调管理 API（可接你自己已有的后台）：

```sh
curl -X POST http://127.0.0.1:8081/admin/users \
  -d '{"user":"alice","action":"grant","quota":50000000}'
```

### 5.2 审计账

```sh
customsctl audit verify --config /usr/local/etc/customapi/customs.json   # 完整性校验；被篡改则非零退出
customsctl audit tail   --config ... --n 20                # 最近 20 条
customsctl audit tail   --config ... --verdict over_billed # 只看被判虚报的
customsctl audit stats  --config ...                       # 汇总：token、上游声明、虚报比例、花费
```

#### 什么时候需要 `audit rehash`

早期版本的写入器把 `entry_hash` 字段（当时还是空串）也序列化进了被哈希的载荷，
而校验器是"去掉该字段再算哈希"，两边对不上：链条本身没有被改动，但当前代码无法验证它，
而网关**拒绝向一个自己验不了的账本追加**（这是有意的，见 `fail_mode`）。

```
$ customsctl audit verify --config ...
ledger:      /var/db/customapi/audit.jsonl
records:     128
head:        seq=128 hash=…
result:      FAILED - 1 violation(s)
  line 1      seq 1      entry_hash_mismatch  content hashes to … but entry_hash is …
```

违规类型（`kind`）：`seq_gap`、`prev_hash_mismatch`、`entry_hash_mismatch`、
`parse_error`、`anchor_mismatch`。

修法是重建整条链：

```sh
customsctl audit rehash --config ... --yes
```

它会：

1. 把原文件复制到 `audit.jsonl.pre-rehash`（**绝不删除证据**）；
2. 按每条记录自身的内容重算 `seq` / `prev_hash` / `entry_hash`，报告改了多少条；
3. 把 `anchor.jsonl` 移到 `anchor.jsonl.pre-rehash`——锚点钉的是**旧链的头**，
   重建后必然对不上，留在原地会被当成篡改证据而让网关拒绝启动；
4. **保留原文件的属主与权限**。

> 第 4 点不是细节：重建通过"临时文件 + rename"完成，如果不显式恢复属主，以 root 身份
> 执行的 `customsctl` 会把账本变成 root 所有，于是**下一次服务启动时报
> `permission denied`**——服务读不到自己的证据库。安装器每次安装也会主动校正
> `/var/db/customapi` 下这些运行时文件的属主与权限，所以升级能顺带修好这种状态。

这是**运维显式发起**的动作，永远不会由网关自动执行：自动重写证据库就等于没有证据库。

**每天至少跑一次 `audit verify` 并检查退出码**，最好放进 cron 或你的监控系统。

### 5.3 备份

必须备份的是**审计账与配额状态**：

```
/var/db/customapi/audit.jsonl      证据，只追加
/var/db/customapi/anchor.jsonl     链头锚点（与账本分开存放更有意义）
/var/db/customapi/state.json       配额消耗状态
/var/db/customapi/scores.json      评分卡（benchd 产出，丢失可由情报层重建）
/usr/local/etc/customapi/          配置与密钥
```

`anchor.jsonl` 若能放到**另一台机器或另一种存储**，防篡改能力会显著提升：
本机同时被攻陷时，攻击者可以同时改写账本与锚点，而异地锚点则做不到。

### 5.4 日志轮转

安装器已写入 `/etc/newsyslog.conf.d/customapi.conf`（7 份、每份 1000 kB、压缩）。
审计账**不做轮转**（它是证据），需要长期运行时请按季度归档：

```sh
# 归档并开启新账本（先停服务，归档后启动即自动续链）
service customsd stop
gzip -c /var/db/customapi/audit.jsonl > /archive/audit-2026Q1.jsonl.gz
: > /var/db/customapi/audit.jsonl
rm -f /var/db/customapi/anchor.jsonl
service customsd start
```

> 启动时会对空账本重新从 `genesis` 开始，这是正确行为。归档文件仍可用
> `customsctl audit verify --ledger /archive/audit-2026Q1.jsonl` 单独校验。

---

## 6. 验收测试

部署后**务必**在目标机运行：

```sh
sh /usr/local/share/customapi/smoke-test.sh     # 89 项端到端 / 16 组
sh /usr/local/share/customapi/intel-test.sh     # 13 项情报闭环
```

或从开发机：`make smoke`、`make intel`、`make e2e`（走 SSH，目标由 `CUSTOMAPI_TARGET`
指定，默认 `customapi-bsd`）。

`smoke-test.sh` 会启动**临时**网关实例（独立端口 18099–18109、独立账本，不触碰生产
配置与账本），并对它执行十六组攻击与正常场景：

| 组 | 内容 | 项数 |
|---|---|---|
| 1 | 透传与独立计量 | 6 |
| 2 | 上游虚报 usage 检出 | 3 |
| 3 | 上游注入提示词被扣留 | 4 |
| 4 | 上游恶意工具调用被扣留 | 3 |
| 5 | 违规输入在到达上游前拦截 | 3 |
| 6 | 配额扣减、429/403、拒绝仍记账 | 4 |
| 7 | 审计链：篡改/删除/重排/启动拒绝/账本被替换 | 8 |
| 8 | 流式：`[DONE]`、内容、计量、流式注入检测 | 5 |
| 9 | 密钥卫生：不进管理 API、不进账本、401 | 5 |
| 10 | 比价与模型目录 | 3 |
| 11 | 提供商多 key 展开、按 key 故障转移、模型发现 | 7 |
| 12 | 三种线协议 | 10 |
| 13 | 管理面可达性（环回/远程/令牌/伪造头） | 10 |
| 14 | 跨协议转换（请求与响应、流式） | 7 |
| 15 | 提供商之间的故障转移 | 5 |
| 16 | 一种协议进、任意协议出 | 6 |

合计 **89 项**。失败时会保留工作目录（账本、日志、mock 日志）供排查，并打印网关日志
尾部；全通过时打印 `ALL CHECKS PASSED`。

> 测试依赖 `python3`（mock 上游）与 `curl`；`jq` 可选（没有时脚本退到 python3 解析）。
> 最小系统上：`pkg install python3 curl jq`。脚本按 root 运行（`make smoke` 走
> `sudo -n sh`），开始时会检查 `${PREFIX:-/usr/local}` 下三个二进制是否已安装。

`intel-test.sh` 覆盖情报闭环的六段：取价目录与有效单价、按渠道的用量诚信、
L1 指纹探针、L2 跑分套件的分类覆盖、评分卡，以及评分反馈到路由。它用 `probe` 用户
打真实流量，因此会消耗配额（但不会花真钱——mock 上游是本地进程）。

---

## 6.1 部署自检（一条命令）

验收测试（`smoke-test.sh`、`intel-test.sh`）在私有端口上启动**临时实例**，证明的是
**代码**没问题；它们不检查"这台机器上装好的那一份"到底能不能用。
`verify-deployment.sh` 检查的正是后者，而且完全只读：所有探测都是 GET，不写入任何东西，
也不消耗配额。

```sh
sh /usr/local/share/customapi/verify-deployment.sh
```

它检查：配置能否校验、env 文件权限、用户面 `/healthz` 与 `/readyz` 是否应答、
未认证的 `/v1/models` 是否被拒、管理面是否可达且按 `admin_remote` 的设定正确门禁
（含伪造 `X-Forwarded-For`）、**账本属主是不是 root**（这是"服务读不到自己的证据库"
的直接原因）、审计链是否完整、控制台 HTML 与静态资源是否能取到。

```
=== results ===
passed: 13
failed: 0

DEPLOYMENT OK
```

通过数随控制台静态资源的条数与 `admin_remote` 的设定浮动（每个资源单独计一项），
不必把 13 当作固定值；判据是 `failed: 0` 与退出码。任何 `[FAIL]` 都附带一条可执行的
修复线索。

> 装了新版本之后记得 `service customsd restart`：安装器可能替换了 `customs.json`，
> 而**运行中的进程仍然持有旧配置**，于是会出现"配置检查通过、但端口不对"这种
> 看起来像 bug 的状态。

---

## 7. 上线前检查清单

- [ ] `customsd.env` 中所有 `replace-me` 占位符已替换为 `openssl rand -hex 32` 生成的强令牌
- [ ] `customsd.env` 权限为 `0640`、属主为 `root:customapi`
- [ ] `customsctl config check --config …` 通过
- [ ] `admin_listen` 默认 **仅监听本机**（`127.0.0.1:8081`）。要让别的机器访问控制台，
      必须同时设 `admin_remote: true` 与 `admin_token`（令牌放 env 文件），
      并且清楚这意味着 LAN 上任何人拿到令牌就能封禁用户、读取整本审计账；
      能用 SSH 隧道就不要开放端口。
- [ ] 控制台外壳（静态 bundle）对未认证调用者可见，这是**有意为之**：否则浏览器
      拿不到那个用来输入令牌的页面。外壳里没有配置、没有密钥、没有数据；
      所有 `/admin/*` 数据接口仍在门禁之后
- [ ] `listen` 按需绑定：模板默认 `0.0.0.0:8080`（用户面必须可被客户端访问）；
      若前面已有反向代理，可改回 `127.0.0.1:8080` 并只暴露代理
- [ ] 如启用了 pf/ipfw，确认已放通用户面端口
- [ ] `fail_mode` 已明确：`closed` 更安全（审计引擎故障则拒绝服务），`open` 更可用
- [ ] 入站/出站 `mode` 与你的风险偏好一致；先用 `report` 观察一段时间再切 `enforce`
- [ ] `tolerance` 与 `min_absolute_delta` 符合你对估算误差的接受度（默认 0.25 / 8，理由见 3.2）
- [ ] `trust` 白名单确实只包含你**真正**信任的对象
- [ ] 审计账与锚点的备份已配置，且锚点尽量异地
- [ ] `audit verify` 已加入日常巡检
- [ ] `smoke-test.sh` 与 `intel-test.sh` 均通过（Rust 版重建发布后需重跑）
- [ ] `verify-deployment.sh` 输出 `DEPLOYMENT OK`
- [ ] 已规划 `benchd` 的探测成本（它打的是真实流量，会消耗配额与真金白银）
- [ ] 记录 `customsd --version` 的输出，作为这次上线的构建标识（便于回溯）

### 关于 benchd 的成本

`benchd` 通过网关本身发起真实请求，因此：

- 会消耗**真实配额与真实费用**（可在账本中按 `probe` 用户单独统计）；
- 请为它配置独立的 `probe` 用户，便于把探测成本与用户花费分开；
- 默认节奏（沿用 Go 版）：取价 30 分钟、指纹 20 分钟、跑分 6 小时。若上游按量计费昂贵，
  调大周期或临时 `service benchd stop`。

---

## 8. 故障排查

| 现象 | 原因与处理 |
|---|---|
| `service customsd start` 后立即停止 | 看 `/var/log/customapi/customsd.log`。最常见：配置校验失败、端口被占、密钥文件不可读 |
| `customsd: audit: refusing to append to a ledger whose hash chain is broken (...)` | 审计账被篡改或损坏。**不要**直接启动；先用 `customsctl audit verify` 定位，必要时归档旧账本并开新账 |
| 启动报 `secrets file ... is mode 644` | `chmod 640` 且 `chown root:customapi` |
| 启动报 `not readable by customapi` | 同上；服务降权后才读该文件 |
| 所有请求 502 `upstream_unavailable` | 渠道 `base_url` 不可达。确认上游/前置网关在跑，或先用 mock 上游验证链路 |
| 请求 403 `content_policy_violation` | 出站规则命中。查 `customsctl audit tail` 看具体 `rule` |
| 响应 502 `upstream_content_blocked` | 入站规则命中，上游回复被扣留。查审计里的 `inbound_findings` |
| 请求 429 `insufficient_quota` | 配额耗尽。`customsctl users grant` 发放 |
| 请求 503 `no_channel` | 没有可用渠道：模型不在任何渠道的目录里、渠道全被排除、或 `allow_translation=false` 且无同协议渠道 |
| 大量 `over_billed` 告警 | 上游确实虚报（该渠道已被自动降权），或 `tolerance` 对你这批内容偏严。先看 `audit stats` 的偏差分布再决定 |
| 流式响应变成整段返回 | 该渠道不可信且入站为 `enforce`，系统选择了 `buffered` 模式（响应头 `X-Customs-Stream-Mode: buffered`）。这是"命中即不泄露"的代价；如接受风险可把该渠道设为 `trusted` 或把入站改为 `report` |
| 日志里出现 `unexpected argument` / `unknown option` | 选项名拼错（例如 `-confg`），或用了不在已知长选项列表里的单横线写法。改用 `--config` 这类规范写法，见 4.1 |
| `benchd` 报 `no probe credential` | 需要一个名为 `probe`（或以 `-probe` 结尾）的用户 |
| 控制台只有一张说明页 | 发布包里没有 `web/`。在开发机 `cd web && npm ci && npm run build` 后重新 `stage`/`install` |
