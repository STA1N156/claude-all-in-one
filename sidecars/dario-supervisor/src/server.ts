/**
 * 管理器 HTTP 接口（只监听回环，x-api-key 鉴权；/livez 免鉴权）。
 *
 *   PUT    /accounts/:id          交接凭证。rev 相同 → 不动令牌；新号 / 新 rev → 停进程、原子写文件
 *   PATCH  /accounts/:id          {proxy} 改出口代理（停进程，下次 ensure 生效）
 *   POST   /accounts/:id/ensure   确保进程在跑 → {port}
 *   DELETE /accounts/:id          停进程、删目录
 *   GET    /accounts[/:id]        状态（从不返回令牌）
 *
 * 同一个号的所有操作（含 ensure）串行执行，避免「正在替换凭证时被拉起」。
 */
import { timingSafeEqual } from 'node:crypto';
import type { Config } from './config.ts';
import type { ProcessManager } from './procman.ts';
import type { AccountRecord, Credentials, Store } from './store.ts';

const json = (status: number, body: unknown, headers: Record<string, string> = {}) =>
  new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json', ...headers } });

export function validProxy(raw: unknown): string | null | undefined {
  if (raw === null || raw === undefined || raw === '') return null;
  if (typeof raw !== 'string') return undefined;
  try {
    const u = new URL(raw);
    if ((u.protocol !== 'http:' && u.protocol !== 'https:') || !u.hostname) return undefined;
    return raw;
  } catch {
    return undefined;
  }
}

function parseCredentials(b: Record<string, unknown>): Credentials | string {
  const str = (k: string) => (typeof b[k] === 'string' && (b[k] as string).length > 0 ? (b[k] as string) : null);
  const accessToken = str('accessToken');
  const refreshToken = str('refreshToken');
  const deviceId = str('deviceId');
  const accountUuid = str('accountUuid');
  if (!accessToken || !refreshToken) return 'accessToken 与 refreshToken 必填';
  if (!deviceId || !accountUuid) return 'deviceId 与 accountUuid 必填（dario 用它们作为该号的身份）';
  const expiresAt = b.expiresAt;
  if (typeof expiresAt !== 'number' || !Number.isFinite(expiresAt) || expiresAt <= 0) return 'expiresAt 须为毫秒时间戳';
  const grantedAt = b.grantedAt;
  if (grantedAt !== undefined && (typeof grantedAt !== 'number' || !Number.isFinite(grantedAt) || grantedAt <= 0)) {
    return 'grantedAt 须为毫秒时间戳';
  }
  return { accessToken, refreshToken, expiresAt, deviceId, accountUuid, ...(grantedAt !== undefined ? { grantedAt: grantedAt as number } : {}) };
}

export class Api {
  private locks = new Map<string, Promise<unknown>>();
  private readonly keyBuf: Buffer;

  constructor(
    private readonly cfg: Config,
    private readonly store: Store,
    private readonly pm: ProcessManager,
    private readonly log: (msg: string) => void = (m) => console.log(m),
  ) {
    this.keyBuf = Buffer.from(cfg.apiKey);
  }

  /** 同一个 id 的操作排队执行。 */
  private withLock<T>(id: string, fn: () => Promise<T>): Promise<T> {
    const prev = this.locks.get(id) ?? Promise.resolve();
    const run = prev.catch(() => {}).then(fn);
    const tail = run.catch(() => {});
    this.locks.set(id, tail);
    void tail.then(() => { if (this.locks.get(id) === tail) this.locks.delete(id); });
    return run;
  }

  private authorized(req: Request): boolean {
    const raw = req.headers.get('x-api-key') ?? req.headers.get('authorization')?.replace(/^Bearer\s+/i, '') ?? '';
    const got = Buffer.from(raw);
    return got.length === this.keyBuf.length && timingSafeEqual(got, this.keyBuf);
  }

  private view(rec: AccountRecord) {
    return {
      id: rec.id,
      rev: rec.rev,
      port: rec.port,
      proxy: rec.proxy,
      needsRelogin: rec.needsRelogin,
      deviceId: rec.deviceId ?? null,
      accountUuid: rec.accountUuid ?? null,
      createdAt: rec.createdAt,
      updatedAt: rec.updatedAt,
      ...this.pm.view(rec.id),
    };
  }

