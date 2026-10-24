<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'

import { api } from '../api'
import { useAsync } from '../async'
import BarMeter from '../components/BarMeter.vue'
import Badge from '../components/Badge.vue'
import Panel from '../components/Panel.vue'
import StatCard from '../components/StatCard.vue'
import StateBlock from '../components/StateBlock.vue'
import { bytes, duration, num, pct, ratio, shortHash, ts } from '../format'
import { failMode, mode, strategy } from '../labels'
import type { Counters } from '../types'

const POLL_MS = 5000

const stats = useAsync(api.stats, { immediate: false, keepOnError: true })
const verify = useAsync(api.verifyAudit)

let timer: number | undefined
// When the last successful poll landed, shown as the sampling timestamp.
const sampledAt = ref('')
const lastOk = computed(() => stats.loaded.value && stats.error.value === '')

async function refresh(): Promise<void> {
  await stats.load()
  if (stats.error.value === '') sampledAt.value = new Date().toISOString()
}

onMounted(async () => {
  await refresh()
  timer = window.setInterval(() => void refresh(), POLL_MS)
})

onUnmounted(() => {
  if (timer !== undefined) window.clearInterval(timer)
})

const counters = computed<Counters | null>(() => stats.data.value?.stats ?? null)

const blockedTotal = computed(() => {
  const c = counters.value
  if (c === null) return 0
  return (c.blocked_outbound ?? 0) + (c.blocked_inbound ?? 0)
})

/** Share of requests the policy engine refused, as a 0..1 ratio. */
const blockRate = computed(() => {
  const c = counters.value
  if (c === null) return 0
  return ratio(blockedTotal.value, c.requests)
})
</script>

