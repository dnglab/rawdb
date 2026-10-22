<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { RouterView, useRouter } from 'vue-router';
import type { MenuItem } from 'primevue/menuitem';
import type Menu from 'primevue/menu';
import { useToast } from 'primevue/usetoast';
import { useAuth } from './auth';
import FeedIcon from './components/FeedIcon.vue';

const router = useRouter();
const toast = useToast();
const { user, ready, isLoggedIn, canViewPending, refresh, logout } = useAuth();

const feedMenu = ref<InstanceType<typeof Menu> | null>(null);
const feedItems: MenuItem[] = [
  { label: 'Published samples', url: '/api/feeds/published.atom' },
  { label: 'Pending uploads', url: '/api/feeds/pending.atom' },
];

async function copyFeed(path?: string) {
  if (!path) return;
  try {
    await navigator.clipboard.writeText(new URL(path, location.origin).href);
    toast.add({ severity: 'success', summary: 'Feed URL copied', life: 2000 });
  } catch {
    toast.add({ severity: 'warn', summary: 'Could not copy the feed URL', life: 3000 });
  }
}

onMounted(refresh);

const navItems = computed<MenuItem[]>(() => {
  const items: MenuItem[] = [
    { label: 'Browse', icon: 'pi pi-images', command: () => router.push('/browse') },
    { label: 'Upload', icon: 'pi pi-upload', command: () => router.push('/upload') },
  ];
  if (isLoggedIn.value && canViewPending.value) {
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
        <Button
          label="Feeds"
          severity="secondary"
          text
          aria-haspopup="true"
          aria-controls="feed-menu"
          @click="feedMenu?.toggle($event)"
        >
          <template #icon="{ class: iconClass }">
            <FeedIcon :class="iconClass" />
          </template>
        </Button>
        <Menu id="feed-menu" ref="feedMenu" :model="feedItems" popup>
          <template #item="{ item, props }">
            <div class="feed-item">
              <a
                :href="item.url"
                target="_blank"
                rel="noopener"
                v-bind="props.action"
                class="feed-link"
              >
                <FeedIcon />
                <span>{{ item.label }}</span>
              </a>
              <Button
                icon="pi pi-copy"
                text
                rounded
                size="small"
                aria-label="Copy feed URL"
                @click.stop="copyFeed(item.url)"
              />
            </div>
          </template>
        </Menu>
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
.feed-item {
  display: flex;
  align-items: center;
  gap: 0.25rem;
  padding-right: 0.25rem;
}
.feed-link {
  display: flex;
  flex: 1;
  align-items: center;
  gap: 0.5rem;
  padding: 0.5rem 0.75rem;
  color: inherit;
  text-decoration: none;
}
</style>
