<script setup lang="ts">
import { onMounted, reactive, ref } from 'vue';
import { useRouter } from 'vue-router';
import { useToast } from 'primevue/usetoast';
import { api, formatBytes, type SetEdit } from '../../api';
import PageHeader from '../../components/PageHeader.vue';
import TagInput from '../../components/TagInput.vue';

const props = defineProps<{ maker: string; model: string }>();
const router = useRouter();
const toast = useToast();

interface Row {
  old_path: string;
  prefix: string;
  filename: string;
  ext: string;
  size: number;
  tags: string[];
  notes: string;
  sha256: string | null;
}

const setForm = reactive({
  license: 'CC0-1.0',
  uploaded_by: '',
  uploaded_at: '' as string | null,
  notes: '',
  special: false,
});
const files = ref<Row[]>([]);
const loaded = ref(false);
const loading = ref(false);
const err = ref<string | null>(null);
const busy = ref(false);

function splitPath(path: string): { prefix: string; filename: string } {
  const i = path.lastIndexOf('/');
  return i < 0
    ? { prefix: '', filename: path }
    : { prefix: path.slice(0, i), filename: path.slice(i + 1) };
}
function curPath(r: Row): string {
  return `${r.prefix.trim().replace(/\/+$/, '')}/${r.filename.trim()}`;
}

async function load() {
  loading.value = true;
  err.value = null;
  try {
    const d = await api.setDetail(props.maker, props.model);
    setForm.license = d.license;
    setForm.uploaded_by = d.uploaded_by ?? '';
    setForm.uploaded_at = d.uploaded_at;
    setForm.notes = d.notes ?? '';
    setForm.special = d.special;
    files.value = Object.values(d.categories)
      .flat()
      .map((f) => {
        const { prefix, filename } = splitPath(f.path);
        return {
          old_path: f.path,
          prefix,
          filename,
          ext: f.extension,
          size: f.size,
          tags: [...f.tags],
          notes: f.notes ?? '',
          sha256: f.sha256 ?? null,
        };
      });
    loaded.value = true;
  } catch (e) {
    err.value = String(e);
  } finally {
    loading.value = false;
  }
}

function buildEdit(): SetEdit {
  return {
    license: setForm.license.trim() || 'CC0-1.0',
    special: setForm.special,
    notes: setForm.notes.trim() ? setForm.notes.trim() : null,
    uploaded_by: setForm.uploaded_by.trim() ? setForm.uploaded_by.trim() : null,
    files: files.value.map((r) => ({
      old_path: r.old_path,
      path: curPath(r),
      tags: r.tags,
      notes: r.notes.trim() ? r.notes.trim() : null,
      license: null,
    })),
  };
}

async function save() {
  busy.value = true;
  try {
    await api.adminSetEdit(props.maker, props.model, buildEdit());
    toast.add({
      severity: 'success',
      summary: 'Saved',
      detail: `${props.maker} ${props.model}`,
      life: 3000,
    });
    await load();
  } catch (e) {
    toast.add({
      severity: 'error',
      summary: 'Save failed',
      detail: String(e),
      life: 6000,
    });
  } finally {
    busy.value = false;
  }
}

onMounted(load);
</script>

