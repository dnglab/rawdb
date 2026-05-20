<script setup lang="ts">
import { onMounted, ref } from 'vue';
import { useRouter } from 'vue-router';
import { api, type Stats } from '../api';
import SampleBrowser from '../components/SampleBrowser.vue';

const router = useRouter();
const stats = ref<Stats | null>(null);
const err = ref<string | null>(null);

onMounted(async () => {
  try {
    stats.value = await api.stats();
  } catch (e) {
    err.value = String(e);
  }
});
</script>

<template>
  <section>
    <div class="hero">
      <h1>RawDB</h1>
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
        <span class="stat-icon"><i class="pi pi-clock" /></span>
        <div>
          <div class="stat-value">{{ stats.pending }}</div>
          <div class="stat-label">Pending uploads</div>
        </div>
      </div>
    </div>

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
.scan-msg {
  margin-bottom: 1rem;
}
.cta {
  margin: 0.5rem 0 1.5rem;
}
</style>
