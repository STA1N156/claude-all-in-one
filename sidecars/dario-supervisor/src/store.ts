/**
 * 持久化层：账号注册表（/data/registry.json）与每号的 dario 凭证文件。
 *
 * 目录布局（DATA_DIR 下）：
 *   registry.json                         —— 所有号的元数据（不含令牌）
 *   <sid>/home/.dario/accounts/seat.json  —— 该号的 dario 账号文件（含令牌，0600）
 *
 * <sid> 是账号 id 的 sha256 前缀：caio 的 id 常是邮箱（含 `@`），不能直接做路径。
 *
 * 令牌只在「交接」时写一次；之后由该号的 dario 进程独占刷新并回写同一个文件。
 * 本模块绝不在 rev 相同时覆盖凭证文件 —— 两方同时持有一条 refresh token 会触发
 * Anthropic 的复用检测，令牌一起作废。
 */
import { createHash } from 'node:crypto';
import { closeSync, fsyncSync, mkdirSync, openSync, readFileSync, renameSync, rmSync, writeSync, chmodSync, existsSync } from 'node:fs';
import { dirname, join } from 'node:path';

/** dario 账号文件的 alias。绝不能用 `login`：那会让 dario 每次启动从其它凭证源回填。 */
export const SEAT_ALIAS = 'seat';

export interface AccountRecord {
  id: string;
  sid: string;
  /** 凭证版本：caio 每次登录 / 导入生成新值，相同则 PUT 不覆盖令牌。 */
  rev: string;
  port: number;
  proxy: string | null;
  createdAt: number;
  updatedAt: number;
  /** dario 判定 refresh token 已死（需要重新登录）。重新 PUT 新 rev 时清除。 */
  needsRelogin: boolean;
  /** 该号对 Anthropic 呈现的身份（非机密）。回传给 caio 作为权威值，避免两边各存一套。 */
  deviceId?: string;
  accountUuid?: string;
}

export interface Credentials {
  accessToken: string;
  refreshToken: string;
  /** 毫秒时间戳。 */
  expiresAt: number;
  deviceId: string;
  accountUuid: string;
  /** 毫秒时间戳；缺省则 dario 无法提示 28 天授权到期。 */
  grantedAt?: number;
}

export function sidFor(id: string): string {
  return createHash('sha256').update(id, 'utf8').digest('hex').slice(0, 24);
}

/** 写临时文件 → fsync → rename → fsync 目录。崩溃后要么是旧文件、要么是新文件。 */
export function writeFileAtomic(path: string, data: string, mode: number): void {
  const dir = dirname(path);
  const tmp = join(dir, `.${Date.now()}-${process.pid}-${Math.random().toString(36).slice(2)}.tmp`);
  const fd = openSync(tmp, 'w', mode);
  try {
    writeSync(fd, data);
    fsyncSync(fd);
  } finally {
    closeSync(fd);
  }
  chmodSync(tmp, mode);
  renameSync(tmp, path);
  try {
    const dfd = openSync(dir, 'r');
    try { fsyncSync(dfd); } finally { closeSync(dfd); }
  } catch {
    // 有的文件系统不支持对目录 fsync，rename 已足够原子。
  }
}

function mkdirPrivate(path: string): void {
  mkdirSync(path, { recursive: true, mode: 0o700 });
  chmodSync(path, 0o700);
}

export class Store {
  private records = new Map<string, AccountRecord>();

  constructor(readonly dataDir: string) {
    mkdirPrivate(dataDir);
    this.load();
  }

  private get registryPath(): string {
    return join(this.dataDir, 'registry.json');
  }

  private load(): void {
    if (!existsSync(this.registryPath)) return;
    const raw = JSON.parse(readFileSync(this.registryPath, 'utf8')) as { accounts?: AccountRecord[] };
    for (const r of raw.accounts ?? []) {
      if (r && typeof r.id === 'string' && typeof r.port === 'number') this.records.set(r.id, { ...r, needsRelogin: !!r.needsRelogin });
    }
  }

  private save(): void {
    const accounts = [...this.records.values()].sort((a, b) => a.port - b.port);
    writeFileAtomic(this.registryPath, JSON.stringify({ version: 1, accounts }, null, 2), 0o600);
  }

  get(id: string): AccountRecord | undefined {
    return this.records.get(id);
  }

  all(): AccountRecord[] {
    return [...this.records.values()];
  }

  usedPorts(): Set<number> {
    return new Set([...this.records.values()].map((r) => r.port));
  }

  homeDir(rec: Pick<AccountRecord, 'sid'>): string {
    return join(this.dataDir, rec.sid, 'home');
  }

  accountFile(rec: Pick<AccountRecord, 'sid'>): string {
    return join(this.homeDir(rec), '.dario', 'accounts', `${SEAT_ALIAS}.json`);
  }

  /** 新建或替换（调用方已保证进程停止）。写凭证文件，再落注册表。 */
  upsert(id: string, rev: string, creds: Credentials, proxy: string | null, port: number): AccountRecord {
    const now = Date.now();
    const prev = this.records.get(id);
    const rec: AccountRecord = {
      id,
      sid: sidFor(id),
      rev,
      port,
      proxy,
      createdAt: prev?.createdAt ?? now,
      updatedAt: now,
      needsRelogin: false,
      deviceId: creds.deviceId,
      accountUuid: creds.accountUuid,
    };
    const home = this.homeDir(rec);
    mkdirPrivate(join(this.dataDir, rec.sid));
    mkdirPrivate(home);
    mkdirPrivate(join(home, '.dario'));
    mkdirPrivate(join(home, '.dario', 'accounts'));
    const file: Record<string, unknown> = {
      alias: SEAT_ALIAS,
      accessToken: creds.accessToken,
      refreshToken: creds.refreshToken,
      expiresAt: creds.expiresAt,
      scopes: [],
      deviceId: creds.deviceId,
      accountUuid: creds.accountUuid,
    };
    if (creds.grantedAt !== undefined) file.grantedAt = creds.grantedAt;
    writeFileAtomic(this.accountFile(rec), JSON.stringify(file, null, 2), 0o600);
    this.records.set(id, rec);
    this.save();
    return rec;
  }

  update(id: string, patch: Partial<Pick<AccountRecord, 'proxy' | 'port' | 'needsRelogin'>>): AccountRecord | undefined {
    const rec = this.records.get(id);
    if (!rec) return undefined;
    const next = { ...rec, ...patch, updatedAt: Date.now() };
    this.records.set(id, next);
    this.save();
    return next;
  }

  /** 删除注册表项与整个账号目录（含凭证）。调用方已保证进程停止。 */
  remove(id: string): boolean {
    const rec = this.records.get(id);
    if (!rec) return false;
    rmSync(join(this.dataDir, rec.sid), { recursive: true, force: true });
    this.records.delete(id);
    this.save();
    return true;
  }
}