<template>
  <section>
    <PageHeader
      :title="`Edit: ${maker} / ${model}`"
      subtitle="Approved set — maker/model are fixed (set identity)"
    >
      <template #actions>
        <Button
          label="Back"
          icon="pi pi-arrow-left"
          severity="secondary"
          text
          @click="router.push(
            `/sets/${encodeURIComponent(maker)}/${encodeURIComponent(model)}`,
          )"
        />
      </template>
    </PageHeader>

    <Message v-if="err" severity="error">{{ err }}</Message>
    <template v-if="loading || !loaded">
      <Skeleton width="100%" height="10rem" />
    </template>

    <template v-else>
      <Card class="mb">
        <template #title>Set metadata</template>
        <template #content>
          <div class="grid">
            <label class="fld">
              <span>License</span>
              <Select
                v-model="setForm.license"
                :options="['CC0-1.0']"
                fluid
              />
            </label>
            <label class="fld">
              <span>Uploaded by</span>
              <InputText v-model="setForm.uploaded_by" placeholder="optional" />
            </label>
            <label class="fld">
              <span>Uploaded at</span>
              <InputText :model-value="setForm.uploaded_at ?? '—'" disabled />
            </label>
            <label class="fld wide chk">
              <Checkbox v-model="setForm.special" binary />
              <span>Non-camera sample set (hidden from default browsing)</span>
            </label>
            <label class="fld wide">
              <span>Notes</span>
              <Textarea v-model="setForm.notes" rows="2" auto-resize />
            </label>
          </div>
        </template>
      </Card>

      <Card class="mb">
        <template #title>Files</template>
        <template #content>
          <p class="muted hint">
            Renaming the prefix or filename moves the file in storage on save.
          </p>
          <div v-for="(r, i) in files" :key="r.old_path" class="frow">
            <div class="line1">
              <label class="mini">
                <span>Prefix</span>
                <InputText v-model="r.prefix" />
              </label>
              <label class="mini grow">
                <span>Filename</span>
                <InputText v-model="r.filename" fluid />
              </label>
              <div class="mini">
                <span>Ext</span>
                <div class="ro">{{ r.ext || '—' }}</div>
              </div>
              <div class="mini">
                <span>Size</span>
                <div class="ro">{{ formatBytes(r.size) }}</div>
              </div>
              <a
                :href="api.downloadUrl(maker, model, r.old_path)"
                target="_blank"
                rel="noopener"
              >
                <Button
                  icon="pi pi-download"
                  size="small"
                  severity="secondary"
                  outlined
                />
              </a>
            </div>
            <div class="line2">
              <label class="mini full">
                <span>Tags</span>
                <TagInput v-model="r.tags" placeholder="add tag" />
              </label>
            </div>
            <div class="line3">
              <label class="mini full">
                <span>Note</span>
                <InputText v-model="r.notes" placeholder="optional" fluid />
              </label>
            </div>
            <div v-if="r.sha256" class="hash" :title="r.sha256">
              <i class="pi pi-hashtag" /> {{ r.sha256 }}
            </div>
            <Divider v-if="i < files.length - 1" />
          </div>
        </template>
      </Card>

      <Card>
        <template #content>
          <Button
            label="Save"
            icon="pi pi-save"
            :loading="busy"
            @click="save"
          />
        </template>
      </Card>
    </template>
  </section>
</template>

<style scoped>
.mb {
  margin-bottom: 1rem;
}
.grid {
  display: grid;
  grid-template-columns: 1fr 1fr 1fr;
  gap: 0.85rem 1rem;
}
.fld {
  display: flex;
  flex-direction: column;
  gap: 0.3rem;
  font-size: 0.85rem;
}
.fld span {
  color: var(--p-text-muted-color);
}
.fld :deep(.p-inputtext),
.fld :deep(textarea) {
  width: 100%;
}
.wide {
  grid-column: 1 / -1;
}
.fld.chk {
  flex-direction: row;
  align-items: center;
  gap: 0.5rem;
}
.hint {
  margin: 0 0 1rem;
  font-size: 0.85rem;
}
.frow {
  margin-bottom: 0.5rem;
}
.line1 {
  display: flex;
  align-items: flex-end;
  gap: 0.75rem;
  flex-wrap: wrap;
}
.line2,
.line3 {
  margin-top: 0.5rem;
}
.mini {
  display: flex;
  flex-direction: column;
  gap: 0.25rem;
  font-size: 0.8rem;
}
.mini > span {
  color: var(--p-text-muted-color);
}
.mini.grow {
  flex: 1;
  min-width: 12rem;
}
.mini.full {
  width: 100%;
}
.ro {
  padding: 0.4rem 0;
  white-space: nowrap;
}
.hash {
  margin-top: 0.5rem;
  font-family: monospace;
  font-size: 0.78rem;
  color: var(--p-text-muted-color);
  overflow-wrap: anywhere;
  line-height: 1.3;
}
.hash .pi-hashtag {
  font-size: 0.72rem;
  margin-right: 0.2rem;
  opacity: 0.7;
}
@media (max-width: 720px) {
  .grid {
    grid-template-columns: 1fr;
  }
}
</style>
