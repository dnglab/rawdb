<script setup lang="ts">
import { onMounted, reactive, ref } from 'vue';
import { useRouter } from 'vue-router';
import { api } from '../api';
import PageHeader from '../components/PageHeader.vue';
import TagInput from '../components/TagInput.vue';

// All uploads land under the raw_modes category; it is no longer
// user-editable.
const CATEGORY = 'raw_modes';

interface FileRow {
  file: File;
  // May be pre-filled with a normalized bit-depth tag from the filename.
  tags: string[];
  // Optional per-file note.
  notesText: string;
}

const router = useRouter();

const set = reactive({
  maker: '',
  model: '',
  license: 'CC0-1.0',
  notes: '',
  uploaded_by: '',
});

const rows = ref<FileRow[]>([]);
const phase = ref<'form' | 'working' | 'done'>('form');
const status = ref('');
const err = ref<string | null>(null);
const fileInput = ref<HTMLInputElement | null>(null);

const suggestedTags = ref<string[]>([]);

// Maker picker: known makers from existing sets, plus free text.
const knownMakers = ref<string[]>([]);
const makerSuggestions = ref<string[]>([]);

function onMakerComplete(e: { query: string }) {
  const q = e.query.trim().toLowerCase();
  makerSuggestions.value = q
    ? knownMakers.value.filter((m) => m.toLowerCase().includes(q))
    : [...knownMakers.value];
}

// Model picker: known (maker, model) pairs from existing sets, suggestions
// scoped to the chosen maker when one is set, plus free text.
const knownModels = ref<{ maker: string; model: string }[]>([]);
const modelSuggestions = ref<string[]>([]);

function onModelComplete(e: { query: string }) {
  const q = e.query.trim().toLowerCase();
  const maker = set.maker.trim().toLowerCase();
  const pool = knownModels.value
    .filter((p) => !maker || p.maker.toLowerCase() === maker)
    .map((p) => p.model);
  const uniq = [...new Set(pool)];
  modelSuggestions.value = q
    ? uniq.filter((m) => m.toLowerCase().includes(q))
    : uniq;
}

onMounted(async () => {
  try {
    // Suggestion chips = operator-curated tags (always shown) plus the
    // top-10 most-used tags from the data. `/api/tags` is sorted by
    // descending count, so taking `.slice(0, 10)` is the head of that
    // distribution; curated tags carry `suggested: true` and may have
    // count 0.
    const counts = (await api.tags()).tags;
    const topUsed = counts.slice(0, 10).map((t) => t.tag);
    const curated = counts.filter((t) => t.suggested).map((t) => t.tag);
    // Curated first (so admins control the prominent slots), then most-used,
    // de-duplicated case-insensitively.
    const seen = new Set<string>();
    suggestedTags.value = [...curated, ...topUsed].filter((t) => {
      const k = t.toLowerCase();
      if (seen.has(k)) return false;
      seen.add(k);
      return true;
    });
  } catch {
    /* suggestions are optional */
  }
  try {
    knownMakers.value = (await api.makers()).makers;
  } catch {
    /* maker list is optional */
  }
  try {
    knownModels.value = (await api.models()).models;
  } catch {
    /* model list is optional */
  }
});

// Apply a suggested tag to every selected file.
function applyTagToAll(tag: string) {
  for (const r of rows.value) {
    if (!r.tags.includes(tag)) r.tags.push(tag);
  }
}

// Detect a bit-depth hint in a filename (14bit, 14-bit, 14_bits, "14 bits",
// 16BIT, …) and normalize to "<n>bits".
function bitTagFromName(name: string): string | null {
  const m = name.match(/(\d{1,2})[\s._-]*bits?\b/i);
  return m ? `${m[1]}bits` : null;
}

function onPick(e: Event) {
  const input = e.target as HTMLInputElement;
  const picked = Array.from(input.files ?? []);
  const existing = new Set(rows.value.map((r) => r.file.name));
  for (const file of picked) {
    if (existing.has(file.name)) continue; // dedupe by name, keep prior edits
    existing.add(file.name);
    const bitTag = bitTagFromName(file.name);
    rows.value.push({
      file,
      tags: bitTag ? [bitTag] : [],
      notesText: '',
    });
  }
  // Allow re-picking the same file later (e.g. after removing it).
  input.value = '';
}

