<script setup lang="ts">
import { onMounted, ref } from 'vue';
import { useRouter } from 'vue-router';
import { api, type Stats } from '../api';
import SampleBrowser from '../components/SampleBrowser.vue';

const router = useRouter();
const version = __RAWDB_VERSION__;
const stats = ref<Stats | null>(null);
const err = ref<string | null>(null);

const pendingOpen = ref(false);
const pendingList = ref<{ maker: string; model: string }[]>([]);
const pendingLoading = ref(false);
const pendingErr = ref<string | null>(null);

onMounted(async () => {
  try {
    stats.value = await api.stats();
  } catch (e) {
    err.value = String(e);
  }
});

// Re-fetched on every open so the list reflects the current queue.
async function openPending() {
  pendingOpen.value = true;
  pendingLoading.value = true;
  pendingErr.value = null;
  try {
    pendingList.value = (await api.pending()).models;
  } catch (e) {
    pendingErr.value = String(e);
  } finally {
    pendingLoading.value = false;
  }
}
</script>

<template>
  <section>
    <div class="hero">
      <div class="hero-title">
        <h1>RawDB</h1>
        <Tag v-if="version" :value="version" title="Version" class="version" />
      </div>
      <p>Community-shared camera RAW sample files for decoder testing.</p>
    </div>

    <Message v-if="err" severity="error">Could not load stats: {{ err }}</Message>

    <div v-else-if="stats" class="stat-row">
      <div class="stat-card">
        <span class="stat-icon"><i class="pi pi-camera" /></span>
        <div>
          <div class="stat-value">{{ stats.models }}</div>
          <div class="stat-label">Supported camera models</div>
        </div>
      </div>
      <div class="stat-card">
        <span class="stat-icon"><i class="pi pi-cog" /></span>
        <div>
          <div class="stat-value">{{ stats.special }}</div>
          <div class="stat-label">Non-camera sets</div>
        </div>
      </div>
      <div class="stat-card">
        <span class="stat-icon"><i class="pi pi-file" /></span>
        <div>
          <div class="stat-value">{{ stats.files }}</div>
          <div class="stat-label">Sample files</div>
        </div>
      </div>
      <div class="stat-card">
        <span class="stat-icon"><i class="pi pi-clock" /></span>
        <div>
          <div class="stat-value">{{ stats.pending }}</div>
          <div class="stat-label">Pending uploads</div>
        </div>
        <Button
          v-if="stats.pending > 0"
          icon="pi pi-arrow-right"
          text
          rounded
          aria-label="Show pending uploads"
          class="stat-action"
          @click="openPending"
        />
      </div>
    </div>

    <Dialog
      v-model:visible="pendingOpen"
      modal
      header="Pending uploads"
      :style="{ width: '28rem' }"
    >
      <p class="muted pending-hint">
        Files in pending uploads are not publicly available until they have
        been reviewed and approved.
      </p>
      <ProgressSpinner v-if="pendingLoading" class="pending-spinner" />
      <Message v-else-if="pendingErr" severity="error">
        Could not load pending uploads: {{ pendingErr }}
      </Message>
      <span v-else-if="pendingList.length === 0" class="muted">
        No pending uploads.
      </span>
      <ul v-else class="pending-list">
        <li v-for="p in pendingList" :key="`${p.maker}/${p.model}`">
          <strong>{{ p.maker }}</strong> {{ p.model }}
        </li>
      </ul>
    </Dialog>

    <Message
      v-if="stats && !stats.ready"
      severity="info"
      :closable="false"
      class="scan-msg"
    >
      Initial scan in progress — listings may be incomplete for a moment.
    </Message>

    <div class="cta">
      <Button
        label="Upload a set"
        icon="pi pi-upload"
        @click="router.push('/upload')"
      />
    </div>

    <Card>
      <template #title>Browse samples</template>
      <template #content>
        <SampleBrowser />
      </template>
    </Card>
  </section>
</template>

<style scoped>
.hero-title {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 0.75rem;
  margin-bottom: 0.4rem;
}
.hero-title h1 {
  margin: 0;
}
/* White with primary text, at 60% of the theme's tag size
   (0.875rem font, 0.25rem 0.5rem padding). */
.version {
  background: var(--p-surface-0);
  color: var(--p-primary-color);
  font-family: monospace;
  font-size: 0.525rem;
  font-weight: 500;
  padding: 0.15rem 0.3rem;
}
.scan-msg {
  margin-bottom: 1rem;
}
.cta {
  margin: 0.5rem 0 1.5rem;
}
.stat-action {
  margin-left: auto;
}
.pending-hint {
  margin: 0 0 1rem;
  font-size: 0.9rem;
}
.pending-spinner {
  display: block;
  width: 2rem;
  height: 2rem;
}
.pending-list {
  margin: 0;
  padding-left: 1.25rem;
  line-height: 1.7;
}
</style>
