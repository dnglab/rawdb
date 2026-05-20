<script setup lang="ts">
import { ref } from 'vue';

const props = defineProps<{
  modelValue: string[];
  placeholder?: string;
}>();
const emit = defineEmits<{
  'update:modelValue': [string[]];
}>();

const draft = ref('');

function commit(raw: string) {
  // Allow pasting "a, b, c" — split on commas too.
  const parts = raw
    .split(',')
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
  if (parts.length === 0) return;
  const next = [...props.modelValue];
  for (const p of parts) {
    if (!next.includes(p)) next.push(p);
  }
  emit('update:modelValue', next);
  draft.value = '';
}

function onKeydown(e: KeyboardEvent) {
  if (e.key === 'Enter' || e.key === ',') {
    e.preventDefault();
    commit(draft.value);
  } else if (e.key === 'Backspace' && draft.value === '' && props.modelValue.length) {
    emit('update:modelValue', props.modelValue.slice(0, -1));
  }
}

function remove(i: number) {
  const next = [...props.modelValue];
  next.splice(i, 1);
  emit('update:modelValue', next);
}
</script>

<template>
  <div class="tag-input" @click="($refs.inp as HTMLInputElement)?.focus()">
    <Chip
      v-for="(t, i) in modelValue"
      :key="`${t}-${i}`"
      :label="t"
      removable
      @remove="remove(i)"
    />
    <input
      ref="inp"
      v-model="draft"
      class="tag-draft"
      :placeholder="modelValue.length ? '' : (placeholder ?? 'Add tag, press Enter')"
      @keydown="onKeydown"
      @blur="commit(draft)"
    />
  </div>
</template>

<style scoped>
.tag-input {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.35rem;
  width: 100%;
  min-height: 2.5rem;
  padding: 0.35rem 0.5rem;
  border: 1px solid var(--p-inputtext-border-color, var(--p-surface-300));
  border-radius: var(--p-inputtext-border-radius, 6px);
  background: var(--p-inputtext-background, var(--p-surface-0));
  cursor: text;
}
.tag-input:focus-within {
  border-color: var(--p-primary-500);
  outline: 0;
}
.tag-draft {
  flex: 1;
  min-width: 6rem;
  border: none;
  outline: none;
  background: transparent;
  color: inherit;
  font: inherit;
  padding: 0.15rem;
}
</style>
