<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'

import { api } from '../api'
import { useAsync } from '../async'
import Badge from '../components/Badge.vue'
import Modal from '../components/Modal.vue'
import Panel from '../components/Panel.vue'
import StateBlock from '../components/StateBlock.vue'
import { compact, dash, millis, money, num, pct, shortHash, signedPct, ts, tsShort } from '../format'
import { decision as decisionLabel, protocolName, protocolTone, severity, verdict as verdictLabel } from '../labels'
import type { AuditQuery, AuditRecord, AuditVerifyResult } from '../types'

const PAGE_SIZES = [25, 50, 100, 200]

const filters = ref({
  actor: '',
  channel: '',
  model: '',
  decision: '',
  verdict: '',
  limit: 50,
  include_samples: false,
})

const offset = ref(0)
const audit = useAsync(
  () =>
    api.audit({
      actor: filters.value.actor.trim(),
      channel: filters.value.channel.trim(),
      model: filters.value.model.trim(),
      decision: filters.value.decision,
      verdict: filters.value.verdict,
      limit: filters.value.limit,
      offset: offset.value,
      include_samples: filters.value.include_samples,
    } satisfies AuditQuery),
  { immediate: true, keepOnError: true },
)

const selected = ref<AuditRecord | null>(null)
const verifyResult = ref<AuditVerifyResult | null>(null)
const verifyError = ref('')
const verifying = ref(false)

const route = useRoute()
const router = useRouter()

function closeDetail(): void {
  selected.value = null
  if (route.query.seq !== undefined) {
    const query = { ...route.query }
    delete query.seq
    void router.replace({ query })
  }
}

const records = computed(() => audit.data.value?.records ?? [])
const total = computed(() => audit.data.value?.total ?? 0)
const pageEnd = computed(() => Math.min(offset.value + filters.value.limit, total.value))

/**
 * `/audit?seq=123` opens that record's detail directly, so an operator can
 * share a link to a specific exchange. A seq outside the current page is
 * ignored rather than silently paginating to it.
 */
watch(
  () => [route.query.seq, records.value] as const,
  () => {
    const raw = route.query.seq
    const wanted = Number(Array.isArray(raw) ? raw[0] : raw)
    if (!Number.isFinite(wanted) || wanted <= 0) return
    const hit = records.value.find((r) => r.seq === wanted)
    if (hit) selected.value = hit
  },
  { immediate: true },
)

const DECISIONS = [
  { value: '', label: '全部' },
  { value: 'allow', label: '放行' },
  { value: 'report', label: '仅记录' },
  { value: 'block_user', label: '拦截用户' },
  { value: 'block_upstream', label: '拦截上游' },
  { value: 'upstream_error', label: '上游错误' },
]

const VERDICTS = [
  { value: '', label: '全部' },
  { value: 'consistent', label: '计量一致' },
  { value: 'over_billed', label: '上游超收' },
  { value: 'under_billed', label: '上游少报' },
  { value: 'not_reported', label: '未上报' },
  { value: 'not_verifiable', label: '无法核验' },
]

function search(): void {
  offset.value = 0
  void audit.load()
}

function nextPage(): void {
  if (offset.value + filters.value.limit >= total.value) return
  offset.value += filters.value.limit
  void audit.load()
}

function prevPage(): void {
  offset.value = Math.max(0, offset.value - filters.value.limit)
  void audit.load()
}

function reset(): void {
  filters.value = {
    actor: '',
    channel: '',
    model: '',
    decision: '',
    verdict: '',
    limit: 50,
    include_samples: false,
  }
  search()
}

async function verify(): Promise<void> {
  verifying.value = true
  verifyError.value = ''
  try {
    verifyResult.value = await api.verifyAudit()
  } catch (err) {
    verifyResult.value = null
    verifyError.value = err instanceof Error ? err.message : String(err)
  } finally {
    verifying.value = false
  }
}

