<script setup lang="ts">
import { onMounted, ref } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import { useAuth } from '../auth';

const route = useRoute();
const router = useRouter();
const { refresh } = useAuth();

const oidcEnabled = ref(false);
const githubEnabled = ref(false);
const passwordEnabled = ref(true);
const password = ref('');
const msg = ref<string | null>(null);
// Friendly explanation rendered when the SSO callback rejected the user
// (unregistered or blocked). The backend redirects here with
// `?error=...&sub=...` so we surface the reason and the canonical sub
// the operator needs to add to users.toml.
const ssoError = ref<{ severity: 'warn' | 'error'; text: string } | null>(null);
const busy = ref(false);

onMounted(async () => {
  const err = typeof route.query.error === 'string' ? route.query.error : null;
  const sub = typeof route.query.sub === 'string' ? route.query.sub : null;
  if (err === 'not_registered') {
    ssoError.value = {
      severity: 'warn',
      text: sub
        ? `Sign-in succeeded but ${sub} isn't a registered user. Ask an administrator to add this identity before trying again.`
        : `Sign-in succeeded but this identity isn't registered with RawDB. Ask an administrator to add it before trying again.`,
    };
  } else if (err === 'blocked') {
    ssoError.value = {
      severity: 'error',
      text: sub
        ? `Account ${sub} is currently blocked. Contact an administrator.`
        : `This account is currently blocked. Contact an administrator.`,
    };
  }

  try {
    const res = await fetch('/auth/methods');
    if (res.ok) {
      const j = (await res.json()) as {
        password: boolean;
        oidc: boolean;
        github: boolean;
      };
      passwordEnabled.value = j.password === true;
      oidcEnabled.value = j.oidc === true;
      githubEnabled.value = j.github === true;
    }
  } catch {
    /* leave defaults: password on, others off — fail-safe */
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
        <Message
          v-if="ssoError"
          :severity="ssoError.severity"
          :closable="false"
          class="mb"
        >
          {{ ssoError.text }}
        </Message>

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

        <template v-if="oidcEnabled || githubEnabled">
          <Divider v-if="passwordEnabled" align="center"
            ><span class="muted">or</span></Divider
          >
          <a v-if="oidcEnabled" href="/auth/oidc/start" class="sso">
            <Button
              label="Sign in with SSO"
              icon="pi pi-key"
              severity="secondary"
              outlined
              fluid
            />
          </a>
          <a v-if="githubEnabled" href="/auth/github/start" class="sso">
            <Button
              label="Sign in with GitHub"
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
.mb {
  margin-bottom: 1rem;
}
</style>