  async handle(req: Request): Promise<Response> {
    const url = new URL(req.url);
    if (url.pathname === '/livez') return json(200, { status: 'ok' });
    if (!this.authorized(req)) return json(401, { error: 'unauthorized' });
    if (url.pathname === '/accounts' && req.method === 'GET') {
      return json(200, { accounts: this.store.all().map((r) => this.view(r)) });
    }
    const m = url.pathname.match(/^\/accounts\/([^/]+)(\/ensure)?$/);
    if (!m) return json(404, { error: 'not found' });
    let id: string;
    try {
      id = decodeURIComponent(m[1]);
    } catch {
      return json(400, { error: 'bad account id' });
    }
    if (id.length === 0 || id.length > 256) return json(400, { error: 'bad account id' });

    if (m[2]) {
      if (req.method !== 'POST') return json(405, { error: 'method not allowed' });
      const r = await this.withLock(id, () => this.pm.ensure(id));
      if (r.ok) return json(200, { port: r.port });
      const status = r.code === 'not_found' ? 404 : r.code === 'needs_relogin' ? 409 : 503;
      const headers: Record<string, string> = r.retryAfterMs ? { 'retry-after': String(Math.ceil(r.retryAfterMs / 1000)) } : {};
      return json(status, { error: r.code, message: r.message, ...(r.retryAfterMs ? { retryAfterMs: r.retryAfterMs } : {}) }, headers);
    }

    switch (req.method) {
      case 'GET': {
        const rec = this.store.get(id);
        return rec ? json(200, this.view(rec)) : json(404, { error: 'not_found' });
      }
      case 'PUT':
        return this.put(id, req);
      case 'PATCH':
        return this.patch(id, req);
      case 'DELETE':
        return this.withLock(id, async () => {
          if (!this.store.get(id)) return json(404, { error: 'not_found' });
          await this.pm.stop(id);
          this.store.remove(id);
          this.pm.resetRuntime(id);
          this.log(`[supervisor] ${id}: 已删除`);
          return json(200, { id, removed: true });
        });
      default:
        return json(405, { error: 'method not allowed' });
    }
  }

  private async body(req: Request): Promise<Record<string, unknown> | null> {
    try {
      const b = (await req.json()) as unknown;
      return b && typeof b === 'object' && !Array.isArray(b) ? (b as Record<string, unknown>) : null;
    } catch {
      return null;
    }
  }

  private async put(id: string, req: Request): Promise<Response> {
    const b = await this.body(req);
    if (!b) return json(400, { error: 'body must be a JSON object' });
    const rev = b.rev;
    if (typeof rev !== 'string' || rev.length === 0 || rev.length > 128) return json(400, { error: 'rev 必填（≤128 字符）' });
    const proxy = validProxy(b.proxy);
    if (proxy === undefined) return json(400, { error: 'proxy 须为 http(s) URL（dario 不支持 socks）' });
    const creds = parseCredentials(b);
    if (typeof creds === 'string') return json(400, { error: creds });
    return this.withLock(id, async () => {
      const existing = this.store.get(id);
      // 同 rev 重复交接：dario 可能早已刷新过令牌，绝不能用旧令牌覆盖。
      if (existing && existing.rev === rev) return json(200, { status: 'unchanged', ...this.view(existing) });
      if (existing) await this.pm.stop(id);
      // 新号：分配端口与登记在全局临界区里完成（不同号的并发交接不能拿到同一个端口）。
      const rec = existing
        ? this.store.upsert(id, rev, creds, proxy, existing.port)
        : await this.pm.withAllocation(async () => this.store.upsert(id, rev, creds, proxy, await this.pm.allocatePort()));
      this.pm.resetRuntime(id);
      this.log(`[supervisor] ${id}: ${existing ? '凭证已替换（重新登录）' : '已接收交接'} rev=${rev} port=${rec.port}`);
      return json(200, { status: existing ? 'replaced' : 'created', ...this.view(rec) });
    });
  }

  private async patch(id: string, req: Request): Promise<Response> {
    const b = await this.body(req);
    if (!b || !('proxy' in b)) return json(400, { error: '只支持修改 proxy' });
    const proxy = validProxy(b.proxy);
    if (proxy === undefined) return json(400, { error: 'proxy 须为 http(s) URL（dario 不支持 socks）' });
    return this.withLock(id, async () => {
      const rec = this.store.get(id);
      if (!rec) return json(404, { error: 'not_found' });
      if (rec.proxy === proxy) return json(200, { status: 'unchanged', ...this.view(rec) });
      await this.pm.stop(id);
      const next = this.store.update(id, { proxy })!;
      this.log(`[supervisor] ${id}: 出口代理已更换，下次请求时以新出口启动`);
      return json(200, { status: 'updated', ...this.view(next) });
    });
  }
}