function removeRow(i: number) {
  rows.value.splice(i, 1);
}

// TOML basic-string escaping (\, ", and the common control chars).
function q(s: string): string {
  const e = s
    .replace(/\\/g, '\\\\')
    .replace(/"/g, '\\"')
    .replace(/\n/g, '\\n')
    .replace(/\t/g, '\\t');
  return `"${e}"`;
}

function arr(items: string[]): string {
  return `[${items.map(q).join(', ')}]`;
}

function cleanTags(items: string[]): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const raw of items) {
    const t = raw.trim();
    if (t && !seen.has(t)) {
      seen.add(t);
      out.push(t);
    }
  }
  return out;
}

function relPath(r: FileRow): string {
  return `${CATEGORY}/${r.file.name}`;
}

function buildMetaToml(): string {
  const lines: string[] = ['[set]'];
  lines.push(`maker = ${q(set.maker.trim())}`);
  lines.push(`model = ${q(set.model.trim())}`);
  lines.push(`license = ${q(set.license.trim() || 'CC0-1.0')}`);
  if (set.uploaded_by.trim())
    lines.push(`uploaded_by = ${q(set.uploaded_by.trim())}`);
  if (set.notes.trim()) lines.push(`notes = ${q(set.notes.trim())}`);

  for (const r of rows.value) {
    lines.push('', '[[files]]');
    lines.push(`path = ${q(relPath(r))}`);
    const fileTags = cleanTags(r.tags);
    if (fileTags.length) lines.push(`tags = ${arr(fileTags)}`);
    if (r.notesText.trim()) lines.push(`notes = ${q(r.notesText.trim())}`);
  }
  return lines.join('\n') + '\n';
}

function validate(): string | null {
  if (!set.maker.trim() || !set.model.trim())
    return 'Maker and model are required.';
  if (rows.value.length === 0) return 'Select at least one file.';
  const names = rows.value.map((r) => r.file.name);
  if (new Set(names).size !== names.length)
    return 'Duplicate file names — each file must have a distinct name.';
  return null;
}

async function putFile(url: string, file: File, sameOrigin: boolean) {
  const res = await fetch(url, {
    method: 'PUT',
    body: file,
    ...(sameOrigin ? { credentials: 'same-origin' as const } : {}),
  });
  if (!res.ok) throw new Error(`upload of ${file.name} failed (${res.status})`);
}

async function submit() {
  err.value = null;
  const v = validate();
  if (v) {
    err.value = v;
    return;
  }
  phase.value = 'working';
  try {
    const maker = set.maker.trim();
    const model = set.model.trim();

    status.value = 'Requesting upload slot…';
    const begin = await api.uploadBegin({
      maker,
      model,
      files: rows.value.map((r) => ({ path: relPath(r) })),
    });

    for (let i = 0; i < rows.value.length; i++) {
      const r = rows.value[i];
      const path = relPath(r);
      status.value = `Uploading ${r.file.name} (${i + 1}/${rows.value.length})…`;
      const presigned = begin.urls?.[path];
      if (presigned) {
        await putFile(presigned, r.file, false);
      } else if (begin.stream_base) {
        const encoded = path.split('/').map(encodeURIComponent).join('/');
        await putFile(`${begin.stream_base}/${encoded}`, r.file, true);
      } else {
        throw new Error(`no upload URL for ${path}`);
      }
    }

    status.value = 'Finalizing…';
    await api.uploadComplete({
      maker,
      model,
      upload_id: begin.upload_id,
      meta_toml: buildMetaToml(),
    });

    phase.value = 'done';
  } catch (e) {
    err.value = String(e);
    phase.value = 'form';
  }
}
</script>

