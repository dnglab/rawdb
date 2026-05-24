<script setup lang="ts">
import { computed, onMounted, reactive, ref } from 'vue';
import { useRouter } from 'vue-router';
import { useToast } from 'primevue/usetoast';
import { useConfirm } from 'primevue/useconfirm';
import { api, formatBytes, type PendingEdit } from '../../api';
import PageHeader from '../../components/PageHeader.vue';
import TagInput from '../../components/TagInput.vue';

const props = defineProps<{ upload_id: string }>();
const router = useRouter();
const toast = useToast();
const confirm = useConfirm();

interface EditFileRow {
  old_path: string;
  prefix: string;
  filename: string;
  ext: string;
  size: number;
  tags: string[];
  notes: string;
  selected: boolean;
  /// Uploader-supplied SHA-256, displayed as the muted third line.
  sha256: string | null;
  /// Latest verify result for this row: 'ok' / 'mismatch' / 'missing' / null.
  verify: 'ok' | 'mismatch' | 'missing' | null;
}

const setForm = reactive({
  maker: '',
  model: '',
  license: 'CC0 1.0',
  uploaded_by: '',
  uploaded_at: '' as string | null,
  notes: '',
  special: false,
});
const files = ref<EditFileRow[]>([]);

const loaded = ref(false);
const loading = ref(false);
const err = ref<string | null>(null);
const busy = ref(false);
const verifying = ref(false);
// Latest verify result summary; populated after the user clicks "Verify".
const verifyResult = ref<{
  ok: boolean;
  total: number;
  verified: number;
  mismatched: number;
  missing: number;
} | null>(null);
const conflict = ref<'refuse' | 'merge' | 'replace'>('refuse');

const conflictOptions = [
  { label: 'Refuse if exists', value: 'refuse' },
  { label: 'Merge into existing', value: 'merge' },
  { label: 'Replace existing', value: 'replace' },
];

const allSelected = computed({
  get: () => files.value.length > 0 && files.value.every((f) => f.selected),
  set: (v: boolean) => files.value.forEach((f) => (f.selected = v)),
});
const selectedCount = computed(
  () => files.value.filter((f) => f.selected).length,
);

function splitPath(path: string): { prefix: string; filename: string } {
  const i = path.lastIndexOf('/');
  return i < 0
    ? { prefix: '', filename: path }
    : { prefix: path.slice(0, i), filename: path.slice(i + 1) };
}

function curPath(r: EditFileRow): string {
  return `${r.prefix.trim().replace(/\/+$/, '')}/${r.filename.trim()}`;
}

async function load() {
  loading.value = true;
  err.value = null;
  try {
    const d = await api.adminPendingDetail(props.upload_id);
    setForm.maker = d.maker;
    setForm.model = d.model;
    setForm.license = d.license;
    setForm.uploaded_by = d.uploaded_by ?? '';
    setForm.uploaded_at = d.uploaded_at;
    setForm.notes = d.notes ?? '';
    setForm.special = d.special;
    files.value = d.files.map((f) => {
      const { prefix, filename } = splitPath(f.path);
      return {
        old_path: f.path,
        prefix,
        filename,
        ext: f.extension,
        size: f.size,
        tags: [...f.tags],
        notes: f.notes ?? '',
        selected: true,
        sha256: f.sha256 ?? null,
        verify: null,
      };
    });
    loaded.value = true;
  } catch (e) {
    err.value = String(e);
  } finally {
    loading.value = false;
  }
}

function downloadHref(r: EditFileRow): string {
  return api.adminPendingDownloadUrl(props.upload_id, r.old_path);
}

