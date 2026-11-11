import { afterEach, describe, expect, test } from 'bun:test';
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createServer, type Server } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { Config } from '../src/config.ts';
import { ProcessManager } from '../src/procman.ts';
import { Api } from '../src/server.ts';
import { Store } from '../src/store.ts';

const KEY = 'test-key-0123456789abcdef';
const FAKE = join(import.meta.dir, 'fake-dario.ts');
let nextPortBase = 28800 + Math.floor(Math.random() * 50) * 20;

interface Rig { cfg: Config; store: Store; pm: ProcessManager; api: Api; dir: string; logs: string[] }
const rigs: Rig[] = [];
const servers: Server[] = [];

function rig(over: Partial<Config> = {}): Rig {
  const dir = mkdtempSync(join(tmpdir(), 'dsup-api-'));
  const base = nextPortBase;
  nextPortBase += 20;
  const cfg: Config = {
    host: '127.0.0.1', port: 0, apiKey: KEY, darioCli: FAKE, bunBin: process.execPath, dataDir: dir,
    portMin: base, portMax: base + 19, idleStopMs: 60_000, startTimeoutMs: 8_000, healthPollMs: 60_000,
    stopTimeoutMs: 5_000, maxBodyMb: 16, ...over,
  };
  const logs: string[] = [];
  const store = new Store(dir);
  const pm = new ProcessManager(cfg, store, (m) => logs.push(m));
  const api = new Api(cfg, store, pm, (m) => logs.push(m));
  const r = { cfg, store, pm, api, dir, logs };
  rigs.push(r);
  return r;
}

afterEach(async () => {
  for (const s of servers.splice(0)) s.close();
  for (const r of rigs.splice(0)) {
    await r.pm.stopAll();
    rmSync(r.dir, { recursive: true, force: true });
  }
});

