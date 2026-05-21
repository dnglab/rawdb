<script setup lang="ts">
import { onMounted, reactive, ref, watch } from 'vue';
import { useRouter } from 'vue-router';
import type {
  DataTablePageEvent,
  DataTableSortEvent,
} from 'primevue/datatable';
import { api, formatBytes, type SearchParams, type SetSummary } from '../api';

const router = useRouter();

const filters = reactive<SearchParams>({
  q: '',
  maker: '',
  model: '',
  extension: '',
  tags: '',
  license: '',
  limit: 25,
  offset: 0,
});

const sets = ref<SetSummary[]>([]);
const total = ref(0);
const loading = ref(false);
const err = ref<string | null>(null);
const includeSpecial = ref(false);

// PrimeVue's lazy DataTable emits a sort event whose `sortField` matches
// the column's `field` prop (when set) or the `sortField` prop (when the
// column has a custom body without a field). `sortOrder` is +1 for asc,
// -1 for desc. We persist the active key in the same shape the backend
// accepts (asc/desc strings) so a refresh round-trips cleanly.
const sortField = ref<string | null>(null);
const sortOrder = ref<1 | -1>(1);

async function reload() {
  loading.value = true;
  err.value = null;
  try {
    const params: SearchParams = {};
    for (const [k, v] of Object.entries(filters)) {
      if (v === '' || v === undefined || v === null) continue;
      (params as Record<string, unknown>)[k] = v;
    }
    if (includeSpecial.value) params.include_special = '1';
    if (sortField.value) {
      params.sort = sortField.value;
      params.order = sortOrder.value === -1 ? 'desc' : 'asc';
    }
    const r = await api.listSets(params);
    sets.value = r.sets;
    total.value = r.total;
  } catch (e) {
    err.value = String(e);
  } finally {
    loading.value = false;
  }
}

onMounted(reload);

let debounce: ReturnType<typeof setTimeout> | null = null;
watch(
  () => [
    filters.q,
    filters.maker,
    filters.model,
    filters.extension,
    filters.tags,
    filters.license,
    includeSpecial.value,
  ],
  () => {
    if (debounce) clearTimeout(debounce);
    debounce = setTimeout(() => {
      filters.offset = 0;
      reload();
    }, 250);
  },
);

function onPage(e: DataTablePageEvent) {
  filters.limit = e.rows;
  filters.offset = e.first;
  reload();
}

function onSort(e: DataTableSortEvent) {
  // Clicking the same header a third time clears sort (sortField undefined).
  sortField.value = (e.sortField as string | null | undefined) ?? null;
  sortOrder.value = (e.sortOrder as 1 | -1 | null | undefined) === -1 ? -1 : 1;
  filters.offset = 0;
  reload();
}

function openSet(s: SetSummary) {
  router.push(
    `/sets/${encodeURIComponent(s.maker)}/${encodeURIComponent(s.model)}`,
  );
}
</script>

<template>
  <div class="browser">
    <div class="filters">
      <IconField class="grow">
        <InputIcon class="pi pi-search" />
        <InputText
          v-model="filters.q"
          placeholder="Search maker, model, notes, tags…"
          fluid
        />
      </IconField>
      <InputText v-model="filters.maker" placeholder="Maker" />
      <InputText v-model="filters.model" placeholder="Model" />
      <InputText v-model="filters.extension" placeholder="Ext (cr3)" />
      <InputText
        v-model="filters.tags"
        placeholder="Tags (comma-separated)"
      />
      <InputText v-model="filters.license" placeholder="License" />
    </div>

    <label class="special-toggle">
      <Checkbox v-model="includeSpecial" binary />
      Show non-camera samples
    </label>

    <Message v-if="err" severity="error" class="mt">{{ err }}</Message>

    <div class="table-scroll">
      <DataTable
        :value="sets"
        :loading="loading"
        lazy
        paginator
        :rows="filters.limit ?? 25"
        :first="filters.offset ?? 0"
        :total-records="total"
        :rows-per-page-options="[25, 50, 100]"
        :sort-field="sortField ?? undefined"
        :sort-order="sortOrder"
        :remove-sortable-sort="true"
        data-key="model"
        row-hover
        @page="onPage"
        @sort="onSort"
        @row-click="openSet($event.data as SetSummary)"
        class="mt"
      >
        <template #empty>
          <span class="muted">No sets match those filters.</span>
        </template>
        <Column field="maker" header="Maker" sortable />
        <Column field="model" header="Model" sortable>
          <template #body="{ data }">
            <RouterLink
              :to="`/sets/${encodeURIComponent(data.maker)}/${encodeURIComponent(data.model)}`"
              @click.stop
            >
              {{ data.model }}
            </RouterLink>
          </template>
        </Column>
        <Column field="file_count" header="Files" sortable>
          <template #body="{ data }">{{ data.file_count }}</template>
        </Column>
        <Column field="total_size" header="Size" sortable>
          <template #body="{ data }">{{ formatBytes(data.total_size) }}</template>
        </Column>
        <Column field="license" header="License" sortable />
        <Column field="tags" header="Tags" sortable>
          <template #body="{ data }">
            <span class="tags">
              <Tag
                v-for="t in data.tags"
                :key="t"
                :value="t"
                severity="secondary"
              />
            </span>
          </template>
        </Column>
      </DataTable>
    </div>
  </div>
</template>

<style scoped>
.filters {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(170px, 1fr));
  gap: 0.6rem;
  align-items: stretch;
}
.filters .grow {
  grid-column: 1 / -1;
}
.filters :deep(.p-inputtext),
.filters :deep(.p-select),
.filters :deep(.p-inputnumber) {
  width: 100%;
}
.mt {
  margin-top: 1rem;
}
.special-toggle {
  display: inline-flex;
  align-items: center;
  gap: 0.45rem;
  margin: 0.85rem 0 0;
  font-size: 0.9rem;
  cursor: pointer;
}
.tags {
  display: inline-flex;
  flex-wrap: wrap;
  gap: 0.3rem;
}
:deep(.p-datatable-tbody > tr) {
  cursor: pointer;
}
</style>
