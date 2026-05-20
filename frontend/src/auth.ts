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
export const canReview = computed(
  () =>
    user.value !== null &&
    (user.value.roles.includes('admin') || user.value.roles.includes('reviewer')),
);

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
  return { user, ready, isLoggedIn, canReview, refresh, logout };
}
