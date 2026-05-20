// Typed client for the RawDB JSON API. All paths are relative — the dev
// server proxies them to the backend on :8080 and prod serves them from
// the same origin.

export interface SetSummary {
  maker: string;
  model: string;
  license: string;
  notes: string | null;
  uploaded_at: string | null;
  uploaded_by: string | null;
  file_count: number;
  total_size: number;
  special: boolean;
  tags: string[];
}

export interface ListResponse {
  sets: SetSummary[];
  total: number;
  limit: number;
  offset: number;
}

export interface FileEnvelope {
  path: string;
  category: string;
  extension: string;
  size: number;
  license: string;
  notes: string | null;
  tags: string[];
}

export interface SetDetail {
  maker: string;
  model: string;
  license: string;
  notes: string | null;
  uploaded_at: string | null;
  uploaded_by: string | null;
  special: boolean;
  categories: Record<string, FileEnvelope[]>;
}

export interface Stats {
  models: number;
  special: number;
  pending: number;
  last_full_scan_at: string | null;
  ready: boolean;
}

export interface SearchParams {
  q?: string;
  maker?: string;
  model?: string;
  license?: string;
  extension?: string;
  /** Comma-separated; every tag must be present on the set. */
  tags?: string;
  /** Include non-camera ("special") sets. Omitted/false hides them. */
  include_special?: string;
  limit?: number;
  offset?: number;
}

export interface Me {
  sub: string;
  source: string;
  roles: string[];
  display_name: string | null;
}

export interface BeginFile {
  path: string;
}

export interface BeginResponse {
  upload_id: string;
  mode: string;
  urls?: Record<string, string>;
  stream_base?: string;
}

export interface PendingRow {
  maker: string;
  model: string;
  upload_id: string;
  license: string;
  notes: string | null;
  uploaded_at: string | null;
  uploaded_by: string | null;
}

export interface PendingFile {
  path: string;
  category: string;
  extension: string;
  size: number;
  license: string | null;
  notes: string | null;
  tags: string[];
}

export interface PendingDetail {
  maker: string;
  model: string;
  license: string;
  notes: string | null;
  uploaded_at: string | null;
  uploaded_by: string | null;
  special: boolean;
  files: PendingFile[];
}

function qs(params: Record<string, unknown>): string {
  const usp = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v === undefined || v === null || v === '') continue;
    usp.set(k, String(v));
  }
  const s = usp.toString();
  return s ? `?${s}` : '';
}

// Build an Error that includes the server's `{ "error": "..." }` reason
// (axum's AppError renders that), falling back to the status code.
async function errorFromResponse(path: string, res: Response): Promise<Error> {
  let detail = '';
  try {
    const body = await res.clone().json();
    if (body && typeof body.error === 'string') detail = body.error;
  } catch {
    try {
      detail = (await res.text()).trim();
    } catch {
      /* ignore */
    }
  }
  return new Error(
    detail
      ? `${detail} (${res.status})`
      : `${path} returned ${res.status}`,
  );
}

async function fetchJSON<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, { credentials: 'same-origin', ...init });
  if (!res.ok) throw await errorFromResponse(path, res);
  return (await res.json()) as T;
}

async function postJSON<T>(path: string, body: unknown): Promise<T> {
  return fetchJSON<T>(path, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
}

// POST with no request/response JSON body (204/201/empty). Throws on non-2xx.
async function postNoBody(path: string): Promise<void> {
  const res = await fetch(path, { method: 'POST', credentials: 'same-origin' });
  if (!res.ok) throw await errorFromResponse(path, res);
}

// POST a JSON body to an endpoint that replies 201/204 with no JSON body.
async function postJSONNoBody(path: string, body: unknown): Promise<void> {
  const res = await fetch(path, {
    method: 'POST',
    credentials: 'same-origin',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (!res.ok) throw await errorFromResponse(path, res);
}

// PUT a JSON body to an endpoint that replies 200/204 with no JSON body.
async function putJSONNoBody(path: string, body: unknown): Promise<void> {
  const res = await fetch(path, {
    method: 'PUT',
    credentials: 'same-origin',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (!res.ok) throw await errorFromResponse(path, res);
}

export interface PendingEditFile {
  old_path: string;
  path: string;
  tags: string[];
  notes: string | null;
  license: string | null;
}

export interface PendingEdit {
  maker: string;
  model: string;
  license: string;
  notes: string | null;
  uploaded_by: string | null;
  special: boolean;
  files: PendingEditFile[];
}

// Editing an already-approved set: maker/model are the set identity (path)
// and are not editable here.
export interface SetEdit {
  license: string;
  special: boolean;
  notes: string | null;
  uploaded_by: string | null;
  files: PendingEditFile[];
}

const enc = encodeURIComponent;

export const api = {
  stats: () => fetchJSON<Stats>('/api/stats'),
  listSets: (params: SearchParams = {}) =>
    fetchJSON<ListResponse>(`/api/sets${qs(params as Record<string, unknown>)}`),
  setDetail: (maker: string, model: string) =>
    fetchJSON<SetDetail>(
      `/api/sets/${encodeURIComponent(maker)}/${encodeURIComponent(model)}`,
    ),
  downloadUrl: (maker: string, model: string, path: string): string => {
    const parts = path.split('/').map(encodeURIComponent).join('/');
    return `/api/download/${encodeURIComponent(maker)}/${encodeURIComponent(model)}/${parts}`;
  },
  tags: () =>
    fetchJSON<{
      tags: { tag: string; count: number; suggested: boolean }[];
    }>('/api/tags'),
  makers: () => fetchJSON<{ makers: string[] }>('/api/makers'),
  models: () =>
    fetchJSON<{ models: { maker: string; model: string }[] }>('/api/models'),
  oidcEnabled: () => fetchJSON<{ enabled: boolean }>('/auth/oidc/enabled'),

  me: () => fetchJSON<Me>('/auth/me'),
  logout: () => postNoBody('/auth/logout'),

  uploadBegin: (body: {
    maker: string;
    model: string;
    files: BeginFile[];
  }) => postJSON<BeginResponse>('/api/upload/begin', body),
  uploadComplete: (body: {
    maker: string;
    model: string;
    upload_id: string;
    meta_toml: string;
  }) => postJSONNoBody('/api/upload/complete', body),

  adminPending: () => fetchJSON<PendingRow[]>('/api/admin/pending'),
  adminPendingDetail: (uploadId: string) =>
    fetchJSON<PendingDetail>(`/api/admin/pending/${enc(uploadId)}`),
  adminPendingEdit: (uploadId: string, edit: PendingEdit) =>
    putJSONNoBody(`/api/admin/pending/${enc(uploadId)}`, edit),
  adminPendingDownloadUrl: (uploadId: string, path: string): string => {
    const parts = path.split('/').map(enc).join('/');
    return `/api/admin/pending/${enc(uploadId)}/download/${parts}`;
  },
  adminSetEdit: (maker: string, model: string, edit: SetEdit) =>
    putJSONNoBody(
      `/api/admin/sets/${enc(maker)}/${enc(model)}`,
      edit,
    ),
  adminApprove: (
    uploadId: string,
    conflict: string,
    files: string[],
  ) =>
    postJSONNoBody(`/api/admin/pending/${enc(uploadId)}/approve`, {
      conflict,
      files,
    }),
  adminReject: (uploadId: string) =>
    postNoBody(`/api/admin/pending/${enc(uploadId)}/reject`),
};

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v >= 10 ? 0 : 1)} ${units[i]}`;
}
