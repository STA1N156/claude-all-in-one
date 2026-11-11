/**
 * 用真实的原版 dario 联调（REAL_DARIO_CLI 指向上游 tag 构建出的 dist/cli.js；不设则跳过）。
 * 出口代理指向一个没人监听的本地端口，假令牌一个请求也到不了 Anthropic。
 */
import { afterAll, describe, expect, test } from 'bun:test';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { Config } from '../src/config.ts';
import { ProcessManager } from '../src/procman.ts';
import { Api } from '../src/server.ts';
import { Store } from '../src/store.ts';

const CLI = process.env.REAL_DARIO_CLI;
const KEY = 'real-key-0123456789abcdef';

describe.skipIf(!CLI)('真实 dario', () => {
  const dir = mkdtempSync(join(tmpdir(), 'dsup-real-'));
  const cfg: Config = {
    host: '127.0.0.1', port: 0, apiKey: KEY, darioCli: CLI ?? '', bunBin: process.execPath, dataDir: dir,
    portMin: 29700, portMax: 29719, idleStopMs: 60_000, startTimeoutMs: 20_000, healthPollMs: 60_000,
    stopTimeoutMs: 10_000, maxBodyMb: 16,
  };
  const logs: string[] = [];
  const store = new Store(dir);
  const pm = new ProcessManager(cfg, store, (m) => logs.push(m));
  const api = new Api(cfg, store, pm, (m) => logs.push(m));
  const call = (method: string, path: string, body?: unknown) => api.handle(new Request(`http://sup${path}`, {
    method, headers: { 'x-api-key': KEY, 'content-type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body),
  }));
  afterAll(async () => { await pm.stopAll(); rmSync(dir, { recursive: true, force: true }); });

  test('交接 → 按需启动 → dario 认出 seat 账号 → 鉴权 → 删除', async () => {
    const put = await call('PUT', '/accounts/real%40example.com', {
      rev: 'r1', accessToken: 'sk-ant-oat01-FAKE', refreshToken: 'sk-ant-ort01-FAKE',
      expiresAt: Date.now() + 6 * 3_600_000, deviceId: '11111111-2222-3333-4444-555555555555',
      accountUuid: 'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee', grantedAt: Date.now(), proxy: 'http://127.0.0.1:9/',
    });
    expect(put.status).toBe(200);
    const ens = await call('POST', '/accounts/real%40example.com/ensure');
    expect(ens.status).toBe(200);
    const { port } = await ens.json() as { port: number };
    const accts = await (await fetch(`http://127.0.0.1:${port}/accounts`, { headers: { 'x-api-key': KEY } })).json() as { accounts: Array<{ alias: string; action: string }> };
    expect(accts.accounts).toHaveLength(1);
    expect(accts.accounts[0].alias).toBe('seat');
    expect((await fetch(`http://127.0.0.1:${port}/v1/messages`, { method: 'POST', body: '{}' })).status).toBe(401);
    await pm.pollHealth();
    expect(pm.view('real@example.com').seat?.action).toBe('none');
    expect(logs.some((l) => l.includes('Outbound proxy: http://127.0.0.1:9/'))).toBe(true);
    expect(logs.some((l) => l.includes('mid-stream continuation: disabled'))).toBe(true);
    expect((await call('DELETE', '/accounts/real%40example.com')).status).toBe(200);
    expect(pm.view('real@example.com').state).toBe('stopped');
  }, 60_000);
});
