<script setup lang="ts">
import { onMounted, ref, watch } from 'vue';
import { useRouter } from 'vue-router';
import { useToast } from 'primevue/usetoast';
import { api, formatBytes, type SetDetail } from '../api';
import PageHeader from '../components/PageHeader.vue';
import { useAuth } from '../auth';
// Pre-rendered to HTML at build time by the markdown plugin in
// vite.config.ts — see license-info.inc.md for the source.
import licenseInfoHtml from '../license-info.inc.md';

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
        <!-- Desktop / tablet: the original DataTable. -->
        <div class="files-desktop table-scroll">
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
                <div v-if="data.sha256" class="hash" :title="data.sha256">
                  <span class="hash-prefix"># sha256</span>
                  {{ data.sha256 }}
                </div>
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

        <!-- Mobile: a card list per file. Long filenames and sha256
             hashes get `.touch-scroll`, which on a narrow viewport
             becomes a single nowrap line you swipe sideways — so the
             content never breaks the page width. The download button
             sits in the bottom-right of each card's meta row. -->
        <div class="files-mobile">
          <div v-for="data in files" :key="data.path" class="file-card">
            <div class="file-card-path touch-scroll" :title="data.path">
              {{ data.path }}
            </div>
            <span v-if="data.tags.length" class="tags sm">
              <Tag
                v-for="t in data.tags"
                :key="t"
                :value="t"
                severity="secondary"
              />
            </span>
            <div
              v-if="data.sha256"
              class="hash touch-scroll"
              :title="data.sha256"
            >
              <span class="hash-prefix"># sha256</span>
              {{ data.sha256 }}
            </div>
            <div class="file-card-row">
              <span class="muted">{{ formatBytes(data.size) }}</span>
              <span class="muted">·</span>
              <span class="muted">{{ data.license }}</span>
              <Button
                label="Download"
                icon="pi pi-download"
                size="small"
                severity="secondary"
                outlined
                class="file-card-dl"
                :loading="downloading.has(data.path)"
                @click="download(data.path)"
              />
            </div>
          </div>
        </div>
      </Panel>

      <Panel
        header="License info"
        toggleable
        :collapsed="true"
        class="cat license-info"
      >
        <div class="license-info-body" v-html="licenseInfoHtml" />
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
.hash {
  margin-top: 0.3rem;
  font-family: monospace;
  font-size: 0.78rem;
  color: var(--p-text-muted-color);
  overflow-wrap: anywhere;
  line-height: 1.3;
}
.hash .hash-prefix {
  margin-right: 0.4rem;
  opacity: 0.7;
}
.cat {
  margin-bottom: 1rem;
}
/* Reset the per-paragraph margins so the rendered markdown sits
   comfortably inside the Panel without an oversized top gap. */
.license-info-body :deep(h2),
.license-info-body :deep(h3) {
  margin-top: 0.75rem;
  margin-bottom: 0.5rem;
}
.license-info-body :deep(h2):first-child,
.license-info-body :deep(h3):first-child {
  margin-top: 0;
}
.license-info-body :deep(p),
.license-info-body :deep(ul) {
  margin: 0.5rem 0;
}

/* ---- responsive: swap DataTable ↔ card list at 720px ----------------- */

/* Default (desktop): show the DataTable, hide the mobile cards. */
.files-mobile {
  display: none;
}

@media (max-width: 720px) {
  .files-desktop {
    display: none;
  }
  .files-mobile {
    display: block;
  }

  /* Each card stacks: path / tags / hash / meta-row. The meta-row pins
     the Download button to the right via margin-left: auto on it. */
  .file-card {
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    padding: 0.75rem 0;
    border-top: 1px solid var(--p-content-border-color, rgba(0, 0, 0, 0.08));
  }
  .file-card:first-child {
    border-top: none;
    padding-top: 0;
  }
  .file-card-path {
    font-weight: 500;
  }
  .file-card-row {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.5rem;
    margin-top: 0.25rem;
  }
  .file-card-row .muted {
    font-size: 0.85rem;
  }
  .file-card-dl {
    margin-left: auto;
  }

  /* Long filenames + sha256 hashes: one nowrap line that the user can
     swipe sideways (overflow-x: auto + -webkit touch behavior). The
     class is a no-op on desktop, so wrapping behavior is preserved
     there. */
  .touch-scroll {
    overflow-x: auto;
    white-space: nowrap;
    -webkit-overflow-scrolling: touch;
    /* Slim scrollbar so it doesn't dominate the card visually. */
    scrollbar-width: thin;
  }
  .touch-scroll::-webkit-scrollbar {
    height: 3px;
  }
  /* Tag chips already wrap; keep them readable on narrow screens. */
  .tags.sm {
    margin: 0;
  }
}
</style>
