// Management-API credential handling.
//
// The console is normally reached over the loopback interface, where the
// gateway trusts the caller and no credential is involved. When an operator
// exposes the management listener on a routable address they must set
// server.admin_remote and server.admin_token; the console then has to present
// that token on every request.
//
// The token is kept in sessionStorage rather than localStorage: it is a
// management credential, and having it disappear when the tab closes is a
// cheaper mistake than leaving it on a shared workstation indefinitely. It is
// also never persisted server-side on the console's behalf.

const STORAGE_KEY = 'customs.admin_token'

/** Query key accepted once at startup for credential bootstrap. */
const FRAGMENT_KEY = 'token'

/**
 * Adopts a credential supplied in the URL fragment, if there is one.
 *
 * A fragment is used rather than a query parameter because fragments are never
 * sent to the server: the token stays out of the gateway's access log and out
 * of any intermediary's. This exists so an operator can open the console from a
 * script or a bookmark without typing, for example
 *
 *	http://gateway:8081/#token=<server.admin_token>
 *
 * The fragment is stripped afterwards so it does not linger in the address bar,
 * in browser history or in a copied link. Returns true when a credential was
 * adopted.
 */
export function adoptFragmentToken(): boolean {
  if (typeof window === 'undefined') return false
  const hash = window.location.hash
  if (hash === '') return false
  const params = new URLSearchParams(hash.startsWith('#') ? hash.slice(1) : hash)
  const token = params.get(FRAGMENT_KEY)
  if (token === null || token.trim() === '') return false
  saveToken(token.trim())
  params.delete(FRAGMENT_KEY)
  const rest = params.toString()
  const clean = window.location.pathname + window.location.search + (rest === '' ? '' : `#${rest}`)
  window.history.replaceState(null, '', clean)
  return true
}

/** Returns the stored token, or an empty string when none was entered. */
export function loadToken(): string {
  try {
    return sessionStorage.getItem(STORAGE_KEY) ?? ''
  } catch {
    // Private-mode browsers can throw on storage access; the console still
    // works, it just cannot remember the token between reloads.
    return ''
  }
}

/** Stores the token so a reload keeps the console authenticated. */
export function saveToken(token: string): void {
  try {
    if (token === '') sessionStorage.removeItem(STORAGE_KEY)
    else sessionStorage.setItem(STORAGE_KEY, token)
  } catch {
    /* see loadToken */
  }
}

/** Forgets the stored token. */
export function clearToken(): void {
  saveToken('')
}

/**
 * Headers to attach to a management request.
 *
 * An empty token contributes nothing, which is what a loopback deployment
 * wants: the gateway ignores the header there.
 */
export function authHeaders(): Record<string, string> {
  const token = loadToken()
  return token === '' ? {} : { Authorization: `Bearer ${token}` }
}

/** Error-code names the gateway uses to say "a credential would help". */
export const UNAUTHORIZED = 'admin_unauthorized'
/** Error-code name for "this listener refuses remote callers entirely". */
export const FORBIDDEN = 'admin_forbidden'
