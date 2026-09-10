# Custom-API 设计备忘（讨论固化）

> 日期：2026-09-10
> 来源：架构讨论结论（双海关边界、流式堵点、计费权威、数据流走查）
> 状态：本文档为讨论结论的固化稿，与 `PROJECT_PLAN.md`（v0.2 双海关版）并行；计划文档待决策项清空后再行定稿。

---

## 一、架构定稿（双海关修订版）

```text
                            管理网络(本机/VPN/Tailscale)
                                    │
                            ┌───────▼───────┐
                            │ new-api 管理面  │
                            └───────────────┘
                                    │(管理接口)
用户 ──► 海关 B ──► new-api ──► 海关 A-1 ──► 上游 1
(chatbox/        │          ├──► 海关 A-2 ──► 上游 2
 codex)          │          └──► 海关 A-3 ──► 上游 3
                 │
                 └──► 审计事件/指标 ──► 独立审计库
```

核心原则：

1. **B 是用户入口策略层**，不负责上游选择、不接触上游 key。
2. **new-api 是唯一的用户配额与基础路由权威**（认证、预扣、结算、退款、渠道重试）。
3. **A 按渠道独立入口**（每渠道一个 A 或每渠道一个可验证身份的内部路由），不搞公共 A 猜渠道。
4. **A/B 都不重复扣用户额度**：A 记上游成本账，new-api 记用户配额账，离线对账。
5. **上游原始响应审计在 A，最终用户响应审计在 B**，两者可能不同（new-api 会转换）。
6. **流式安全策略 = 检测后断流**，不承诺完整过滤/撤回。
7. **管理面不经过用户入口**（B 只放行 API 路径）。
8. **动态性价比路由 = 控制面调 new-api 渠道权重**，A 不在数据面临时改路由；A 可拒绝当前上游/返回结构化错误，重试仍由 new-api 决定。

---

## 二、组件职责边界

| 项目 | 海关 B | new-api | 海关 A |
|---|---|---|---|
| 用户输入审计 | **是（主职责）** | 否 | 否 |
| 用户限速/滥用检测 | **是** | 部分（配额） | 否 |
| 危险 tool 预检 | **是** | 否 | 否 |
| 用户认证/配额/结算 | 否（只透传） | **是（唯一权威）** | 否 |
| 模型映射/协议适配 | 否 | **是** | 否 |
| 渠道选择/重试 | 否 | **是** | 否（只上报异常） |
| 上游 key/地址绑定 | 否 | 持有上游 key | **是（出口执行）** |
| 上游 usage 对账 | 否 | 解析 usage 计费 | **是（重算比对）** |
| 上游响应注入检测 | 否 | 否 | **是** |
| 模型指纹采样 | 否 | 否 | **是** |
| request ID 起源 | **是（入口生成）** | 透传+日志 | 透传+审计 |
| 审计对象 | 用户原始请求/最终响应 | 自身日志 | 上游原始请求/原始响应 |
| 是否可公网暴露 | 视部署（唯一入口） | **否** | **否** |

---

## 三、请求数据流走查：chatbox 输入「你好」

### 3.1 非流式

```
1. chatbox 构造 POST https://<海关B>/v1/chat/completions
   body: {"model":"gpt-x","messages":[{"role":"user","content":"你好"}],"stream":false}
   header: Authorization: Bearer sk-用户token

2. 海关 B
   - 生成/接受 request_id；剥离用户伪造的内部 header（X-Custom-*）
   - 输入审计：「你好」分类 clean，通过
   - 限速/并发检查通过；无 tools，跳过 tool 预检
   - 附加受控 header：request_id、user_hash=sha256(token)、policy=allow、policy_version
   - 转发到 new-api 内网端口（B 是唯一允许访问者）

3. new-api
   - 认证 token → 用户/分组/配额
   - 解析统一模型名 → 映射上游模型名
   - 估算输入 token → 预扣额度（PreConsume）
   - 渠道选择（权重/优先级/健康）
   - adaptor 构造上游请求（替换上游 key、可能调整字段）
   - 转发到该渠道绑定的海关 A 入口

4. 海关 A
   - 验证内部签名 → 确知渠道身份（channel_id）
   - 记录 attempt_id，绑定上游地址，转发真实上游

5. 上游返回完整 JSON（content + usage{prompt_tokens, completion_tokens}）

6. 海关 A
   - 本地 token 重算 vs 上游 usage → 一致/偏差事件
   - 响应注入检测（规则+分片模式）→ clean
   - 指纹采样（按采样率，不阻塞）
   - 审计事件：upstream_attempt / upstream_usage_received /
     upstream_usage_recalculated / response_policy=clean
   - 原样转发给 new-api

7. new-api
   - 解析 usage → 计算最终配额 → 结算（预扣 vs 实际，多退少补）
   - 转换为用户协议响应格式
   - 记录请求日志（用户/模型/渠道/token/耗时）

8. 海关 B
   - 出站检查（可选轻规则）
   - 响应附 request_id，返回 chatbox

9. chatbox 显示回复
```