<template>
  <section>
    <PageHeader
      title="Upload a sample set"
      subtitle="Contribute RAW files for decoder testing"
    />

    <Card v-if="phase === 'done'">
      <template #content>
        <Message severity="success" :closable="false">
          Upload complete. It will appear in the review queue shortly.
        </Message>
        <div class="done-actions">
          <Button
            label="Back to browse"
            icon="pi pi-images"
            @click="router.push('/browse')"
          />
        </div>
      </template>
    </Card>

    <Card v-else>
      <template #content>
        <Message v-if="err" severity="error" class="mb">{{ err }}</Message>

        <form @submit.prevent="submit">
          <fieldset :disabled="phase === 'working'" class="fs">
            <div class="grid">
              <label class="fld">
                <span>Maker *</span>
                <AutoComplete
                  v-model="set.maker"
                  :suggestions="makerSuggestions"
                  dropdown
                  complete-on-focus
                  placeholder="Choose or type a maker"
                  fluid
                  @complete="onMakerComplete"
                />
              </label>
              <label class="fld">
                <span>Model *</span>
                <AutoComplete
                  v-model="set.model"
                  :suggestions="modelSuggestions"
                  dropdown
                  complete-on-focus
                  placeholder="Choose or type a model"
                  fluid
                  @complete="onModelComplete"
                />
              </label>
              <label class="fld">
                <span>License</span>
                <Select
                  v-model="set.license"
                  :options="['CC0-1.0']"
                  fluid
                />
              </label>
              <label class="fld">
                <span>Uploaded by</span>
                <InputText v-model="set.uploaded_by" placeholder="optional" />
              </label>
              <label class="fld wide">
                <span>Notes</span>
                <Textarea v-model="set.notes" rows="2" auto-resize />
              </label>
            </div>

            <div class="pick">
              <input
                ref="fileInput"
                type="file"
                multiple
                class="hidden-input"
                @change="onPick"
              />
              <Button
                type="button"
                label="Add files"
                icon="pi pi-plus"
                severity="secondary"
                outlined
                @click="fileInput?.click()"
              />
              <span class="muted">{{ rows.length }} file(s) selected</span>
            </div>

            <div
              v-if="rows.length && suggestedTags.length"
              class="suggested mt"
            >
              <span class="muted">Apply suggested tag to all files:</span>
              <Tag
                v-for="t in suggestedTags"
                :key="t"
                :value="t"
                severity="secondary"
                class="sg-tag"
                @click="applyTagToAll(t)"
              />
            </div>

            <div v-if="rows.length" class="table-scroll mt">
              <DataTable :value="rows" data-key="file.name">
                <Column header="File">
                  <template #body="{ data }">{{ data.file.name }}</template>
                </Column>
                <Column header="Tags">
                  <template #body="{ data }">
                    <TagInput v-model="data.tags" placeholder="add tag" />
                  </template>
                </Column>
                <Column header="Note">
                  <template #body="{ data }">
                    <InputText
                      v-model="data.notesText"
                      placeholder="optional"
                      fluid
                    />
                  </template>
                </Column>
                <Column header="">
                  <template #body="{ index }">
                    <Button
                      type="button"
                      icon="pi pi-times"
                      severity="danger"
                      text
                      rounded
                      @click="removeRow(index)"
                    />
                  </template>
                </Column>
              </DataTable>
            </div>

            <div class="submit">
              <Button
                type="submit"
                label="Upload"
                icon="pi pi-upload"
                :loading="phase === 'working'"
              />
              <span v-if="phase === 'working'" class="muted">{{ status }}</span>
            </div>
            <ProgressBar
              v-if="phase === 'working'"
              mode="indeterminate"
              class="mt"
              style="height: 6px"
            />
          </fieldset>
        </form>
      </template>
    </Card>
  </section>
</template>

<style scoped>
.fs {
  border: none;
  padding: 0;
  margin: 0;
}
.grid {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 0.85rem 1rem;
  margin-bottom: 1rem;
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
.suggested {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.35rem;
  margin-top: 0.4rem;
}
.sg-tag {
  cursor: pointer;
}
.sg-tag:hover {
  filter: brightness(0.95);
}
.pick {
  display: flex;
  align-items: center;
  gap: 0.75rem;
}
.hidden-input {
  display: none;
}
.mt {
  margin-top: 1rem;
}
.mb {
  margin-bottom: 1rem;
}
.submit {
  display: flex;
  align-items: center;
  gap: 0.85rem;
  margin-top: 1rem;
}
.done-actions {
  margin-top: 1rem;
}
@media (max-width: 640px) {
  .grid {
    grid-template-columns: 1fr;
  }
}
</style>
