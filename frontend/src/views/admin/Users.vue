<script setup lang="ts">
import { onMounted, ref } from 'vue';
import { useToast } from 'primevue/usetoast';
import { useConfirm } from 'primevue/useconfirm';
import PageHeader from '../../components/PageHeader.vue';

interface User {
  sub: string;
  display_name: string | null;
  blocked: boolean;
  roles: string[];
  added_at: string | null;
  added_by: string | null;
}

const toast = useToast();
const confirm = useConfirm();

const users = ref<User[]>([]);
const loading = ref(false);

const newSub = ref('');
const newDisplay = ref('');
const newRoles = ref<string[]>(['reviewer']);

function fail(label: string, status: number) {
  toast.add({
    severity: 'error',
    summary: label,
    detail: `HTTP ${status}`,
    life: 4000,
  });
}

async function load() {
  loading.value = true;
  try {
    const res = await fetch('/api/admin/users', { credentials: 'same-origin' });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    users.value = await res.json();
  } catch (e) {
    toast.add({
      severity: 'error',
      summary: 'Could not load users',
      detail: String(e),
      life: 4000,
    });
  } finally {
    loading.value = false;
  }
}

async function addUser() {
  if (!newSub.value.trim()) return;
  const res = await fetch('/api/admin/users', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    credentials: 'same-origin',
    body: JSON.stringify({
      sub: newSub.value,
      display_name: newDisplay.value || null,
      roles: newRoles.value,
    }),
  });
  if (!res.ok) return fail('Add failed', res.status);
  toast.add({
    severity: 'success',
    summary: 'User added',
    detail: newSub.value,
    life: 3000,
  });
  newSub.value = '';
  newDisplay.value = '';
  newRoles.value = ['reviewer'];
  await load();
}

async function patchUser(
  u: User,
  patch: Partial<{ blocked: boolean; roles: string[] }>,
) {
  const res = await fetch(`/api/admin/users/${encodeURIComponent(u.sub)}`, {
    method: 'PATCH',
    headers: { 'content-type': 'application/json' },
    credentials: 'same-origin',
    body: JSON.stringify(patch),
  });
  if (!res.ok) return fail('Update failed', res.status);
  await load();
}

function deleteUser(u: User) {
  confirm.require({
    message: `Delete user ${u.sub}? This cannot be undone.`,
    header: 'Confirm delete',
    icon: 'pi pi-exclamation-triangle',
    rejectProps: { label: 'Cancel', severity: 'secondary', outlined: true },
    acceptProps: { label: 'Delete', severity: 'danger' },
    accept: async () => {
      const res = await fetch(
        `/api/admin/users/${encodeURIComponent(u.sub)}`,
        { method: 'DELETE', credentials: 'same-origin' },
      );
      if (!res.ok && res.status !== 204) return fail('Delete failed', res.status);
      toast.add({
        severity: 'success',
        summary: 'User deleted',
        detail: u.sub,
        life: 3000,
      });
      await load();
    },
  });
}

function toggleRole(u: User, role: string) {
  const roles = u.roles.includes(role)
    ? u.roles.filter((r) => r !== role)
    : [...u.roles, role];
  patchUser(u, { roles });
}

onMounted(load);
</script>

<template>
  <section>
    <PageHeader title="Users" subtitle="OIDC accounts and their roles" />

    <Card class="add-card">
      <template #title>Add user</template>
      <template #content>
        <form class="add" @submit.prevent="addUser">
          <InputText v-model="newSub" placeholder="sub (e.g. github:userslug)" />
          <InputText v-model="newDisplay" placeholder="Display name" />
          <div class="roles">
            <label
              ><Checkbox v-model="newRoles" value="admin" /> admin</label
            >
            <label
              ><Checkbox v-model="newRoles" value="reviewer" /> reviewer</label
            >
            <label
              ><Checkbox v-model="newRoles" value="unlimited" /> unlimited</label
            >
          </div>
          <Button type="submit" label="Add" icon="pi pi-user-plus" />
        </form>
      </template>
    </Card>

    <Card>
      <template #content>
        <div class="table-scroll">
          <DataTable :value="users" :loading="loading" data-key="sub">
            <template #empty>
              <span class="muted">No users yet.</span>
            </template>
            <Column field="sub" header="Sub" />
            <Column header="Display">
              <template #body="{ data }">{{ data.display_name ?? '—' }}</template>
            </Column>
            <Column header="Roles">
              <template #body="{ data }">
                <div class="roles">
                  <label>
                    <Checkbox
                      :model-value="data.roles.includes('admin')"
                      binary
                      @update:model-value="toggleRole(data, 'admin')"
                    />
                    admin
                  </label>
                  <label>
                    <Checkbox
                      :model-value="data.roles.includes('reviewer')"
                      binary
                      @update:model-value="toggleRole(data, 'reviewer')"
                    />
                    reviewer
                  </label>
                  <label>
                    <Checkbox
                      :model-value="data.roles.includes('unlimited')"
                      binary
                      @update:model-value="toggleRole(data, 'unlimited')"
                    />
                    unlimited
                  </label>
                </div>
              </template>
            </Column>
            <Column header="Status">
              <template #body="{ data }">
                <Tag
                  :value="data.blocked ? 'blocked' : 'active'"
                  :severity="data.blocked ? 'danger' : 'success'"
                />
              </template>
            </Column>
            <Column header="Actions">
              <template #body="{ data }">
                <div class="actions">
                  <Button
                    :label="data.blocked ? 'Unblock' : 'Block'"
                    :icon="data.blocked ? 'pi pi-lock-open' : 'pi pi-lock'"
                    size="small"
                    severity="secondary"
                    outlined
                    @click="patchUser(data, { blocked: !data.blocked })"
                  />
                  <Button
                    label="Delete"
                    icon="pi pi-trash"
                    size="small"
                    severity="danger"
                    text
                    @click="deleteUser(data)"
                  />
                </div>
              </template>
            </Column>
          </DataTable>
        </div>
      </template>
    </Card>
  </section>
</template>

<style scoped>
.add-card {
  margin-bottom: 1rem;
}
.add {
  display: flex;
  flex-wrap: wrap;
  gap: 0.75rem;
  align-items: center;
}
.roles {
  display: flex;
  gap: 1rem;
}
.roles label {
  display: inline-flex;
  align-items: center;
  gap: 0.4rem;
}
.actions {
  display: flex;
  gap: 0.5rem;
}
</style>