const req = (r: Rig, method: string, path: string, body?: unknown, key: string | null = KEY) =>
  r.api.handle(new Request(`http://sup${path}`, {
    method,
    headers: { ...(key ? { 'x-api-key': key } : {}), 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  }));

const handover = (rev = 'r1', extra: Record<string, unknown> = {}) => ({
  rev, accessToken: 'at-' + rev, refreshToken: 'rt-' + rev, expiresAt: Date.now() + 3_600_000,
  deviceId: 'dev-1', accountUuid: 'uuid-1', grantedAt: Date.now(), ...extra,
});

const ID = 'someone@example.com';
const enc = encodeURIComponent(ID);

describe('鉴权与校验', () => {
  test('/livez 免鉴权，其余缺 key 返回 401', async () => {
    const r = rig();
    expect((await req(r, 'GET', '/livez', undefined, null)).status).toBe(200);
    expect((await req(r, 'GET', '/accounts', undefined, null)).status).toBe(401);
    expect((await req(r, 'GET', '/accounts', undefined, 'wrong-key-0123456789abcd')).status).toBe(401);
    expect((await req(r, 'GET', '/accounts')).status).toBe(200);
  });

  test('PUT 拒绝缺字段、socks 代理、空 rev', async () => {
    const r = rig();
    expect((await req(r, 'PUT', `/accounts/${enc}`, { ...handover(), refreshToken: '' })).status).toBe(400);
    expect((await req(r, 'PUT', `/accounts/${enc}`, { ...handover(), deviceId: undefined })).status).toBe(400);
    expect((await req(r, 'PUT', `/accounts/${enc}`, { ...handover(), proxy: 'socks5://h:1' })).status).toBe(400);
    expect((await req(r, 'PUT', `/accounts/${enc}`, { ...handover(), rev: '' })).status).toBe(400);
    expect((await req(r, 'PUT', `/accounts/${enc}`, { ...handover(), expiresAt: 'soon' })).status).toBe(400);
    expect(r.store.all()).toHaveLength(0);
  });
});

describe('交接：同 rev 绝不覆盖令牌', () => {
  test('新号创建；同 rev 重放不动文件；新 rev 才替换', async () => {
    const r = rig();
    const c = await req(r, 'PUT', `/accounts/${enc}`, handover('r1', { proxy: 'http://127.0.0.1:9/' }));
    expect(c.status).toBe(200);
    const created = await c.json() as { status: string; port: number };
    expect(created.status).toBe('created');
    expect(created.port).toBeGreaterThanOrEqual(r.cfg.portMin);
    const file = r.store.accountFile(r.store.get(ID)!);
    // 模拟 dario 已经刷新过令牌并回写了文件。
    const rotated = { ...JSON.parse(readFileSync(file, 'utf8')), accessToken: 'at-rotated', refreshToken: 'rt-rotated' };
    writeFileSync(file, JSON.stringify(rotated));
    const again = await (await req(r, 'PUT', `/accounts/${enc}`, handover('r1'))).json() as { status: string };
    expect(again.status).toBe('unchanged');
    expect(JSON.parse(readFileSync(file, 'utf8')).refreshToken).toBe('rt-rotated');
    const replaced = await (await req(r, 'PUT', `/accounts/${enc}`, handover('r2'))).json() as { status: string; port: number };
    expect(replaced.status).toBe('replaced');
    expect(replaced.port).toBe(created.port);
    expect(JSON.parse(readFileSync(file, 'utf8')).refreshToken).toBe('rt-r2');
  });

  test('新 rev 替换前先停掉运行中的进程', async () => {
    const r = rig();
    await req(r, 'PUT', `/accounts/${enc}`, handover('r1'));
    expect((await req(r, 'POST', `/accounts/${enc}/ensure`)).status).toBe(200);
    expect(r.pm.view(ID).state).toBe('running');
    await req(r, 'PUT', `/accounts/${enc}`, handover('r2'));
    expect(r.pm.view(ID).state).toBe('stopped');
  });
});

describe('进程生命周期', () => {
  test('ensure 按需启动，参数与环境白名单正确，重复 ensure 复用', async () => {
    const r = rig();
    process.env.SUPERVISOR_SECRET_SHOULD_NOT_LEAK = 'x';
    await req(r, 'PUT', `/accounts/${enc}`, handover('r1', { proxy: 'http://127.0.0.1:9/' }));
    expect(r.pm.view(ID).state).toBe('stopped');
    const e1 = await req(r, 'POST', `/accounts/${enc}/ensure`);
    expect(e1.status).toBe(200);
    const { port } = await e1.json() as { port: number };
    expect((await fetch(`http://127.0.0.1:${port}/livez`)).status).toBe(200);
    const e2 = await (await req(r, 'POST', `/accounts/${enc}/ensure`)).json() as { port: number };
    expect(e2.port).toBe(port);
    const launch = JSON.parse(readFileSync(join(r.store.homeDir(r.store.get(ID)!), 'launch.json'), 'utf8'));
    expect(launch.argv).toEqual(expect.arrayContaining([
      'proxy', `--port=${port}`, '--host=127.0.0.1', '--no-live-capture', '--session-per-client',
      '--no-ledger', '--upstream-proxy=http://127.0.0.1:9/',
    ]));
    // 续流必须保持开启(2026-10-12 断流事故):argv 里不得再出现 --no-midstream-continue,
    // 否则长思考流被上游掐断时客户端只能拿到残流。
    expect(launch.argv).not.toContain('--no-midstream-continue');
    expect(launch.env.HOME).toBe(r.store.homeDir(r.store.get(ID)!));
    expect(launch.env.DARIO_API_KEY).toBe(KEY);
    expect(launch.env.DARIO_IGNORE_CC_CREDENTIALS).toBe('1');
    expect(launch.env.SUPERVISOR_SECRET_SHOULD_NOT_LEAK).toBeUndefined();
    delete process.env.SUPERVISOR_SECRET_SHOULD_NOT_LEAK;
  });

  test('并发 ensure 只起一个进程', async () => {
    const r = rig();
    await req(r, 'PUT', `/accounts/${enc}`, handover());
    const rs = await Promise.all([1, 2, 3, 4].map(() => req(r, 'POST', `/accounts/${enc}/ensure`)));
    expect(rs.every((x) => x.status === 200)).toBe(true);
    expect(r.logs.filter((l) => l.includes('dario 已启动'))).toHaveLength(1);
  });

  test('闲置超时后停止，下一次 ensure 冷启动恢复', async () => {
    const r = rig({ idleStopMs: 1_000 });
    await req(r, 'PUT', `/accounts/${enc}`, handover());
    await req(r, 'POST', `/accounts/${enc}/ensure`);
    await r.pm.idleSweep(Date.now() + 500);
    expect(r.pm.view(ID).state).toBe('running');
    await r.pm.idleSweep(Date.now() + 2_000);
    expect(r.pm.view(ID).state).toBe('stopped');
    expect((await req(r, 'POST', `/accounts/${enc}/ensure`)).status).toBe(200);
    expect(r.pm.view(ID).state).toBe('running');
  });

  test('启动即崩溃 → 503 start_failed，随后进入退避并带 retry-after', async () => {
    const r = rig();
    await req(r, 'PUT', `/accounts/${enc}`, handover());
    writeFileSync(join(r.store.homeDir(r.store.get(ID)!), 'fake.json'), JSON.stringify({ crash: true }));
    const e1 = await req(r, 'POST', `/accounts/${enc}/ensure`);
    expect(e1.status).toBe(503);
    expect((await e1.json() as { error: string }).error).toBe('start_failed');
    const e2 = await req(r, 'POST', `/accounts/${enc}/ensure`);
    expect(e2.status).toBe(503);
    expect((await e2.json() as { error: string }).error).toBe('backoff');
    expect(Number(e2.headers.get('retry-after'))).toBeGreaterThan(0);
  });

  test('注册端口被占用时拒绝启动，不换端口（防同号双进程）', async () => {
    const r = rig();
    await req(r, 'PUT', `/accounts/${enc}`, handover());
    const port = r.store.get(ID)!.port;
    const squatter = createServer();
    servers.push(squatter);
    await new Promise<void>((ok) => squatter.listen(port, '127.0.0.1', () => ok()));
    const e = await req(r, 'POST', `/accounts/${enc}/ensure`);
    expect(e.status).toBe(503);
    expect(r.store.get(ID)!.port).toBe(port);
    expect(r.logs.some((l) => l.includes('被占用'))).toBe(true);
  });

  test('不同新号并发交接拿到不同端口', async () => {
    const r = rig();
    const ids = ['p1', 'p2', 'p3', 'p4', 'p5'];
    const rs = await Promise.all(ids.map((id) => req(r, 'PUT', `/accounts/${id}`, handover())));
    expect(rs.every((x) => x.status === 200)).toBe(true);
    const ports = ids.map((id) => r.store.get(id)!.port);
    expect(new Set(ports).size).toBe(ids.length);
  });

  test('交接结果回传实际存下的身份（同 rev 重放也回传首次的值）', async () => {
    const r = rig();
    const first = await (await req(r, 'PUT', `/accounts/${enc}`, handover('r1', { deviceId: 'dev-A', accountUuid: 'uuid-A' }))).json() as { deviceId: string; accountUuid: string };
    expect(first).toMatchObject({ deviceId: 'dev-A', accountUuid: 'uuid-A' });
    const again = await (await req(r, 'PUT', `/accounts/${enc}`, handover('r1', { deviceId: 'dev-B', accountUuid: 'uuid-B' }))).json() as { status: string; deviceId: string };
    expect(again).toMatchObject({ status: 'unchanged', deviceId: 'dev-A' });
  });

  test('新号分配端口时跳过已登记与被占用的端口', async () => {
    const r = rig();
    const squatter = createServer();
    servers.push(squatter);
    await new Promise<void>((ok) => squatter.listen(r.cfg.portMin, '127.0.0.1', () => ok()));
    await req(r, 'PUT', '/accounts/a', handover());
    await req(r, 'PUT', '/accounts/b', handover());
    expect(r.store.get('a')!.port).toBe(r.cfg.portMin + 1);
    expect(r.store.get('b')!.port).toBe(r.cfg.portMin + 2);
  });
});

describe('死号判定', () => {
  test('dario 报 action=regrant → 标记需要重新登录、停进程、ensure 返回 409', async () => {
    const r = rig();
    await req(r, 'PUT', `/accounts/${enc}`, handover());
    await req(r, 'POST', `/accounts/${enc}/ensure`);
    writeFileSync(join(r.store.homeDir(r.store.get(ID)!), 'fake.json'), JSON.stringify({ action: 'regrant' }));
    await r.pm.pollHealth();
    expect(r.store.get(ID)!.needsRelogin).toBe(true);
    expect(r.pm.view(ID).state).toBe('stopped');
    const e = await req(r, 'POST', `/accounts/${enc}/ensure`);
    expect(e.status).toBe(409);
    // 重新登录（新 rev）清除死号标记。
    await req(r, 'PUT', `/accounts/${enc}`, handover('r2'));
    expect(r.store.get(ID)!.needsRelogin).toBe(false);
    expect((await req(r, 'POST', `/accounts/${enc}/ensure`)).status).toBe(200);
  });

  test('令牌过期超过一个刷新周期才判死，短暂过期不判', async () => {
    const r = rig();
    await req(r, 'PUT', `/accounts/${enc}`, handover());
    await req(r, 'POST', `/accounts/${enc}/ensure`);
    writeFileSync(join(r.store.homeDir(r.store.get(ID)!), 'fake.json'), JSON.stringify({ expiresInMs: -1000 }));
    const t0 = Date.now();
    await r.pm.pollHealth(t0);
    await r.pm.pollHealth(t0 + 10 * 60_000);
    expect(r.store.get(ID)!.needsRelogin).toBe(false);
    await r.pm.pollHealth(t0 + 21 * 60_000);
    expect(r.store.get(ID)!.needsRelogin).toBe(true);
  });

  test('状态里带 dario 的额度读数，且从不返回令牌', async () => {
    const r = rig();
    await req(r, 'PUT', `/accounts/${enc}`, handover());
    await req(r, 'POST', `/accounts/${enc}/ensure`);
    await r.pm.pollHealth();
    const text = await (await req(r, 'GET', `/accounts/${enc}`)).text();
    expect(JSON.parse(text).seat).toMatchObject({ action: 'none', util5h: 0.1, util7d: 0.2 });
    expect(text).not.toContain('rt-r1');
    expect(text).not.toContain('at-r1');
  });
});

describe('改代理与删除', () => {
  test('PATCH 代理：停进程并在下次启动生效；同值不重启', async () => {
    const r = rig();
    await req(r, 'PUT', `/accounts/${enc}`, handover());
    await req(r, 'POST', `/accounts/${enc}/ensure`);
    expect((await (await req(r, 'PATCH', `/accounts/${enc}`, { proxy: null })).json() as { status: string }).status).toBe('unchanged');
    expect(r.pm.view(ID).state).toBe('running');
    expect((await req(r, 'PATCH', `/accounts/${enc}`, { proxy: 'socks5://h:1' })).status).toBe(400);
    expect((await (await req(r, 'PATCH', `/accounts/${enc}`, { proxy: 'http://127.0.0.1:8/' })).json() as { status: string }).status).toBe('updated');
    expect(r.pm.view(ID).state).toBe('stopped');
    await req(r, 'POST', `/accounts/${enc}/ensure`);
    const launch = JSON.parse(readFileSync(join(r.store.homeDir(r.store.get(ID)!), 'launch.json'), 'utf8'));
    expect(launch.argv).toContain('--upstream-proxy=http://127.0.0.1:8/');
  });

  test('DELETE 停进程、删目录，之后 ensure 404', async () => {
    const r = rig();
    await req(r, 'PUT', `/accounts/${enc}`, handover());
    await req(r, 'POST', `/accounts/${enc}/ensure`);
    const home = r.store.homeDir(r.store.get(ID)!);
    expect((await req(r, 'DELETE', `/accounts/${enc}`)).status).toBe(200);
    expect(existsSync(home)).toBe(false);
    expect((await req(r, 'POST', `/accounts/${enc}/ensure`)).status).toBe(404);
    expect((await req(r, 'DELETE', `/accounts/${enc}`)).status).toBe(404);
  });
});
