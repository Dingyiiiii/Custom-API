<script setup lang="ts">
import { computed, ref } from 'vue'

import { api, errorText } from '../api'
import { useAsync } from '../async'
import Badge from '../components/Badge.vue'
import BarMeter from '../components/BarMeter.vue'
import Modal from '../components/Modal.vue'
import Panel from '../components/Panel.vue'
import StatCard from '../components/StatCard.vue'
import StateBlock from '../components/StateBlock.vue'
import { compact, money, num, pct, ratio, ts } from '../format'
import { period as periodLabel, trust } from '../labels'
import { useAppStore } from '../store'
import type { UserAction, UserState } from '../types'

const store = useAppStore()
const users = useAsync(api.users)
const search = ref('')

/** Modal state for the numeric actions. */
type EditorMode = 'grant' | 'add_quota' | 'set_period' | 'suspend' | 'resume'

const editor = ref<{ open: boolean; mode: EditorMode; user: UserState | null }>({
  open: false,
  mode: 'grant',
  user: null,
})
const amount = ref(0)
const newPeriod = ref('monthly')
const busy = ref(false)
const editorError = ref('')

const PERIODS = [
  { value: 'daily', label: '每日' },
  { value: 'weekly', label: '每周' },
  { value: 'monthly', label: '每月' },
  { value: 'none', label: '不重置' },
]

const filtered = computed(() => {
  const list = users.data.value?.users ?? []
  const needle = search.value.trim().toLowerCase()
  if (needle === '') return list
  return list.filter((u) => u.name.toLowerCase().includes(needle))
})

const totals = computed(() => {
  const list = users.data.value?.users ?? []
  let consumed = 0
  let spend = 0
  let requests = 0
  let unlimited = 0
  let suspended = 0
  for (const u of list) {
    consumed += u.consumed ?? 0
    spend += u.spend ?? 0
    requests += u.requests ?? 0
    if ((u.quota ?? 0) <= 0) unlimited++
    if (!u.enabled) suspended++
  }
  return {
    consumed,
    spend,
    requests,
    unlimited,
    suspended,
    currency: list[0]?.currency ?? 'CNY',
  }
})

/** Quota left this period; -1 marks an unlimited account. */
function remaining(user: UserState): number {
  const quota = user.quota ?? 0
  if (quota <= 0) return -1
  return Math.max(0, quota - (user.consumed ?? 0))
}

function usedRatio(user: UserState): number {
  const quota = user.quota ?? 0
  if (quota <= 0) return 0
  return ratio(user.consumed, quota)
}

function openEditor(user: UserState, mode: EditorMode): void {
  editor.value = { open: true, mode, user }
  editorError.value = ''
  amount.value = mode === 'add_quota' ? 0 : (user.quota ?? 0)
  newPeriod.value = user.period && user.period !== '' ? user.period : 'monthly'
}

/** The per-row action menu; it resets to the placeholder after each choice. */
function onAction(user: UserState, event: Event): void {
  const el = event.target as HTMLSelectElement
  const value = el.value
  el.value = ''
  if (value === '') return
  openEditor(user, value as EditorMode)
}

const editorTitle = computed(() => {
  const name = editor.value.user?.name ?? ''
  switch (editor.value.mode) {
    case 'grant':
      return `发放配额 · ${name}`
    case 'add_quota':
      return `追加配额 · ${name}`
    case 'set_period':
      return `设置周期 · ${name}`
    case 'suspend':
      return `暂停账号 · ${name}`
    default:
      return `恢复账号 · ${name}`
  }
})

