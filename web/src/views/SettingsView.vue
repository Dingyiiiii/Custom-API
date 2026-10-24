<script setup lang="ts">
import { computed } from 'vue'

import { api } from '../api'
import { useAsync } from '../async'
import Badge from '../components/Badge.vue'
import Panel from '../components/Panel.vue'
import StateBlock from '../components/StateBlock.vue'
import { dash, num, pct } from '../format'
import { failMode, mode, protocolName, protocolTone, strategy, trust } from '../labels'
import { useAppStore } from '../store'

const config = useAsync(api.config)
const store = useAppStore()

const version = computed(() => store.version)
const channels = computed(() => config.data.value?.channels ?? [])
</script>

<template>
  <Panel title="版本信息" hint="GET /admin/version">
    <template #actions>
      <button class="btn btn-sm" type="button" @click="store.loadMeta()">重新读取</button>
    </template>
    <StateBlock :loading="version === null && store.metaError === ''" :error="store.metaError">
      <dl v-if="version" class="kv">
        <dt>版本</dt>
        <dd class="mono">{{ dash(version.version) }}</dd>
        <dt>提交</dt>
        <dd class="mono break">{{ dash(version.commit) }}</dd>
        <dt>构建时间</dt>
        <dd class="mono">{{ dash(version.built) }}</dd>
      </dl>
    </StateBlock>
  </Panel>

  <StateBlock
    :loading="config.loading.value && config.data.value === null"
    :error="config.error.value"
    :empty="config.data.value === null"
    empty-text="管理接口没有返回配置摘要。"
    @retry="config.load()"
  >
    <template v-if="config.data.value">
      <div class="banner warn">
        <span class="banner-icon">⛨</span>
        <div>
          <div class="banner-title">只读页面，且密钥已脱敏</div>
          <div class="banner-text">
            接口只返回 <span class="mono">has_key</span> 布尔值与密钥前缀，任何 API Key
            的明文都不会出现在浏览器里；配置的修改请直接编辑服务端配置文件并重载。
          </div>
        </div>
      </div>

      <div class="grid grid-2">
        <Panel title="服务端" hint="server">
          <dl class="kv">
            <dt>用户入口</dt>
            <dd class="mono">{{ dash(config.data.value.server.listen) }}</dd>
            <dt>管理入口</dt>
            <dd class="mono">{{ dash(config.data.value.server.admin_listen) }}</dd>
            <dt>路由策略</dt>
            <dd>
              <Badge
                :text="strategy(config.data.value.server.route_strategy).text"
                :tone="strategy(config.data.value.server.route_strategy).tone"
                :title="config.data.value.server.route_strategy"
              />
            </dd>
            <dt>失效模式</dt>
            <dd>
              <Badge
                :text="failMode(config.data.value.server.fail_mode).text"
                :tone="failMode(config.data.value.server.fail_mode).tone"
              />
            </dd>
            <dt>上游超时</dt>
            <dd class="mono">{{ dash(config.data.value.server.upstream_timeout) }}</dd>
          </dl>
        </Panel>

        <Panel title="策略" hint="policy">
          <dl class="kv">
            <dt>出口模式</dt>
            <dd>
              <Badge
                :text="mode(config.data.value.policy.outbound_mode).text"
                :tone="mode(config.data.value.policy.outbound_mode).tone"
              />
            </dd>
            <dt>入口模式</dt>
            <dd>
              <Badge
                :text="mode(config.data.value.policy.inbound_mode).text"
                :tone="mode(config.data.value.policy.inbound_mode).tone"
              />
            </dd>
            <dt>工具调用策略</dt>
            <dd>
              <Badge
                :text="mode(config.data.value.policy.tool_policy).text"
                :tone="mode(config.data.value.policy.tool_policy).tone"
              />
            </dd>
            <dt>计量容差</dt>
            <dd class="mono">{{ pct(config.data.value.policy.tolerance) }}</dd>
            <dt>出口规则数</dt>
            <dd class="mono">{{ num(config.data.value.policy.outbound_rules) }}</dd>
            <dt>入口规则数</dt>
            <dd class="mono">{{ num(config.data.value.policy.inbound_rules) }}</dd>
          </dl>
        </Panel>
      </div>

      <Panel
        :title="`渠道配置（${num(channels.length)} 个）`"
        hint="密钥列只显示是否存在，不显示内容"
      >
        <div class="table-wrap">
          <table class="data compact">
            <thead>
              <tr>
                <th>渠道</th>
                <th>上游地址</th>
                <th>协议</th>
                <th>可信度</th>
                <th>启用</th>
                <th>权重</th>
                <th>密钥</th>
                <th>模型</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="ch in channels" :key="ch.name">
                <td class="mono">{{ ch.name }}</td>
                <td class="mono break">{{ dash(ch.base_url) }}</td>
                <td>
                  <Badge :text="protocolName(ch.protocol)" :tone="protocolTone(ch.protocol)" />
                </td>
                <td>
                  <Badge :text="trust(ch.trust).text" :tone="trust(ch.trust).tone" />
                </td>
                <td>
                  <Badge :text="ch.enabled ? '启用' : '停用'" :tone="ch.enabled ? 'ok' : 'muted'" />
                </td>
                <td class="mono">{{ ch.weight }}</td>
                <td>
                  <Badge
                    :text="ch.has_key ? '已配置（值已脱敏）' : '未配置'"
                    :tone="ch.has_key ? 'ok' : 'warn'"
                    title="接口只返回 has_key 布尔值"
                  />
                </td>
                <td class="small">
                  <details v-if="(ch.models ?? []).length > 0" class="raw">
                    <summary>{{ (ch.models ?? []).length }} 个</summary>
                    <div class="tag-list" style="margin-top: 6px">
                      <Badge v-for="m in ch.models ?? []" :key="m" :text="m" tone="muted" />
                    </div>
                  </details>
                  <span v-else class="muted">未声明</span>
                </td>
              </tr>
              <tr v-if="channels.length === 0">
                <td colspan="8" class="muted">没有配置任何渠道。</td>
              </tr>
            </tbody>
          </table>
        </div>
      </Panel>
    </template>
  </StateBlock>
</template>
