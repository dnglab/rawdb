import { createRouter, createWebHistory, type RouteRecordRaw } from 'vue-router';

const routes: RouteRecordRaw[] = [
  { path: '/', component: () => import('./views/Home.vue') },
  { path: '/browse', component: () => import('./views/Browse.vue') },
  {
    path: '/sets/:maker/:model',
    component: () => import('./views/SetDetail.vue'),
    props: true,
  },
  { path: '/upload', component: () => import('./views/Upload.vue') },
  { path: '/login', component: () => import('./views/Login.vue') },
  { path: '/admin', component: () => import('./views/admin/Pending.vue') },
  {
    path: '/admin/pending/:upload_id',
    component: () => import('./views/admin/ReviewUpload.vue'),
    props: true,
  },
  {
    path: '/admin/sets/:maker/:model',
    component: () => import('./views/admin/SetEdit.vue'),
    props: true,
  },
  { path: '/admin/users', component: () => import('./views/admin/Users.vue') },
  // Catch-all: the backend serves index.html for unknown paths so deep links
  // work; Vue Router renders this for anything that matches no route above.
  {
    path: '/:pathMatch(.*)*',
    component: () => import('./views/NotFound.vue'),
  },
];

export const router = createRouter({
  history: createWebHistory(),
  routes,
});