async function submit(): Promise<void> {
  const user = editor.value.user
  if (user === null) return
  busy.value = true
  editorError.value = ''
  const mode = editor.value.mode
  try {
    let action: UserAction = 'grant'
    let quota = 0
    let period = ''
    switch (mode) {
      case 'grant':
        action = 'grant'
        quota = Math.max(0, Math.trunc(amount.value) || 0)
        break
      case 'add_quota':
        action = 'add_quota'
        quota = Math.trunc(amount.value) || 0
        break
      case 'set_period':
        action = 'set_period'
        period = newPeriod.value
        break
      case 'suspend':
        action = 'suspend'
        break
      default:
        action = 'resume'
        break
    }
    const res = await api.userAction(user.name, action, quota, period)
    store.notify(`已更新用户 ${res.user.name}：${modeLabel(mode)}`, 'ok')
    editor.value = { open: false, mode, user: null }
    await users.load()
  } catch (err) {
    editorError.value = errorText(err)
  } finally {
    busy.value = false
  }
}

function modeLabel(mode: EditorMode): string {
  switch (mode) {
    case 'grant':
      return '发放配额'
    case 'add_quota':
      return '追加配额'
    case 'set_period':
      return '设置周期'
    case 'suspend':
      return '暂停'
    default:
      return '恢复'
  }
}
</script>

