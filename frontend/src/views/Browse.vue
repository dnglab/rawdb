<script setup lang="ts">
import PageHeader from '../components/PageHeader.vue';
import SampleBrowser from '../components/SampleBrowser.vue';
</script>

<template>
  <section>
    <PageHeader
      title="Browse sets"
      subtitle="Search and download community RAW sample sets"
    />
    <Card>
      <template #content>
        <SampleBrowser />
      </template>
    </Card>
  </section>
</template>
