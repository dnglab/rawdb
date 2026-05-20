<script setup lang="ts">
import { useRoute, useRouter } from 'vue-router';

const route = useRoute();
const router = useRouter();
</script>

<template>
  <div class="nf">
    <div class="code">404</div>
    <h1>Page not found</h1>
    <p class="muted">
      Nothing matches <code>{{ route.fullPath }}</code>.
    </p>
    <div class="actions">
      <Button
        label="Back to home"
        icon="pi pi-home"
        @click="router.push('/')"
      />
      <Button
        label="Browse sets"
        icon="pi pi-images"
        severity="secondary"
        outlined
        @click="router.push('/browse')"
      />
    </div>
  </div>
</template>

<style scoped>
.nf {
  text-align: center;
  padding: 4rem 1rem;
}
.code {
  font-size: 5rem;
  font-weight: 800;
  line-height: 1;
  color: var(--p-primary-500);
  letter-spacing: -0.04em;
}
.nf h1 {
  margin: 0.5rem 0 0.25rem;
}
code {
  background: var(--p-surface-200);
  padding: 0.1rem 0.4rem;
  border-radius: 0.25rem;
}
.actions {
  display: flex;
  gap: 0.75rem;
  justify-content: center;
  margin-top: 1.5rem;
}
</style>