<template>
  <StateBlock
    :loading="users.loading.value && users.data.value === null"
    :error="users.error.value"
    :empty="users.data.value === null"
    empty-text="管理接口没有返回用户列表。"
    @retry="users.load()"
  >
    <template v-if="users.data.value">
      <div class="stats">
        <StatCard label="用户数" :value="num(users.data.value.count)" tone="accent" />
        <StatCard label="不限配额" :value="num(totals.unlimited)" foot="配额为 0 的账号" />
        <StatCard
          label="已暂停"
          :value="num(totals.suspended)"
          :tone="totals.suspended > 0 ? 'warn' : 'muted'"
        />
        <StatCard label="本周期请求" :value="num(totals.requests)" />
        <StatCard label="本周期消费" :value="compact(totals.consumed)" :foot="num(totals.consumed)" />
        <StatCard label="本周期花费" :value="money(totals.spend, totals.currency)" />
      </div>

      <Panel title="用户与配额" hint="POST /admin/users 支持 grant、add_quota、suspend、resume、set_period">
        <template #actions>
          <input v-model="search" type="search" placeholder="搜索用户名" style="width: 180px" />
          <button class="btn btn-sm" type="button" @click="users.load()">刷新</button>
        </template>

        <div class="table-wrap">
          <table class="data compact">
            <thead>
              <tr>
                <th>用户</th>
                <th>周期</th>
                <th>配额</th>
                <th>已用</th>
                <th>剩余</th>
                <th>使用率</th>
                <th>请求</th>
                <th>花费</th>
                <th>最近活动</th>
                <th>状态</th>
                <th>操作</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="u in filtered" :key="u.name">
                <td>
                  <div class="mono">{{ u.name }}</div>
                  <div class="tag-list" style="margin-top: 2px">
                    <Badge :text="trust(u.trust).text" :tone="trust(u.trust).tone" />
                    <Badge v-if="(u.models ?? []).length > 0" text="限定模型" tone="info" />
                  </div>
                </td>
                <td>
                  <Badge :text="periodLabel(u.period).text" :tone="periodLabel(u.period).tone" />
                </td>
                <td class="mono">
                  <Badge v-if="(u.quota ?? 0) <= 0" text="不限" tone="muted" />
                  <span v-else>{{ compact(u.quota) }}</span>
                </td>
                <td class="mono" :title="`累计消费 ${num(u.lifetime_consumed)}`">
                  {{ compact(u.consumed) }}
                </td>
                <td class="mono">
                  <Badge v-if="remaining(u) < 0" text="不限" tone="muted" />
                  <span v-else>{{ compact(remaining(u)) }}</span>
                </td>
                <td>
                  <BarMeter
                    v-if="(u.quota ?? 0) > 0"
                    :value="usedRatio(u)"
                    :label="pct(usedRatio(u))"
                    :good="0.9"
                    :bad="0.99"
                    :tone="usedRatio(u) >= 0.99 ? 'danger' : ''"
                  />
                  <span v-else class="muted">不限</span>
                </td>
                <td class="mono">{{ num(u.requests) }}</td>
                <td class="mono">{{ money(u.spend, u.currency) }}</td>
                <td class="small nowrap" :title="`周期开始：${ts(u.period_start)}`">
                  {{ ts(u.last_seen) }}
                </td>
                <td>
                  <Badge :text="u.enabled ? '启用' : '已暂停'" :tone="u.enabled ? 'ok' : 'warn'" />
                </td>
                <td>
                  <select
                    class="action-select"
                    :value="''"
                    :aria-label="`对 ${u.name} 执行操作`"
                    @change="onAction(u, $event)"
                  >
                    <option value="">操作…</option>
                    <option value="grant">发放配额</option>
                    <option value="add_quota">追加配额</option>
                    <option value="set_period">设置周期</option>
                    <option v-if="u.enabled" value="suspend">暂停账号</option>
                    <option v-else value="resume">恢复账号</option>
                  </select>
                </td>
              </tr>
              <tr v-if="filtered.length === 0">
                <td colspan="11" class="muted">
                  {{ (users.data.value.users.length ?? 0) === 0 ? '还没有任何用户。' : '没有匹配的用户。' }}
                </td>
              </tr>
            </tbody>
          </table>
        </div>

        <div class="small muted" style="margin-top: 8px">
          配额单位为 token，<span class="mono">0</span> 表示不限量。用户由服务端配置文件定义，控制台只能修改配额与状态。
        </div>
      </Panel>
    </template>
  </StateBlock>

  <Modal :open="editor.open" :title="editorTitle" :busy="busy" @close="editor.open = false">
    <div v-if="editor.user">
      <dl class="kv">
        <dt>用户</dt>
        <dd class="mono">{{ editor.user.name }}</dd>
        <dt>当前配额</dt>
        <dd class="mono">
          {{ (editor.user.quota ?? 0) <= 0 ? '不限' : num(editor.user.quota) }}
        </dd>
        <dt>本周期已用</dt>
        <dd class="mono">{{ num(editor.user.consumed) }}</dd>
        <dt>当前周期</dt>
        <dd>{{ periodLabel(editor.user.period).text }}</dd>
      </dl>

      <div class="divider" />

      <div v-if="editor.mode === 'grant'" class="field">
        <label for="editor-amount">新配额（token，0 表示不限）</label>
        <input id="editor-amount" v-model.number="amount" type="number" min="0" />
        <div class="small muted">
          直接设置为该数值；例如 {{ num(50000000) }} 约等于 5000 万 token。
        </div>
      </div>

      <div v-else-if="editor.mode === 'add_quota'" class="field">
        <label for="editor-delta">追加数量（token，可为负数以扣减）</label>
        <input id="editor-delta" v-model.number="amount" type="number" />
        <div class="small muted">在当前配额基础上增减。</div>
      </div>

      <div v-else-if="editor.mode === 'set_period'" class="field">
        <label for="editor-period">重置周期</label>
        <select id="editor-period" v-model="newPeriod">
          <option v-for="p in PERIODS" :key="p.value" :value="p.value">{{ p.label }}</option>
        </select>
        <div class="small muted">修改周期后，本周期已消费的额度不会被清零。</div>
      </div>

      <div v-else class="small">
        确认要{{ modeLabel(editor.mode) }}用户 <span class="mono">{{ editor.user.name }}</span> 吗？
        <template v-if="editor.mode === 'suspend'">暂停后该用户的请求会被拒绝。</template>
        <template v-else>恢复后该用户可以继续调用。</template>
      </div>

      <div v-if="editorError" class="state error">{{ editorError }}</div>
    </div>

    <template #footer>
      <button class="btn" type="button" :disabled="busy" @click="editor.open = false">取消</button>
      <button class="btn btn-primary" type="button" :disabled="busy" @click="submit">
        <span v-if="busy" class="spinner" />确认{{ modeLabel(editor.mode) }}
      </button>
    </template>
  </Modal>
</template>
