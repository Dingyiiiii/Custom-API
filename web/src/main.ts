import { createApp } from 'vue'
import { createPinia } from 'pinia'

import App from './App.vue'
import { adoptFragmentToken } from './auth'
import { router } from './router'
import './styles.css'

// Before the app mounts: adopt a credential passed as #token=... so a scripted
// or bookmarked link can open the console without a manual prompt. This has to
// happen before the first API call, which the store issues on mount.
adoptFragmentToken()

createApp(App).use(createPinia()).use(router).mount('#app')