### 3.2 流式（stream: true）

```text
上游 ──chunk1──► A ──chunk1──► new-api ──chunk1──► B ──chunk1──► chatbox
      ──chunk2──►    ──chunk2──►           ──chunk2──►    ──chunk2──►
      ──chunkN──►    ──chunkN──►           ──chunkN──►    ──chunkN──►
      （usage 常在最后 chunk）
```

与 3.1 的差异：

- B **不等待完整响应**，chunk 级透传（否则首 token 延迟 = 完整响应延迟）；
- A 分片缓冲模式检测：发现恶意内容时**断流 + 审计事件**，已发出的 chunk 无法撤回；
- new-api 边收边转发；usage 缺失时自行估算或以上游最终 usage 为准；
- 结算发生在流结束时（收到最终 usage/DONE）；
- 四层任一层断开都会向上/下游传播，按实际已产生 token 结算。

### 3.3 取消与异常

```text
用户关窗口 → B 断开 → new-api 断开 → A 断开 → 上游断开
```

必须区分的状态：

```text
accepted / forwarding / first_token / streaming /
client_cancelled / upstream_cancelled / upstream_failed /
completed / billing_pending / billing_settled / billing_refunded /
audit_pending
```

铁律：**上游已产生的 token 必须按实际结算，不得因用户取消而全额退回**；
new-api 重试产生的每次上游尝试都要单独计量（attempt_id），B 不得把重试当新用户请求重复预扣。

### 3.4 codex（Anthropic 协议）

请求走 `/v1/messages`（messages 结构不同、SSE event 类型不同），链路相同：

```text
codex → B → new-api（Anthropic 端点/adaptor）→ A → 上游
```

B/A 的解析器按协议分支；M1 只覆盖 OpenAI-compatible，Anthropic 解析器接口预留。

---

## 四、堵点清单（按严重程度）

| 级别 | 堵点 | 结论/待验证 |
|---|---|---|
| P0 | new-api 是否支持**每渠道独立 A 入口** | 待实验：BaseURL 指向不同 A、路径不重写、重试仍进正确 A、流式长连接正常 |
| P0 | new-api adaptor 是否会吞掉上游原始响应 | A 审计上游原文，B 审计用户最终响应，二者不一致属正常，需记录转换差异 |
| P1 | 流式无法「过滤后发送」 | 检测后断流；已发送不可撤回；自然语言诱导不承诺 100% |
| P1 | 取消/重试的计费一致性 | M1 必测：首 token 前取消、中途取消、重试、部分 SSE 后 5xx、usage chunk 丢失 |
| P1 | 管理面混入用户入口 | B 只放行 API 路径；管理面走管理网络 |
| P2 | 内部 header 伪造 | B 剥离客户端 X-Custom-*；A 验签名防绕过/重放 |

---

## 五、内部接口约定（草案）

### 5.1 ID 体系

```text
request_id          用户请求级，全链路不变（B 生成）
attempt_id          每次上游尝试唯一（new-api 或 A 生成）
upstream_request_id 真正发给上游的请求 ID
```

### 5.2 B → new-api

- 保留用户原始 API 语义；附加受控 header：
  `X-Custom-Request-Id`、`X-Custom-User-Hash`、`X-Custom-Policy-Decision`、`X-Custom-Policy-Version`
- 客户端同名 header 一律剥离；new-api 只信任来自 B 的网络（mTLS/内网签名）
- `Policy-Decision` 不替代 new-api 认证与配额

### 5.3 new-api → A

- 附加签名 header：request_id、attempt_id、channel_id、nominal_model、upstream_model、signature
- 签名覆盖：method + path + body hash + ids + channel_id + timestamp + nonce
- 每渠道独立 A 入口时签名仍保留（防绕过 new-api 直连 A、防渠道伪装与重放）

### 5.4 计费权威划分

| 账 | 权威 | 记录内容 |
|---|---|---|
| 用户配额账 | new-api | 预扣/结算/退款/余额 |
| 上游成本账 | 海关 A | 上游报告 usage、本地重算、差异、耗时、状态 |
| 审计账 | A + B | 同一 request_id 下的事件流（accept/attempt/usage/policy/refund） |

离线对账任务发现异常 → 审计事件 + 渠道降权/告警，**不在热路径重复扣费**。

---

## 六、下一步（M0 实验清单）

1. 每渠道独立 A 入口可行性（P0 实验）
2. SSE 经 B→new-api→A 三层原样通过
3. request/attempt ID 全链路传播与日志串联
4. new-api 适配层对请求/响应的改动范围实测（确定 A/B 各自审计对象的确切形态）

*本文档为讨论固化 v0.1，随实验结论更新。*