<template>
  <StateBlock
    :loading="stats.loading.value && stats.data.value === null"
    :error="stats.error.value && stats.data.value === null ? stats.error.value : ''"
    :empty="stats.data.value === null"
    empty-text="管理接口没有返回运行状态。"
    @retry="stats.load()"
  >
    <template v-if="stats.data.value">
      <div class="banner" :class="lastOk ? 'ok' : 'warn'">
        <span class="banner-icon">{{ lastOk ? '●' : '▲' }}</span>
        <div>
          <div class="banner-title">
            网关运行中 · 已运行 {{ duration(stats.data.value.uptime_s) }}
          </div>
          <div class="banner-text">
            每 {{ POLL_MS / 1000 }} 秒自动刷新（离开本页即停止）。
            <span v-if="!lastOk">最近一次刷新失败：{{ stats.error.value }}</span>
          </div>
        </div>
        <span class="spacer" />
        <span v-if="stats.loading.value" class="badge muted"><span class="spinner" />刷新中</span>
        <button class="btn btn-sm" type="button" @click="refresh">立即刷新</button>
      </div>

      <!-- Audit chain integrity: the one thing an operator must not miss. -->
      <div v-if="verify.loading.value && verify.data.value === null" class="banner">
        <span class="banner-icon">⛨</span>
        <div class="banner-title"><span class="spinner" />正在校验审计哈希链…</div>
      </div>
      <div v-else-if="verify.error.value" class="banner danger">
        <span class="banner-icon">✕</span>
        <div>
          <div class="banner-title">审计链校验失败：无法完成校验</div>
          <div class="banner-text">{{ verify.error.value }}</div>
        </div>
        <span class="spacer" />
        <button class="btn btn-sm" type="button" @click="verify.load()">重试</button>
      </div>
      <div
        v-else-if="verify.data.value"
        class="banner"
        :class="verify.data.value.ok ? 'ok' : 'danger'"
      >
        <span class="banner-icon">{{ verify.data.value.ok ? '✔' : '✕' }}</span>
        <div>
          <div class="banner-title">
            审计链{{ verify.data.value.ok ? '完整' : '已损坏' }} ·
            {{ num(verify.data.value.records) }} 条记录 · 链头 #{{ verify.data.value.head_seq }}
          </div>
          <div class="banner-text">
            链头哈希 <span class="mono">{{ shortHash(verify.data.value.head_hash, 10) }}</span>
            · 已核对锚点 {{ num(verify.data.value.anchors_checked) }} 个
            <span v-if="verify.data.value.anchors_ignored">
              （忽略 {{ num(verify.data.value.anchors_ignored) }} 个超出账本的锚点）
            </span>
          </div>
        </div>
        <span class="spacer" />
        <button class="btn btn-sm" type="button" :disabled="verify.loading.value" @click="verify.load()">
          重新校验
        </button>
      </div>

      <div v-if="verify.data.value && !verify.data.value.ok" class="card">
        <header class="card-head">
          <h3>违规明细</h3>
          <span class="hint">{{ num(verify.data.value.violations?.length ?? 0) }} 项</span>
        </header>
        <div class="card-body tight">
          <div class="table-wrap">
            <table class="data">
              <thead>
                <tr>
                  <th>类型</th>
                  <th>行号</th>
                  <th>序号</th>
                  <th>详情</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="(v, i) in verify.data.value.violations ?? []" :key="i">
                  <td><Badge :text="v.kind" tone="danger" /></td>
                  <td class="mono">{{ v.line ?? '—' }}</td>
                  <td class="mono">{{ v.seq ?? '—' }}</td>
                  <td class="wrap">{{ v.detail || '—' }}</td>
                </tr>
                <tr v-if="(verify.data.value.violations?.length ?? 0) === 0">
                  <td colspan="4" class="muted">接口返回 ok=false，但没有给出违规条目。</td>
                </tr>
              </tbody>
            </table>
          </div>
        </div>
      </div>

      <div class="stats">
        <StatCard label="请求总数" :value="num(counters?.requests)" tone="accent" />
        <StatCard
          label="出口拦截"
          :value="num(counters?.blocked_outbound)"
          :tone="(counters?.blocked_outbound ?? 0) > 0 ? 'warn' : 'muted'"
          foot="命中出口规则的请求"
        />
        <StatCard
          label="入口拦截"
          :value="num(counters?.blocked_inbound)"
          :tone="(counters?.blocked_inbound ?? 0) > 0 ? 'warn' : 'muted'"
          foot="命中入口规则的响应"
        />
        <StatCard
          label="上游错误"
          :value="num(counters?.upstream_errors)"
          :tone="(counters?.upstream_errors ?? 0) > 0 ? 'danger' : 'muted'"
        />
        <StatCard
          label="计量超收"
          :value="num(counters?.over_billed)"
          :tone="(counters?.over_billed ?? 0) > 0 ? 'danger' : 'muted'"
          foot="上游上报用量高于本地统计"
        />
        <StatCard label="流式请求" :value="num(counters?.streamed_requests)" />
        <StatCard
          label="指纹不符"
          :value="num(counters?.fingerprint_miss)"
          :tone="(counters?.fingerprint_miss ?? 0) > 0 ? 'warn' : 'muted'"
        />
        <StatCard
          label="降级响应"
          :value="num(counters?.degraded)"
          :tone="(counters?.degraded ?? 0) > 0 ? 'warn' : 'muted'"
        />
        <StatCard label="入站流量" :value="bytes(counters?.bytes_in)" />
        <StatCard label="出站流量" :value="bytes(counters?.bytes_out)" />
        <StatCard
          label="拦截占比"
          :value="pct(blockRate)"
          :foot="`共拦截 ${num(blockedTotal)} 次`"
        />
        <StatCard
          label="运行时长"
          :value="duration(stats.data.value.uptime_s)"
          foot="进程启动至今"
        />
      </div>

      <div class="grid grid-2">
        <Panel title="规模">
          <dl class="kv">
            <dt>渠道数</dt>
            <dd class="mono">{{ num(stats.data.value.channels) }}</dd>
            <dt>用户数</dt>
            <dd class="mono">{{ num(stats.data.value.users) }}</dd>
            <dt>审计序号</dt>
            <dd class="mono">{{ num(stats.data.value.audit_seq) }}</dd>
            <dt>链头哈希</dt>
            <dd class="mono break">{{ shortHash(stats.data.value.audit_head, 12) }}</dd>
            <dt>账本路径</dt>
            <dd class="mono break">{{ stats.data.value.audit_path || '—' }}</dd>
          </dl>
        </Panel>

        <Panel title="策略与执行模式">
          <dl class="kv">
            <dt>路由策略</dt>
            <dd>
              <Badge
                :text="strategy(stats.data.value.strategy).text"
                :tone="strategy(stats.data.value.strategy).tone"
                :title="strategy(stats.data.value.strategy).raw"
              />
            </dd>
            <dt>出口模式</dt>
            <dd>
              <Badge
                :text="mode(stats.data.value.outbound_mode).text"
                :tone="mode(stats.data.value.outbound_mode).tone"
                :title="mode(stats.data.value.outbound_mode).raw"
              />
            </dd>
            <dt>入口模式</dt>
            <dd>
              <Badge
                :text="mode(stats.data.value.inbound_mode).text"
                :tone="mode(stats.data.value.inbound_mode).tone"
                :title="mode(stats.data.value.inbound_mode).raw"
              />
            </dd>
            <dt>失效模式</dt>
            <dd>
              <Badge
                :text="failMode(stats.data.value.fail_mode).text"
                :tone="failMode(stats.data.value.fail_mode).tone"
                :title="failMode(stats.data.value.fail_mode).raw"
              />
            </dd>
            <dt>降级占比</dt>
            <dd>
              <BarMeter
                :value="ratio(counters?.degraded, counters?.requests)"
                :label="`${pct(ratio(counters?.degraded, counters?.requests))}（${num(counters?.degraded)} 次）`"
                :good="1"
                :bad="0.999"
              />
            </dd>
          </dl>
        </Panel>
      </div>

      <Panel title="采样" hint="统计数据来自网关内存计数器，进程重启后归零">
        <div class="small muted">
          最近一次成功采样：{{ sampledAt === '' ? '尚未成功' : ts(sampledAt) }} ·
          轮询间隔 {{ POLL_MS / 1000 }} 秒，仅在本页生效 ·
          审计序号 {{ num(stats.data.value.audit_seq) }}
        </div>
      </Panel>
    </template>
  </StateBlock>
</template>
