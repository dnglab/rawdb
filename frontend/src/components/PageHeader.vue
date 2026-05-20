<script setup lang="ts">
defineProps<{ title: string; subtitle?: string }>();
</script>

<template>
  <div class="page-header">
    <div class="ph-text">
      <h1>{{ title }}</h1>
      <p v-if="subtitle" class="muted">{{ subtitle }}</p>
    </div>
    <div class="ph-actions">
      <slot name="actions" />
    </div>
  </div>
</template>

<style scoped>
.page-header {
  display: flex;
  align-items: flex-end;
  justify-content: space-between;
  gap: 1rem;
  flex-wrap: wrap;
  margin-bottom: 1.25rem;
}
.ph-text h1 {
  margin: 0;
  font-size: 1.6rem;
  letter-spacing: -0.02em;
}
.ph-text p {
  margin: 0.25rem 0 0;
  font-size: 0.95rem;
}
.ph-actions {
  display: flex;
  gap: 0.5rem;
  align-items: center;
}
</style>
