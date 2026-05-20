<script setup lang="ts">
import { onMounted, ref } from 'vue';
import { useRouter } from 'vue-router';
import { api, type PendingRow } from '../../api';
import PageHeader from '../../components/PageHeader.vue';

const router = useRouter();
const rows = ref<PendingRow[]>([]);
const loading = ref(false);
const err = ref<string | null>(null);
const denied = ref(false);

async function load() {
  loading.value = true;
  err.value = null;
  denied.value = false;
  try {
    rows.value = await api.adminPending();
  } catch (e) {
    const m = String(e);
    if (m.includes('401') || m.includes('403')) denied.value = true;
    else err.value = m;
  } finally {
    loading.value = false;
  }
}

function review(r: PendingRow) {
  router.push(`/admin/pending/${encodeURIComponent(r.upload_id)}`);
}

onMounted(load);
</script>

<template>
  <section>
    <PageHeader
      title="Pending uploads"
      subtitle="Review and approve contributed sample sets"
    >
      <template #actions>
        <Button
          label="Manage users"
          icon="pi pi-users"
          severity="secondary"
          outlined
          @click="router.push('/admin/users')"
        />
      </template>
    </PageHeader>

    <Message v-if="denied" severity="warn">
      Not authorized. <RouterLink to="/login">Sign in</RouterLink> with a
      reviewer or admin account.
    </Message>
    <Message v-else-if="err" severity="error">{{ err }}</Message>

    <Card v-else>
      <template #content>
        <div class="table-scroll">
          <DataTable
            :value="rows"
            :loading="loading"
            data-key="upload_id"
            row-hover
            paginator
            :rows="20"
            @row-click="review($event.data as PendingRow)"
          >
            <template #empty>
              <span class="muted">No pending uploads.</span>
            </template>
            <Column field="maker" header="Maker" sortable />
            <Column field="model" header="Model" sortable />
            <Column header="Upload">
              <template #body="{ data }">
                <RouterLink
                  :to="`/admin/pending/${encodeURIComponent(data.upload_id)}`"
                  @click.stop
                >
                  {{ data.upload_id }}
                </RouterLink>
              </template>
            </Column>
            <Column field="license" header="License" />
            <Column header="Uploaded">
              <template #body="{ data }">{{ data.uploaded_at ?? '—' }}</template>
            </Column>
            <Column header="By">
              <template #body="{ data }">{{ data.uploaded_by ?? '—' }}</template>
            </Column>
          </DataTable>
        </div>
      </template>
    </Card>
  </section>
</template>

<style scoped>
:deep(.p-datatable-tbody > tr) {
  cursor: pointer;
}
</style>
