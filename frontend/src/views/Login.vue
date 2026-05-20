<script setup lang="ts">
import { onMounted, ref } from 'vue';
import { useRouter } from 'vue-router';
import { useAuth } from '../auth';

const router = useRouter();
const { refresh } = useAuth();

const oidcEnabled = ref(false);
const passwordEnabled = ref(true);
const password = ref('');
const msg = ref<string | null>(null);
const busy = ref(false);

onMounted(async () => {
  try {
    const res = await fetch('/auth/methods');
    if (res.ok) {
      const j = (await res.json()) as { password: boolean; oidc: boolean };
      passwordEnabled.value = j.password === true;
      oidcEnabled.value = j.oidc === true;
    }
  } catch {
    /* leave defaults: password on, oidc off — matches the fail-safe path */
  }
});

async function loginPassword() {
  msg.value = null;
  busy.value = true;
  try {
    const res = await fetch('/auth/login', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ password: password.value }),
    });
    if (res.ok) {
      await refresh();
      router.push('/admin');
    } else {
      msg.value = `Login failed (${res.status})`;
    }
  } finally {
    busy.value = false;
  }
}
</script>

<template>
  <div class="login-wrap">
    <Card class="login-card">
      <template #title>Sign in</template>
      <template #subtitle>Reviewer / admin access</template>
      <template #content>
        <form
          v-if="passwordEnabled"
          class="form"
          @submit.prevent="loginPassword"
        >
          <Password
            v-model="password"
            placeholder="Admin password"
            :feedback="false"
            toggle-mask
            fluid
            input-id="pw"
            autocomplete="current-password"
          />
          <Button
            type="submit"
            label="Sign in"
            icon="pi pi-sign-in"
            :loading="busy"
            fluid
          />
        </form>

        <template v-if="oidcEnabled">
          <Divider v-if="passwordEnabled" align="center"
            ><span class="muted">or</span></Divider
          >
          <a href="/auth/oidc/start" class="sso">
            <Button
              label="Sign in with SSO"
              icon="pi pi-github"
              severity="secondary"
              outlined
              fluid
            />
          </a>
        </template>

        <Message v-if="msg" severity="error" class="mt">{{ msg }}</Message>
      </template>
    </Card>
  </div>
</template>

<style scoped>
.login-wrap {
  display: flex;
  justify-content: center;
  align-items: flex-start;
  padding-top: 3rem;
}
.login-card {
  width: 100%;
  max-width: 380px;
}
.form {
  display: flex;
  flex-direction: column;
  gap: 0.85rem;
}
.sso {
  display: block;
}
.sso:hover {
  text-decoration: none;
}
.mt {
  margin-top: 1rem;
}
</style>
