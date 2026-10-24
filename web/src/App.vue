<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { RouterLink, RouterView, useRoute } from 'vue-router'

import Modal from './components/Modal.vue'
import ToastHost from './components/ToastHost.vue'
import { NAV } from './router'
import { onCredentialRequired } from './api'
import { useAppStore } from './store'
import { duration } from './format'
import { failMode } from './labels'

const route = useRoute()
const store = useAppStore()

const current = computed(() => NAV.find((n) => n.path === route.path) ?? NAV[0])

// Management credential dialog. Reached only when the gateway refuses a call
// with admin_unauthorized - that is, when the operator has exposed the
// management listener with server.admin_remote and set server.admin_token.
const credentialOpen = computed(() => store.credentialRequired)
const tokenInput = ref('')
const busy = ref(false)

// Any management call may be the first to discover the requirement, so the
// dialog is driven from the API layer rather than from a single view.
onCredentialRequired(() => {
  store.credentialRequired = true
})

async function submitCredential(): Promise<void> {
  busy.value = true
  const ok = await store.submitCredential(tokenInput.value)
  busy.value = false
  if (ok) {
    tokenInput.value = ''
    store.notify('管理令牌已生效', 'ok')
  }
}

onMounted(() => {
  // Identity + health for the top bar; failures are reported in the bar rather
  // than blocking any route.
  void store.loadMeta()
})
</script>

<template>
  <div class="app">
    <aside class="sidebar">
      <div class="brand">
        <div class="brand-title">海关网关控制台</div>
        <div class="brand-sub">customapi-customs</div>
      </div>
      <nav class="nav">
        <RouterLink
          v-for="item in NAV"
          :key="item.path"
          class="nav-item"
          :class="{ active: item.path === route.path }"
          :to="item.path"
        >
          <span class="nav-icon">{{ item.icon }}</span>
          <span>{{ item.title }}</span>
        </RouterLink>
      </nav>
      <div class="sidebar-foot">
        <div>{{ store.versionLabel }}</div>
        <div v-if="store.health">
          出口 {{ failMode(store.health.fail_mode).text }}
        </div>
      </div>
    </aside>

    <div class="main">
      <header class="topbar">
        <h1>{{ current.title }}</h1>
        <span class="topbar-sub">{{ current.hint }}</span>
        <span class="spacer" />
        <span v-if="store.metaError" class="badge danger" :title="store.metaError">
          后端不可达
        </span>
        <span v-else-if="store.credentialRequired" class="badge warn"> 需要令牌 </span>
        <span v-else-if="store.health" class="badge ok">
          运行中 · 已运行 {{ duration(store.health.uptime_s) }}
        </span>
        <span v-else class="badge muted"><span class="spinner" />连接中</span>
        <button
          v-if="store.hasCredential"
          class="btn btn-sm"
          type="button"
          title="清除已保存的管理令牌"
          @click="store.forgetCredential()"
        >
          退出
        </button>
      </header>

      <main class="content">
        <RouterView />
      </main>
    </div>

    <Modal
      :open="credentialOpen"
      title="管理令牌"
      :busy="busy"
      @close="() => {}"
    >
      <p class="muted">
        管理接口已通过 <code>server.admin_remote</code> 对外开放，浏览器需要出示
        <code>server.admin_token</code> 才能读取数据。令牌只保存在本标签页的
        sessionStorage 中，关闭标签页即失效。
      </p>
      <div class="field">
        <label for="admin-token">管理令牌</label>
        <input
          id="admin-token"
          v-model="tokenInput"
          type="password"
          autocomplete="off"
          placeholder="server.admin_token"
          @keyup.enter="submitCredential"
        />
      </div>
      <p v-if="store.credentialError" class="form-error">{{ store.credentialError }}</p>
      <template #footer>
        <button class="btn btn-primary" type="button" :disabled="busy" @click="submitCredential">
          {{ busy ? '校验中…' : '保存并继续' }}
        </button>
      </template>
    </Modal>

    <ToastHost />
  </div>
</template>
