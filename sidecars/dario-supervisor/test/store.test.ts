import { afterEach, describe, expect, test } from 'bun:test';
import { existsSync, mkdtempSync, readFileSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { SEAT_ALIAS, Store, sidFor } from '../src/store.ts';

const dirs: string[] = [];
const tmp = () => {
  const d = mkdtempSync(join(tmpdir(), 'dsup-store-'));
  dirs.push(d);
  return d;
};
afterEach(() => { for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true }); });

const creds = { accessToken: 'at', refreshToken: 'rt', expiresAt: 1_800_000_000_000, deviceId: 'dev', accountUuid: 'uuid' };

describe('Store', () => {
  test('sid 稳定且只含十六进制（邮箱里的 @ 不进路径）', () => {
    expect(sidFor('a@b.com')).toBe(sidFor('a@b.com'));
    expect(sidFor('a@b.com')).toMatch(/^[0-9a-f]{24}$/);
    expect(sidFor('a@b.com')).not.toBe(sidFor('a@b.co'));
  });

  test('upsert 写出 dario 账号文件：alias=seat、毫秒字段、0600 / 目录 0700', () => {
    const s = new Store(tmp());
    const rec = s.upsert('a@b.com', 'r1', { ...creds, grantedAt: 1_700_000_000_000 }, 'http://p:1/', 18800);
    const file = s.accountFile(rec);
    const body = JSON.parse(readFileSync(file, 'utf8'));
    expect(body).toMatchObject({ alias: SEAT_ALIAS, accessToken: 'at', refreshToken: 'rt', expiresAt: 1_800_000_000_000, deviceId: 'dev', accountUuid: 'uuid', grantedAt: 1_700_000_000_000 });
    expect(file.endsWith(`/.dario/accounts/${SEAT_ALIAS}.json`)).toBe(true);
    expect(statSync(file).mode & 0o777).toBe(0o600);
    expect(statSync(join(s.homeDir(rec), '.dario', 'accounts')).mode & 0o777).toBe(0o700);
    expect(statSync(join(s.dataDir, 'registry.json')).mode & 0o777).toBe(0o600);
  });

  test('没有 grantedAt 时不写该字段', () => {
    const s = new Store(tmp());
    const rec = s.upsert('x', 'r1', creds, null, 18800);
    expect('grantedAt' in JSON.parse(readFileSync(s.accountFile(rec), 'utf8'))).toBe(false);
  });

  test('注册表重启后读回，且不含令牌', () => {
    const d = tmp();
    const s1 = new Store(d);
    s1.upsert('x', 'r1', creds, 'http://p:1/', 18801);
    s1.update('x', { needsRelogin: true });
    const s2 = new Store(d);
    expect(s2.get('x')).toMatchObject({ id: 'x', rev: 'r1', port: 18801, proxy: 'http://p:1/', needsRelogin: true });
    expect(readFileSync(join(d, 'registry.json'), 'utf8')).not.toContain('"rt"');
    expect(s2.usedPorts().has(18801)).toBe(true);
  });

  test('remove 删除整个账号目录', () => {
    const s = new Store(tmp());
    const rec = s.upsert('x', 'r1', creds, null, 18800);
    expect(s.remove('x')).toBe(true);
    expect(existsSync(join(s.dataDir, rec.sid))).toBe(false);
    expect(s.get('x')).toBeUndefined();
    expect(s.remove('x')).toBe(false);
  });
});
