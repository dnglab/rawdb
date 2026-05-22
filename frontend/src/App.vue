<script setup lang="ts">
import { computed, onMounted } from 'vue';
import { RouterView, useRouter } from 'vue-router';
import type { MenuItem } from 'primevue/menuitem';
import { useAuth } from './auth';

const router = useRouter();
const { user, ready, isLoggedIn, canReview, refresh, logout } = useAuth();

onMounted(refresh);

const navItems = computed<MenuItem[]>(() => {
  const items: MenuItem[] = [
    { label: 'Browse', icon: 'pi pi-images', command: () => router.push('/browse') },
    { label: 'Upload', icon: 'pi pi-upload', command: () => router.push('/upload') },
  ];
  if (isLoggedIn.value && canReview.value) {
    items.push({
      label: 'Admin',
      icon: 'pi pi-shield',
      command: () => router.push('/admin'),
    });
  }
  return items;
});

const initial = computed(() => {
  const u = user.value;
  if (!u) return '?';
  const s = u.display_name || u.sub || '?';
  return s.replace(/^[^:]*:/, '').charAt(0).toUpperCase() || '?';
});
</script>

<template>
  <Menubar :model="navItems" class="topbar">
    <template #start>
      <a class="brand" @click="router.push('/')">
        <i class="pi pi-database" />
        <span>RawDB</span>
      </a>
    </template>
    <template #end>
      <div class="bar-end" v-if="ready">
        <template v-if="isLoggedIn">
          <Avatar
            :label="initial"
            shape="circle"
            size="normal"
            class="me"
            title="Account"
            @click="router.push('/account')"
          />
          <Button
            label="Sign out"
            icon="pi pi-sign-out"
            severity="secondary"
            text
            @click="logout"
          />
        </template>
        <Button
          v-else
          label="Sign in"
          icon="pi pi-sign-in"
          severity="secondary"
          outlined
          @click="router.push('/login')"
        />
      </div>
    </template>
  </Menubar>

  <main class="page">
    <RouterView />
  </main>

  <footer class="app-footer">
    RawDB — community camera RAW samples ·
    <a
      href="https://github.com/dnglab/dnglab"
      target="_blank"
      rel="noopener"
      >dnglab</a
    >
    · MIT OR Apache-2.0
  </footer>

  <Toast />
  <ConfirmDialog />
</template>

<style scoped>
.topbar {
  position: sticky;
  top: 0;
  z-index: 50;
  border-radius: 0;
  border-left: none;
  border-right: none;
  border-top: none;
  padding-inline: 1.25rem;
}
.brand {
  display: inline-flex;
  align-items: center;
  gap: 0.5rem;
  font-weight: 700;
  font-size: 1.15rem;
  color: var(--p-primary-600);
  cursor: pointer;
  margin-right: 1.5rem;
  user-select: none;
}
.brand:hover {
  text-decoration: none;
}
.bar-end {
  display: flex;
  align-items: center;
  gap: 0.6rem;
}
.bar-end .me {
  background: var(--p-primary-100);
  color: var(--p-primary-700);
  font-weight: 600;
  cursor: pointer;
}
</style>