/** Tokens as reported locally vs by the upstream, for the detail view. */
function delta(local: number | undefined, upstream: number | undefined): string {
  if (upstream === undefined) return '—'
  const diff = (local ?? 0) - upstream
  const sign = diff > 0 ? '+' : ''
  return `${sign}${num(diff)}`
}

const findingsOf = computed(() => {
  const rec = selected.value
  if (rec === null) return { outbound: [], inbound: [] }
  return { outbound: rec.outbound_findings ?? [], inbound: rec.inbound_findings ?? [] }
})
</script>

<template>
  <Panel title="审计账本" hint="GET /admin/audit，按时间倒序分页">
    <template #actions>
      <div class="btn-row">
        <button class="btn" type="button" :disabled="verifying" @click="verify">
          <span v-if="verifying" class="spinner" />{{ verifying ? '校验中…' : '校验审计链' }}
        </button>
        <button class="btn btn-primary" type="button" @click="search">查询</button>
      </div>
    </template>

    <div class="filters">
      <div class="field">
        <label for="f-actor">调用方</label>
        <input id="f-actor" v-model="filters.actor" type="text" placeholder="用户" style="width: 120px" />
      </div>
      <div class="field">
        <label for="f-channel">渠道</label>
        <input
          id="f-channel"
          v-model="filters.channel"
          type="text"
          placeholder="provider/key"
          style="width: 150px"
        />
      </div>
      <div class="field">
        <label for="f-model">模型</label>
        <input id="f-model" v-model="filters.model" type="text" placeholder="gpt-4o" style="width: 140px" />
      </div>
      <div class="field">
        <label for="f-decision">判定</label>
        <select id="f-decision" v-model="filters.decision">
          <option v-for="d in DECISIONS" :key="d.value" :value="d.value">{{ d.label }}</option>
        </select>
      </div>
      <div class="field">
        <label for="f-verdict">计量判定</label>
        <select id="f-verdict" v-model="filters.verdict">
          <option v-for="v in VERDICTS" :key="v.value" :value="v.value">{{ v.label }}</option>
        </select>
      </div>
      <div class="field">
        <label for="f-limit">每页</label>
        <select id="f-limit" v-model.number="filters.limit">
          <option v-for="n in PAGE_SIZES" :key="n" :value="n">{{ n }}</option>
        </select>
      </div>
      <div class="field">
        <label for="f-samples">样本</label>
        <label class="small muted" style="display: flex; gap: 6px; align-items: center">
          <input id="f-samples" v-model="filters.include_samples" type="checkbox" />
          包含样本
        </label>
      </div>
      <button class="btn" type="button" @click="reset">重置</button>
    </div>
    <div v-if="filters.include_samples" class="small" style="color: var(--warn); margin-top: 6px">
      注意：包含样本会把被放行请求的脱敏正文一并取回，仅在排查问题时使用。
    </div>
  </Panel>

  <!-- Chain verification result: shown inline, violations first. -->
  <div v-if="verifyError" class="banner danger">
    <span class="banner-icon">✕</span>
    <div>
      <div class="banner-title">无法完成审计链校验</div>
      <div class="banner-text">{{ verifyError }}</div>
    </div>
  </div>
  <template v-else-if="verifyResult">
    <div class="banner" :class="verifyResult.ok ? 'ok' : 'danger'">
      <span class="banner-icon">{{ verifyResult.ok ? '✔' : '✕' }}</span>
      <div>
        <div class="banner-title">
          审计链{{ verifyResult.ok ? '完整' : '已损坏' }} · {{ num(verifyResult.records) }} 条记录 ·
          链头 #{{ verifyResult.head_seq }}
        </div>
        <div class="banner-text">
          链头哈希 <span class="mono">{{ shortHash(verifyResult.head_hash, 12) }}</span> ·
          已核对锚点 {{ num(verifyResult.anchors_checked) }} 个
          <span v-if="verifyResult.anchors_ignored">
            ，忽略 {{ num(verifyResult.anchors_ignored) }} 个
          </span>
        </div>
      </div>
      <span class="spacer" />
      <Badge
        :text="`${num(verifyResult.violations?.length ?? 0)} 项违规`"
        :tone="verifyResult.ok ? 'ok' : 'danger'"
      />
    </div>
    <div v-if="!verifyResult.ok" class="card">
      <header class="card-head"><h3>违规明细</h3></header>
      <div class="card-body tight">
        <div class="table-wrap">
          <table class="data compact">
            <thead>
              <tr>
                <th>类型</th>
                <th>行号</th>
                <th>序号</th>
                <th>详情</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="(v, i) in verifyResult.violations ?? []" :key="i">
                <td><Badge :text="v.kind" tone="danger" /></td>
                <td class="mono">{{ v.line ?? '—' }}</td>
                <td class="mono">{{ v.seq ?? '—' }}</td>
                <td class="wrap">{{ dash(v.detail) }}</td>
              </tr>
              <tr v-if="(verifyResult.violations?.length ?? 0) === 0">
                <td colspan="4" class="muted">校验返回 ok=false，但没有违规明细。</td>
              </tr>
            </tbody>
          </table>
        </div>
      </div>
    </div>
  </template>

  <Panel :title="`审计记录（${num(total)} 条）`" hint="点击任意一行查看详情">
    <template #actions>
      <div class="btn-row">
        <span class="small muted">
          {{ total === 0 ? '0' : `${num(offset + 1)}–${num(pageEnd)}` }} / {{ num(total) }}
        </span>
        <button class="btn btn-sm" type="button" :disabled="offset === 0" @click="prevPage">
          上一页
        </button>
        <button
          class="btn btn-sm"
          type="button"
          :disabled="offset + filters.limit >= total"
          @click="nextPage"
        >
          下一页
        </button>
        <button class="btn btn-sm" type="button" @click="audit.load()">刷新</button>
      </div>
    </template>

    <StateBlock
      :loading="audit.loading.value && audit.data.value === null"
      :error="audit.error.value"
      :empty="audit.data.value !== null && records.length === 0"
      empty-text="没有符合条件的审计记录。"
      @retry="audit.load()"
    >
      <div class="table-wrap">
        <table class="data compact">
          <thead>
            <tr>
              <th>序号</th>
              <th>时间</th>
              <th>调用方</th>
              <th>渠道</th>
              <th>模型 / 协议</th>
              <th>判定</th>
              <th>tokens 本地 → 上游</th>
              <th>计量</th>
              <th>成本</th>
              <th>延迟</th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="rec in records"
              :key="rec.seq"
              class="expandable"
              @click="selected = rec"
            >
              <td class="mono">{{ rec.seq }}</td>
              <td class="small nowrap" :title="ts(rec.time)">{{ tsShort(rec.time) }}</td>
              <td class="mono">{{ dash(rec.actor) }}</td>
              <td class="mono">{{ dash(rec.channel) }}</td>
              <td>
                <div class="mono">{{ dash(rec.model) }}</div>
                <div class="tag-list" style="margin-top: 2px">
                  <Badge
                    :text="protocolName(rec.protocol)"
                    :tone="protocolTone(rec.protocol)"
                    :title="rec.upstream_protocol ? `上游协议：${rec.upstream_protocol}` : ''"
                  />
                  <Badge v-if="rec.translated" text="已转换" tone="info" />
                  <Badge v-if="rec.server_state" text="服务端状态" tone="warn" />
                  <Badge v-if="rec.stream" text="流式" tone="muted" />
                </div>
              </td>
              <td>
                <Badge
                  :text="decisionLabel(rec.decision).text"
                  :tone="decisionLabel(rec.decision).tone"
                  :title="`${String(rec.decision)}${rec.status_code ? ` · HTTP ${rec.status_code}` : ''}`"
                />
              </td>
              <td class="mono">
                {{ compact(rec.usage.total_tokens) }}
                <span class="faint">
                  →
                  {{ rec.usage.upstream_reported ? compact(rec.usage.upstream_total_tokens) : '未上报' }}
                </span>
              </td>
              <td>
                <Badge
                  :text="verdictLabel(rec.usage.verdict).text"
                  :tone="verdictLabel(rec.usage.verdict).tone"
                  :title="rec.usage.verdict"
                />
              </td>
              <td class="mono">{{ money(rec.cost?.total_cost, rec.cost?.currency) }}</td>
              <td class="mono">{{ millis(rec.latency_ms?.total_ms) }}</td>
            </tr>
          </tbody>
        </table>
      </div>
    </StateBlock>
  </Panel>

  <Modal
    :open="selected !== null"
    :title="selected ? `审计记录 #${selected.seq}` : ''"
    wide
    @close="closeDetail"
  >
    <template v-if="selected">
      <div class="tag-list">
        <Badge
          :text="decisionLabel(selected.decision).text"
          :tone="decisionLabel(selected.decision).tone"
        />
        <Badge
          :text="verdictLabel(selected.usage.verdict).text"
          :tone="verdictLabel(selected.usage.verdict).tone"
        />
        <Badge v-if="selected.translated" text="已转换协议" tone="info" />
        <Badge v-if="selected.server_state" text="服务端状态请求" tone="warn" />
        <Badge v-if="selected.stream" text="流式" tone="muted" />
        <Badge v-if="selected.error" text="有错误" tone="danger" />
      </div>

      <div>
        <div class="section-title">基本信息</div>
        <dl class="kv">
          <dt>记录 ID</dt>
          <dd class="mono break">{{ selected.id }}</dd>
          <dt>开始时间</dt>
          <dd class="mono">{{ ts(selected.time) }}</dd>
          <dt>结束时间</dt>
          <dd class="mono">{{ ts(selected.end_time) }}</dd>
          <dt>调用方</dt>
          <dd class="mono">{{ dash(selected.actor) }}</dd>
          <dt>渠道 / 提供商 / 密钥</dt>
          <dd class="mono">
            {{ dash(selected.channel) }} / {{ dash(selected.provider) }} / {{ dash(selected.key) }}
          </dd>
          <dt>模型</dt>
          <dd class="mono">
            {{ dash(selected.model) }}
            <span v-if="selected.upstream_model" class="faint">
              → 上游 {{ selected.upstream_model }}
            </span>
          </dd>
          <dt>协议</dt>
          <dd>
            <Badge :text="protocolName(selected.protocol)" :tone="protocolTone(selected.protocol)" />
            <span v-if="selected.upstream_protocol" class="small muted">
              → 上游 {{ protocolName(selected.upstream_protocol) }}
            </span>
          </dd>
          <dt>入口</dt>
          <dd class="mono">{{ dash(selected.endpoint) }}</dd>
          <dt>客户端 IP</dt>
          <dd class="mono">{{ dash(selected.client_ip) }}</dd>
          <dt>状态码</dt>
          <dd class="mono">{{ selected.status_code ?? '—' }}</dd>
          <dt>尝试次数</dt>
          <dd class="mono">{{ num(selected.attempts) }}</dd>
          <dt>配额余额</dt>
          <dd class="mono">{{ selected.quota_after === undefined ? '—' : num(selected.quota_after) }}</dd>
        </dl>
      </div>

      <div v-if="selected.error" class="banner danger">
        <span class="banner-icon">✕</span>
        <div>
          <div class="banner-title">错误</div>
          <div class="banner-text break">{{ selected.error }}</div>
        </div>
      </div>

      <div>
        <div class="section-title">命中规则</div>
        <div class="table-wrap">
          <table class="data compact">
            <thead>
              <tr>
                <th>方向</th>
                <th>规则</th>
                <th>级别</th>
                <th>字段</th>
                <th>摘录哈希</th>
                <th>可信豁免</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="(f, i) in findingsOf.outbound" :key="`o${i}`">
                <td><Badge text="出口" tone="accent" /></td>
                <td class="mono">{{ f.rule }}</td>
                <td>
                  <Badge :text="severity(f.severity).text" :tone="severity(f.severity).tone" />
                </td>
                <td class="mono">{{ dash(f.field) }}</td>
                <td class="mono">{{ shortHash(f.excerpt_hash, 6) }}</td>
                <td>{{ f.trust_exempt ? '是' : '否' }}</td>
              </tr>
              <tr v-for="(f, i) in findingsOf.inbound" :key="`i${i}`">
                <td><Badge text="入口" tone="info" /></td>
                <td class="mono">{{ f.rule }}</td>
                <td>
                  <Badge :text="severity(f.severity).text" :tone="severity(f.severity).tone" />
                </td>
                <td class="mono">{{ dash(f.field) }}</td>
                <td class="mono">{{ shortHash(f.excerpt_hash, 6) }}</td>
                <td>{{ f.trust_exempt ? '是' : '否' }}</td>
              </tr>
              <tr v-if="findingsOf.outbound.length + findingsOf.inbound.length === 0">
                <td colspan="6" class="muted">没有命中任何规则。</td>
              </tr>
            </tbody>
          </table>
        </div>
      </div>

      <div>
        <div class="section-title">用量对比</div>
        <div class="table-wrap">
          <table class="data compact">
            <thead>
              <tr>
                <th>项目</th>
                <th>本地统计</th>
                <th>上游上报</th>
                <th>差值</th>
                <th>偏差</th>
              </tr>
            </thead>
            <tbody>
              <tr>
                <td>输入 tokens</td>
                <td class="mono">{{ num(selected.usage.prompt_tokens) }}</td>
                <td class="mono">{{ num(selected.usage.upstream_prompt_tokens) }}</td>
                <td class="mono">{{ delta(selected.usage.prompt_tokens, selected.usage.upstream_prompt_tokens) }}</td>
                <td class="mono">{{ signedPct(selected.usage.prompt_delta) }}</td>
              </tr>
              <tr>
                <td>输出 tokens</td>
                <td class="mono">{{ num(selected.usage.completion_tokens) }}</td>
                <td class="mono">{{ num(selected.usage.upstream_completion_tokens) }}</td>
                <td class="mono">
                  {{ delta(selected.usage.completion_tokens, selected.usage.upstream_completion_tokens) }}
                </td>
                <td class="mono">{{ signedPct(selected.usage.completion_delta) }}</td>
              </tr>
              <tr>
                <td>合计 tokens</td>
                <td class="mono">{{ num(selected.usage.total_tokens) }}</td>
                <td class="mono">{{ num(selected.usage.upstream_total_tokens) }}</td>
                <td class="mono">{{ delta(selected.usage.total_tokens, selected.usage.upstream_total_tokens) }}</td>
                <td class="muted">—</td>
              </tr>
            </tbody>
          </table>
        </div>
        <dl class="kv" style="margin-top: 8px">
          <dt>判定</dt>
          <dd>
            <Badge
              :text="verdictLabel(selected.usage.verdict).text"
              :tone="verdictLabel(selected.usage.verdict).tone"
            />
          </dd>
          <dt>容差</dt>
          <dd class="mono">{{ pct(selected.usage.tolerance) }}</dd>
          <dt>最小绝对差值</dt>
          <dd class="mono">{{ num(selected.usage.min_absolute_delta) }}</dd>
          <dt>最大绝对偏差</dt>
          <dd class="mono">{{ num(selected.usage.worst_abs) }}</dd>
          <dt>计量方式</dt>
          <dd class="mono">{{ dash(selected.usage.method) }}</dd>
          <dt>分词器</dt>
          <dd class="mono">{{ dash(selected.usage.tokenizer_family) }}</dd>
          <dt>上游是否上报</dt>
          <dd>{{ selected.usage.upstream_reported ? '是' : '否' }}</dd>
        </dl>
      </div>

      <div>
        <div class="section-title">成本</div>
        <dl class="kv">
          <dt>输入成本</dt>
          <dd class="mono">{{ money(selected.cost?.input_cost, selected.cost?.currency) }}</dd>
          <dt>输出成本</dt>
          <dd class="mono">{{ money(selected.cost?.output_cost, selected.cost?.currency) }}</dd>
          <dt>合计</dt>
          <dd class="mono">{{ money(selected.cost?.total_cost, selected.cost?.currency) }}</dd>
          <dt>标价合计</dt>
          <dd class="mono">{{ money(selected.cost?.list_cost, selected.cost?.currency) }}</dd>
          <dt>折扣</dt>
          <dd class="mono">{{ pct(selected.cost?.discount_pct, 1, true) }}</dd>
          <dt>计价依据</dt>
          <dd class="mono">{{ dash(selected.cost?.basis) }}</dd>
          <dt>计价模型</dt>
          <dd class="mono">{{ dash(selected.cost?.price_model) }}</dd>
        </dl>
      </div>

      <div>
        <div class="section-title">延迟</div>
        <dl class="kv">
          <dt>总耗时</dt>
          <dd class="mono">{{ millis(selected.latency_ms?.total_ms) }}</dd>
          <dt>首字节</dt>
          <dd class="mono">{{ millis(selected.latency_ms?.ttfb_ms) }}</dd>
          <dt>上游耗时</dt>
          <dd class="mono">{{ millis(selected.latency_ms?.upstream_ms) }}</dd>
          <dt>出口审计</dt>
          <dd class="mono">{{ millis(selected.latency_ms?.outbound_audit_ms) }}</dd>
          <dt>入口审计</dt>
          <dd class="mono">{{ millis(selected.latency_ms?.inbound_audit_ms) }}</dd>
        </dl>
      </div>

      <div v-if="selected.fingerprint?.checked">
        <div class="section-title">指纹核验</div>
        <dl class="kv">
          <dt>得分</dt>
          <dd class="mono">{{ selected.fingerprint.score?.toFixed(4) ?? '—' }}</dd>
          <dt>阈值</dt>
          <dd class="mono">{{ selected.fingerprint.threshold?.toFixed(4) ?? '—' }}</dd>
          <dt>结论</dt>
          <dd>
            <Badge
              :text="selected.fingerprint.verdict ?? '—'"
              :tone="selected.fingerprint.verdict === 'match' ? 'ok' : 'danger'"
            />
          </dd>
          <dt>基线来源</dt>
          <dd class="mono">{{ dash(selected.fingerprint.baseline_source) }}</dd>
        </dl>
      </div>

      <div v-if="selected.route_reason || selected.failover_note">
        <div class="section-title">路由</div>
        <div v-if="selected.route_reason" class="small break">
          选择原因：{{ selected.route_reason }}
        </div>
        <div v-if="selected.failover_note" class="small break" style="color: var(--warn)">
          故障转移：{{ selected.failover_note }}
        </div>
      </div>

      <div>
        <div class="section-title">完整性</div>
        <dl class="kv">
          <dt>请求哈希</dt>
          <dd class="mono break">{{ dash(selected.request_hash) }}</dd>
          <dt>响应哈希</dt>
          <dd class="mono break">{{ dash(selected.response_hash) }}</dd>
          <dt>前序哈希</dt>
          <dd class="mono break">{{ shortHash(selected.prev_hash, 12) }}</dd>
          <dt>本条哈希</dt>
          <dd class="mono break">{{ shortHash(selected.entry_hash, 12) }}</dd>
        </dl>
      </div>

      <div v-if="selected.sample">
        <div class="section-title">样本（已脱敏）</div>
        <pre v-if="selected.sample.request_text" class="json">{{ selected.sample.request_text }}</pre>
        <pre v-if="selected.sample.response_text" class="json">{{ selected.sample.response_text }}</pre>
      </div>
    </template>

    <template #footer>
      <button class="btn" type="button" @click="closeDetail">关闭</button>
    </template>
  </Modal>
</template>
