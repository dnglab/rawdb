<script setup lang="ts">
import { onMounted, ref, watch } from 'vue';
import { useRouter } from 'vue-router';
import { useToast } from 'primevue/usetoast';
import { api, formatBytes, type SetDetail } from '../api';
import PageHeader from '../components/PageHeader.vue';
import { useAuth } from '../auth';

const props = defineProps<{ maker: string; model: string }>();
const router = useRouter();
const toast = useToast();
const { canReview } = useAuth();

// Tracks which file paths have an in-flight download preflight, so the
// row's button can show a brief busy state.
const downloading = ref<Set<string>>(new Set());

function humanDuration(secs: number): string {
  if (secs < 60) return `${secs} second${secs === 1 ? '' : 's'}`;
  const mins = Math.ceil(secs / 60);
  return `${mins} minute${mins === 1 ? '' : 's'}`;
}

// Downloads are rate limited server-side (per backend instance, per IP).
// We preflight with a redirect-manual fetch so a 429 can be surfaced as a
// toast instead of a full-page error. On success we navigate normally;
// the backend's same-path dedup grace means this second hit reuses the
// preflight's token rather than consuming another.
async function download(path: string) {
  if (!detail.value) return;
  const url = api.downloadUrl(detail.value.maker, detail.value.model, path);
  downloading.value.add(path);
  const ctrl = new AbortController();
  try {
    const res = await fetch(url, { redirect: 'manual', signal: ctrl.signal });
    if (res.status === 429) {
      let secs = 0;
      try {
        secs = (await res.json())?.retry_after_secs ?? 0;
      } catch {
        /* body may be empty */
      }
      if (!secs) {
        const ra = res.headers.get('retry-after');
        secs = ra ? parseInt(ra, 10) || 0 : 0;
      }
      toast.add({
        severity: 'warn',
        summary: 'Download limit reached',
        detail: secs
          ? `Too many downloads from your address. Try again in about ${humanDuration(secs)}.`
          : 'Too many downloads from your address. Please wait a little and try again.',
        life: 7000,
      });
      return;
    }
  } catch {
    /* network error during preflight — fall through and let the
       browser's own navigation surface whatever went wrong */
  } finally {
    // Stop the preflight body (relevant in streaming download mode);
    // harmless for the redirect (presigned) case.
    ctrl.abort();
    downloading.value.delete(path);
  }
  // Not rate limited — trigger the actual download.
  window.location.href = url;
}

const detail = ref<SetDetail | null>(null);
const err = ref<string | null>(null);
const loading = ref(false);

async function load() {
  err.value = null;
  detail.value = null;
  loading.value = true;
  try {
    detail.value = await api.setDetail(props.maker, props.model);
  } catch (e) {
    err.value = String(e);
  } finally {
    loading.value = false;
  }
}

onMounted(load);
watch(() => [props.maker, props.model], load);
</script>

<template>
  <section>
    <Message v-if="err" severity="error">{{ err }}</Message>

    <template v-else-if="loading || !detail">
      <Skeleton width="20rem" height="2rem" class="mb" />
      <Skeleton width="100%" height="12rem" />
    </template>

    <template v-else>
      <PageHeader :title="`${detail.maker} ${detail.model}`">
        <template #actions>
          <Tag
            v-if="detail.special"
            value="non-camera"
            severity="warn"
          />
          <Tag :value="detail.license" severity="info" />
          <Button
            v-if="canReview"
            label="Edit"
            icon="pi pi-pencil"
            size="small"
            severity="secondary"
            outlined
            @click="router.push(
              `/admin/sets/${encodeURIComponent(detail.maker)}/${encodeURIComponent(detail.model)}`,
            )"
          />
        </template>
      </PageHeader>

      <p class="meta muted">
        <span v-if="detail.uploaded_by">
          <i class="pi pi-user" /> {{ detail.uploaded_by }}
        </span>
        <span v-if="detail.uploaded_at">
          <i class="pi pi-calendar" /> {{ detail.uploaded_at }}
        </span>
      </p>
      <p v-if="detail.notes">{{ detail.notes }}</p>

      <Panel
        v-for="(files, category) in detail.categories"
        :key="category"
        :header="String(category)"
        toggleable
        class="cat"
      >
        <div class="table-scroll">
          <DataTable :value="files" data-key="path" row-hover>
            <Column header="File">
              <template #body="{ data }">
                <div>{{ data.path }}</div>
                <span v-if="data.tags.length" class="tags sm">
                  <Tag
                    v-for="t in data.tags"
                    :key="t"
                    :value="t"
                    severity="secondary"
                  />
                </span>
              </template>
            </Column>
            <Column header="Size">
              <template #body="{ data }">{{ formatBytes(data.size) }}</template>
            </Column>
            <Column field="license" header="License" />
            <Column header="">
              <template #body="{ data }">
                <Button
                  label="Download"
                  icon="pi pi-download"
                  size="small"
                  severity="secondary"
                  outlined
                  :loading="downloading.has(data.path)"
                  @click="download(data.path)"
                />
              </template>
            </Column>
          </DataTable>
        </div>
      </Panel>
    </template>
  </section>
</template>

<style scoped>
.mb {
  margin-bottom: 1rem;
}
.meta {
  display: flex;
  gap: 1.25rem;
  margin: 0.25rem 0 0.75rem;
  font-size: 0.9rem;
}
.tags {
  display: flex;
  flex-wrap: wrap;
  gap: 0.35rem;
  margin: 0.5rem 0 1rem;
}
.tags.sm {
  margin: 0.35rem 0 0;
}
.cat {
  margin-bottom: 1rem;
}
</style>