function buildEdit(): PendingEdit {
  return {
    maker: setForm.maker.trim(),
    model: setForm.model.trim(),
    license: setForm.license.trim() || 'CC0 1.0',
    notes: setForm.notes.trim() ? setForm.notes.trim() : null,
    uploaded_by: setForm.uploaded_by.trim() ? setForm.uploaded_by.trim() : null,
    special: setForm.special,
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
    await api.adminPendingEdit(props.upload_id, buildEdit());
    toast.add({
      severity: 'success',
      summary: 'Saved',
      detail: 'Pending upload updated',
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

async function approve() {
  if (selectedCount.value === 0) {
    toast.add({
      severity: 'warn',
      summary: 'Nothing selected',
      detail: 'Check at least one file to approve.',
      life: 4000,
    });
    return;
  }
  const selectedPaths = files.value
    .filter((f) => f.selected)
    .map((f) => curPath(f));
  busy.value = true;
  try {
    // Persist edits first (also performs any prefix/filename renames), then
    // promote only the checked files; unchecked files are discarded.
    await api.adminPendingEdit(props.upload_id, buildEdit());
    await api.adminApprove(props.upload_id, conflict.value, selectedPaths);
    toast.add({
      severity: 'success',
      summary: 'Approved',
      detail: `${setForm.maker} ${setForm.model}`,
      life: 3000,
    });
    router.push('/admin');
  } catch (e) {
    toast.add({
      severity: 'error',
      summary: 'Approve failed',
      detail: String(e),
      life: 6000,
    });
  } finally {
    busy.value = false;
  }
}

function reject() {
  confirm.require({
    message: `Reject this upload? The entire set is deleted (checkmarks are ignored).`,
    header: 'Confirm reject',
    icon: 'pi pi-exclamation-triangle',
    rejectProps: { label: 'Cancel', severity: 'secondary', outlined: true },
    acceptProps: { label: 'Reject', severity: 'danger' },
    accept: async () => {
      busy.value = true;
      try {
        await api.adminReject(props.upload_id);
        toast.add({
          severity: 'success',
          summary: 'Rejected',
          detail: props.upload_id,
          life: 3000,
        });
        router.push('/admin');
      } catch (e) {
        toast.add({
          severity: 'error',
          summary: 'Reject failed',
          detail: String(e),
          life: 6000,
        });
      } finally {
        busy.value = false;
      }
    },
  });
}

async function verifyChecksums() {
  verifying.value = true;
  verifyResult.value = null;
  // Wipe stale per-row badges before re-running.
  for (const r of files.value) r.verify = null;
  try {
    const res = await api.adminVerifyPending(props.upload_id);
    verifyResult.value = {
      ok: res.ok,
      total: res.total,
      verified: res.verified,
      mismatched: res.mismatched,
      missing: res.missing,
    };
    const byPath = new Map(res.files.map((f) => [f.path, f]));
    for (const r of files.value) {
      const v = byPath.get(r.old_path);
      r.verify = v
        ? (v.status as 'ok' | 'mismatch' | 'missing')
        : null;
    }
    if (res.ok) {
      toast.add({
        severity: 'success',
        summary: 'Checksums verified',
        detail:
          res.missing > 0
            ? `${res.verified}/${res.total} matched; ${res.missing} without a claim.`
            : `${res.verified}/${res.total} matched.`,
        life: 4000,
      });
    } else {
      toast.add({
        severity: 'error',
        summary: 'Checksum mismatch',
        detail: `${res.mismatched} of ${res.total} file(s) failed verification.`,
        life: 6000,
      });
    }
  } catch (e) {
    toast.add({
      severity: 'error',
      summary: 'Verify failed',
      detail: String(e),
      life: 6000,
    });
  } finally {
    verifying.value = false;
  }
}

async function downloadAll() {
  const sel = files.value.filter((f) => f.selected);
  if (sel.length === 0) {
    toast.add({
      severity: 'warn',
      summary: 'Nothing selected',
      detail: 'Check the files you want to download.',
      life: 4000,
    });
    return;
  }
  for (const r of sel) {
    const a = document.createElement('a');
    a.href = downloadHref(r);
    a.rel = 'noopener';
    document.body.appendChild(a);
    a.click();
    a.remove();
    // Stagger so the browser doesn't suppress rapid navigations.
    await new Promise((res) => setTimeout(res, 400));
  }
}

onMounted(load);
</script>

<template>
  <section>
    <PageHeader title="Review upload" :subtitle="upload_id">
      <template #actions>
        <Button
          label="Back"
          icon="pi pi-arrow-left"
          severity="secondary"
          text
          @click="router.push('/admin')"
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
              <span>Maker *</span>
              <InputText v-model="setForm.maker" />
            </label>
            <label class="fld">
              <span>Model *</span>
              <InputText v-model="setForm.model" />
            </label>
            <label class="fld">
              <span>License</span>
              <InputText v-model="setForm.license" />
            </label>
            <label class="fld">
              <span>Uploaded by</span>
              <InputText v-model="setForm.uploaded_by" placeholder="" />
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
        <template #title>
          <div class="files-head">
            <span>Files</span>
            <label class="sel-all">
              <Checkbox v-model="allSelected" binary />
              Select all ({{ selectedCount }}/{{ files.length }})
            </label>
            <Button
              label="Download selected"
              icon="pi pi-download"
              size="small"
              severity="secondary"
              outlined
              :disabled="selectedCount === 0"
              @click="downloadAll"
            />
            <Button
              label="Verify checksums"
              icon="pi pi-shield"
              size="small"
              severity="secondary"
              outlined
              :loading="verifying"
              @click="verifyChecksums"
            />
          </div>
        </template>
        <template #content>
          <p class="muted hint">
            Renaming the prefix or filename moves the file in storage on save.
            On approve, only checked files are kept; unchecked files are
            deleted. Reject deletes the whole set regardless of checkmarks.
          </p>
          <Message
            v-if="verifyResult"
            :severity="verifyResult.ok ? 'success' : 'error'"
            :closable="false"
            class="mb"
          >
            {{ verifyResult.verified }}/{{ verifyResult.total }} matched
            <template v-if="verifyResult.mismatched > 0">
              · {{ verifyResult.mismatched }} mismatch{{ verifyResult.mismatched === 1 ? '' : 'es' }}
            </template>
            <template v-if="verifyResult.missing > 0">
              · {{ verifyResult.missing }} without claim
            </template>
          </Message>
          <div v-for="(r, i) in files" :key="r.old_path" class="frow">
            <div class="line1">
              <Checkbox v-model="r.selected" binary />
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
              <a :href="downloadHref(r)" target="_blank" rel="noopener">
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
                <InputText v-model="r.notes" placeholder="" fluid />
              </label>
            </div>
            <div v-if="r.sha256 || r.verify" class="hash-row">
              <span v-if="r.sha256" class="hash" :title="r.sha256">
                <span class="hash-prefix"># sha256</span>
                {{ r.sha256 }}
              </span>
              <span v-else class="hash">
                <span class="hash-prefix"># sha256</span>
                no claim
              </span>
              <Tag
                v-if="r.verify === 'ok'"
                value="verified"
                severity="success"
                icon="pi pi-check"
              />
              <Tag
                v-else-if="r.verify === 'mismatch'"
                value="mismatch"
                severity="danger"
                icon="pi pi-times"
              />
              <Tag
                v-else-if="r.verify === 'missing'"
                value="no claim"
                severity="warn"
                icon="pi pi-question"
              />
            </div>
            <Divider v-if="i < files.length - 1" />
          </div>
        </template>
      </Card>

      <Card>
        <template #content>
          <div class="decide">
            <Button
              label="Save"
              icon="pi pi-save"
              :loading="busy"
              @click="save"
            />
            <span class="sep" />
            <span class="lbl">On conflict</span>
            <Select
              v-model="conflict"
              :options="conflictOptions"
              option-label="label"
              option-value="value"
            />
            <Button
              label="Approve selected"
              icon="pi pi-check"
              severity="success"
              :loading="busy"
              @click="approve"
            />
            <Button
              label="Reject"
              icon="pi pi-times"
              severity="danger"
              outlined
              :disabled="busy"
              @click="reject"
            />
          </div>
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
.files-head {
  display: flex;
  align-items: center;
  gap: 1rem;
  flex-wrap: wrap;
}
.sel-all {
  display: inline-flex;
  align-items: center;
  gap: 0.4rem;
  font-size: 0.9rem;
  font-weight: 400;
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
.decide {
  display: flex;
  align-items: center;
  gap: 0.75rem;
  flex-wrap: wrap;
}
.decide .lbl {
  font-weight: 600;
}
.decide .sep {
  flex: 1;
}
.hash-row {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 0.5rem;
  margin-top: 0.3rem;
}
.hash {
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
.mb {
  margin-bottom: 0.75rem;
}
@media (max-width: 720px) {
  .grid {
    grid-template-columns: 1fr;
  }
}
</style>
