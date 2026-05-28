<script setup lang="ts">
import { onMounted, ref } from 'vue';
import { useRouter } from 'vue-router';
import { useToast } from 'primevue/usetoast';
import { useConfirm } from 'primevue/useconfirm';
import { api } from '../api';
import { useAuth } from '../auth';
import PageHeader from '../components/PageHeader.vue';

const router = useRouter();
const toast = useToast();
const confirm = useConfirm();
const { user, ready, isLoggedIn } = useAuth();

const loading = ref(true);
const eligible = ref(false);
const hasKey = ref(false);
const busy = ref(false);
// The plaintext key, held in memory only right after generation.
const freshKey = ref<string | null>(null);

async function loadStatus() {
  loading.value = true;
  try {
    const s = await api.apiKeyStatus();
    eligible.value = s.eligible;
    hasKey.value = s.has_key;
  } catch (e) {
    toast.add({
      severity: 'error',
      summary: 'Could not load API key status',
      detail: String(e),
      life: 4000,
    });
  } finally {
    loading.value = false;
  }
}

onMounted(async () => {
  // The page is for the signed-in user; bounce anonymous visitors.
  if (ready.value && !isLoggedIn.value) {
    router.replace('/login');
    return;
  }
  await loadStatus();
});

async function generate() {
  busy.value = true;
  try {
    const { api_key } = await api.apiKeyCreate();
    freshKey.value = api_key;
    hasKey.value = true;
    toast.add({
      severity: 'success',
      summary: 'API key generated',
      detail: 'Copy it now — it cannot be shown again.',
      life: 5000,
    });
  } catch (e) {
    toast.add({
      severity: 'error',
      summary: 'Generation failed',
      detail: String(e),
      life: 4000,
    });
  } finally {
    busy.value = false;
  }
}

function confirmGenerate() {
  // Regenerating invalidates the previous key.
  if (!hasKey.value) {
    generate();
    return;
  }
  confirm.require({
    message:
      'You already have an API key. Generating a new one immediately ' +
      'invalidates the old key. Continue?',
    header: 'Regenerate API key',
    icon: 'pi pi-exclamation-triangle',
    rejectProps: { label: 'Cancel', severity: 'secondary', outlined: true },
    acceptProps: { label: 'Regenerate' },
    accept: generate,
  });
}

function revoke() {
  confirm.require({
    message: 'Revoke your API key? Any client using it will stop working.',
    header: 'Revoke API key',
    icon: 'pi pi-exclamation-triangle',
    rejectProps: { label: 'Cancel', severity: 'secondary', outlined: true },
    acceptProps: { label: 'Revoke', severity: 'danger' },
    accept: async () => {
      busy.value = true;
      try {
        await api.apiKeyDelete();
        hasKey.value = false;
        freshKey.value = null;
        toast.add({
          severity: 'success',
          summary: 'API key revoked',
          life: 3000,
        });
      } catch (e) {
        toast.add({
          severity: 'error',
          summary: 'Revoke failed',
          detail: String(e),
          life: 4000,
        });
      } finally {
        busy.value = false;
      }
    },
  });
}

async function copyKey() {
  if (!freshKey.value) return;
  try {
    await navigator.clipboard.writeText(freshKey.value);
    toast.add({ severity: 'success', summary: 'Copied', life: 2000 });
  } catch {
    /* clipboard blocked — the field is selectable as a fallback */
  }
}
</script>

<template>
  <section>
    <PageHeader title="Account" :subtitle="user?.sub ?? ''" />

    <Card v-if="user" class="mb">
      <template #title>Profile</template>
      <template #content>
        <dl class="profile">
          <dt>Subject</dt>
          <dd>{{ user.sub }}</dd>
          <dt>Display name</dt>
          <dd>{{ user.display_name ?? '—' }}</dd>
          <dt>Sign-in</dt>
          <dd>{{ user.source }}</dd>
          <dt>Roles</dt>
          <dd>
            <span v-if="user.roles.length" class="roles">
              <Tag
                v-for="r in user.roles"
                :key="r"
                :value="r"
                severity="secondary"
              />
            </span>
            <span v-else class="muted">none</span>
          </dd>
        </dl>
      </template>
    </Card>

    <Card>
      <template #title>API key</template>
      <template #content>
        <p class="muted desc">
          A personal API key bypasses the per-IP download rate limit and
          unlocks the bulk <code>/api/export</code> endpoint. Send it as the
          <code>X-API-Key</code> request header. One key per account.
        </p>

        <ProgressSpinner v-if="loading" style="width: 2rem; height: 2rem" />

        <template v-else>
          <Message
            v-if="!eligible"
            severity="info"
            :closable="false"
          >
            API keys require the <strong>apiservice</strong> role. Ask an
            administrator to grant it.
          </Message>

          <template v-else>
            <Message
              v-if="freshKey"
              severity="warn"
              :closable="false"
              class="mb"
            >
              Copy this key now — it is shown only once and cannot be
              retrieved again.
            </Message>

            <div v-if="freshKey" class="key-row mb">
              <InputText
                :model-value="freshKey"
                readonly
                class="key-field"
                @focus="(e: FocusEvent) => (e.target as HTMLInputElement).select()"
              />
              <Button
                icon="pi pi-copy"
                label="Copy"
                severity="secondary"
                outlined
                @click="copyKey"
              />
            </div>

            <p v-else-if="hasKey" class="muted mb">
              An API key is active on this account. Its value can't be shown
              again — regenerate if you've lost it.
            </p>
            <p v-else class="muted mb">No API key yet.</p>

            <div class="actions">
              <Button
                :label="hasKey ? 'Regenerate key' : 'Generate key'"
                icon="pi pi-key"
                :loading="busy"
                @click="confirmGenerate"
              />
              <Button
                v-if="hasKey"
                label="Revoke"
                icon="pi pi-trash"
                severity="danger"
                outlined
                :disabled="busy"
                @click="revoke"
              />
            </div>
          </template>
        </template>
      </template>
    </Card>
  </section>
</template>

<style scoped>
.mb {
  margin-bottom: 1rem;
}
.desc {
  margin-top: 0;
}
.profile {
  display: grid;
  grid-template-columns: max-content 1fr;
  gap: 0.4rem 1.25rem;
  margin: 0;
}
.profile dt {
  color: var(--p-text-muted-color);
  font-size: 0.9rem;
}
.profile dd {
  margin: 0;
}
.roles {
  display: inline-flex;
  flex-wrap: wrap;
  gap: 0.35rem;
}
.key-row {
  display: flex;
  gap: 0.5rem;
  align-items: center;
}
.key-field {
  flex: 1;
  font-family: monospace;
}
.actions {
  display: flex;
  gap: 0.6rem;
}
</style>
