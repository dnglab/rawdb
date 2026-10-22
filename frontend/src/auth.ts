// Client-side auth state. Auth itself is cookie-based (`rawdb_session`); this
// just mirrors `GET /auth/me` so the UI can react to login/logout. Module
// singleton — every importer shares the same refs.

import { computed, ref } from 'vue';
import { router } from './router';

export interface Me {
  sub: string;
  source: string;
  roles: string[];
  display_name: string | null;
}

const user = ref<Me | null>(null);
// False until the first /auth/me round-trip resolves; lets the nav avoid a
// flash of the wrong (anonymous) state on first paint.
const ready = ref(false);

export const isLoggedIn = computed(() => user.value !== null);

// Mirrors the backend role guards (`crate::auth`). These only drive UI
// visibility; the server enforces every check.
function hasAny(...roles: string[]): boolean {
  return user.value !== null && roles.some((r) => user.value!.roles.includes(r));
}
export const isAdmin = computed(() => hasAny('admin'));
export const canViewPending = computed(() => hasAny('admin', 'reviewer', 'approver'));
export const canEditPending = computed(() => hasAny('admin', 'reviewer'));
export const canApprove = computed(() => hasAny('admin', 'approver'));
export const canEditSets = computed(() => hasAny('admin', 'editor'));
export const canDeleteSets = computed(() => hasAny('admin', 'deleter'));

async function refresh(): Promise<void> {
  try {
    const res = await fetch('/auth/me', { credentials: 'same-origin' });
    user.value = res.ok ? ((await res.json()) as Me) : null;
  } catch {
    user.value = null;
  } finally {
    ready.value = true;
  }
}

async function logout(): Promise<void> {
  try {
    await fetch('/auth/logout', { method: 'POST', credentials: 'same-origin' });
  } finally {
    user.value = null;
    router.push('/');
  }
}

export function useAuth() {
  return {
    user,
    ready,
    isLoggedIn,
    isAdmin,
    canViewPending,
    canEditPending,
    canApprove,
    canEditSets,
    canDeleteSets,
    refresh,
    logout,
  };
}
