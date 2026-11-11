/**
 * 每号一个 dario 进程：按需启动、闲置停止、崩溃退避、健康轮询判死。
 *
 * 不变量（违反即可能让两个进程同时刷新同一条 refresh token → 令牌作废）：
 * - 同一个号任何时刻至多一个进程。启动并发由 startPromise 合并；
 * - 号的注册端口被占用时**拒绝启动**而不是换端口 —— 占着端口的很可能是上一个
 *   管理器留下的同号 dario。新端口只在 PUT 新号时分配；
 * - 停止 = SIGTERM 后等进程真正退出（dario 会排空在途请求、把内存里更新的令牌落盘），
 *   超时才 SIGKILL。替换凭证文件前必须先停。
 */
import { spawn, type ChildProcess } from 'node:child_process';
import { createServer } from 'node:net';
import type { Config } from './config.ts';
import type { AccountRecord, Store } from './store.ts';

export type ProcState = 'stopped' | 'starting' | 'running' | 'stopping';

/** dario `GET /accounts` 里与判活有关的字段（单号池只有一个 seat）。 */
export interface SeatStatus {
  action: string | null;
  status: string | null;
  expiresInMs: number | null;
  util5h: number | null;
  util7d: number | null;
  grantLevel: string | null;
  requestCount: number | null;
  rejectedCount: number | null;
}

interface Runtime {
  state: ProcState;
  child: ChildProcess | null;
  startPromise: Promise<EnsureResult> | null;
  exited: Promise<void> | null;
  lastUsed: number;
  startedAt: number;
  crashes: number;
  backoffUntil: number;
  status: SeatStatus | null;
  statusAt: number;
  expiredSince: number | null;
}

export type EnsureResult =
  | { ok: true; port: number }
  | { ok: false; code: 'not_found' | 'needs_relogin' | 'backoff' | 'start_failed'; message: string; retryAfterMs?: number };

/** 过期超过一个刷新周期（dario 15 分钟一轮）还没刷新上，就认定 refresh token 已死。 */
const EXPIRED_DEAD_AFTER_MS = 20 * 60_000;
/** 连续跑满这么久才算「稳定」，崩溃计数清零。 */
const STABLE_RUN_MS = 5 * 60_000;
const BACKOFF_BASE_MS = 1_000;
const BACKOFF_MAX_MS = 60_000;

export function parseSeatStatus(body: unknown): SeatStatus | null {
  const seat = (body as { accounts?: unknown[] } | null)?.accounts?.[0] as Record<string, unknown> | undefined;
  if (!seat || typeof seat !== 'object') return null;
  const num = (v: unknown) => (typeof v === 'number' && Number.isFinite(v) ? v : null);
  const str = (v: unknown) => (typeof v === 'string' ? v : null);
  return {
    action: str(seat.action),
    status: str(seat.status),
    expiresInMs: num(seat.expiresInMs),
    util5h: num(seat.util5h),
    util7d: num(seat.util7d),
    grantLevel: str(seat.grantLevel),
    requestCount: num(seat.requestCount),
    rejectedCount: num(seat.rejectedCount),
  };
}

/** 端口此刻能否在 127.0.0.1 上监听。 */
export function portFree(port: number): Promise<boolean> {
  return new Promise((resolve) => {
    const srv = createServer();
    srv.once('error', () => resolve(false));
    srv.listen({ port, host: '127.0.0.1', exclusive: true }, () => srv.close(() => resolve(true)));
  });
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export class ProcessManager {
  private rt = new Map<string, Runtime>();
  private timers: ReturnType<typeof setInterval>[] = [];

  constructor(
    private readonly cfg: Config,
    private readonly store: Store,
    private readonly log: (msg: string) => void = (m) => console.log(m),
  ) {}

  private runtime(id: string): Runtime {
    let r = this.rt.get(id);
    if (!r) {
      r = {
        state: 'stopped', child: null, startPromise: null, exited: null, lastUsed: 0, startedAt: 0,
        crashes: 0, backoffUntil: 0, status: null, statusAt: 0, expiredSince: null,
      };
      this.rt.set(id, r);
    }
    return r;
  }

  /**
   * 新号分配端口：区间内最小的、注册表没用过且此刻可监听的端口。
   * 调用方必须在 [`withAllocation`] 里调用并在同一临界区内落注册表——否则两个不同新号
   * 并发交接会探到同一个空闲端口，各自登记，之后其中一个号永远「端口被占用」启动不了。
   */
  async allocatePort(): Promise<number> {
    const used = this.store.usedPorts();
    for (let p = this.cfg.portMin; p <= this.cfg.portMax; p++) {
      if (used.has(p)) continue;
      if (await portFree(p)) return p;
    }
    throw new Error(`端口区间 ${this.cfg.portMin}-${this.cfg.portMax} 已用尽`);
  }

  private allocTail: Promise<unknown> = Promise.resolve();

  /** 全局串行的「分配端口 + 登记」临界区。 */
  withAllocation<T>(fn: () => Promise<T>): Promise<T> {
    const run = this.allocTail.catch(() => {}).then(fn);
    this.allocTail = run.catch(() => {});
    return run;
  }

  /** 确保该号的进程在跑，返回端口。每个请求都会调用一次（同时刷新闲置计时）。 */
  async ensure(id: string): Promise<EnsureResult> {
    const rec = this.store.get(id);
    if (!rec) return { ok: false, code: 'not_found', message: '管理器里没有这个号' };
    if (rec.needsRelogin) return { ok: false, code: 'needs_relogin', message: 'dario 判定 refresh token 已失效，需要重新登录' };
    const r = this.runtime(id);
    r.lastUsed = Date.now();
    if (r.state === 'running') return { ok: true, port: rec.port };
    if (r.state === 'starting' && r.startPromise) return r.startPromise;
    if (r.state === 'stopping' && r.exited) await r.exited;
    const wait = r.backoffUntil - Date.now();
    if (wait > 0) return { ok: false, code: 'backoff', message: '进程刚崩溃，退避中', retryAfterMs: wait };
    r.startPromise = this.start(rec, r).finally(() => { r.startPromise = null; });
    return r.startPromise;
  }

  private async start(rec: AccountRecord, r: Runtime): Promise<EnsureResult> {
    r.state = 'starting';
    if (!(await portFree(rec.port))) {
      r.state = 'stopped';
      this.noteCrash(r);
      const message = `端口 ${rec.port} 被占用（可能是上一个管理器留下的同号 dario），拒绝启动以免两个进程同时刷新`;
      this.log(`[supervisor] ${rec.id}: ${message}`);
      return { ok: false, code: 'start_failed', message };
    }
    const args = [
      this.cfg.darioCli, 'proxy',
      `--port=${rec.port}`, '--host=127.0.0.1',
      // 续流保持 dario 默认开启(**别再加 --no-midstream-continue**):claude.ai 常在 4 分钟
      // 级掐断长思考流,不开时客户端只能拿到无 message_stop 的残流(2026-10-12 opus-5.5
      // 实测 223s/266s 断流,opencode 表现为「请求永远发不出去」)。续流第一跳回本号
      // 原模型重试,深度上限 2 层(dario MAX_CONTINUATION_DEPTH)。
      '--no-live-capture', '--session-per-client', '--no-ledger',
    ];
    if (rec.proxy) args.push(`--upstream-proxy=${rec.proxy}`);
    const child = spawn(this.cfg.bunBin, args, {
      cwd: this.store.homeDir(rec),
      // 白名单环境：不把管理器自己的变量（含其它密钥）漏给子进程。
      env: {
        PATH: process.env.PATH ?? '/usr/local/bin:/usr/bin:/bin',
        HOME: this.store.homeDir(rec),
        DARIO_API_KEY: this.cfg.apiKey,
        DARIO_IGNORE_CC_CREDENTIALS: '1',
        DARIO_NO_LIVE_CAPTURE: '1',
        DARIO_MAX_BODY_MB: String(this.cfg.maxBodyMb),
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    r.child = child;
    const tag = `[dario ${rec.id}]`;
    const pipe = (s: NodeJS.ReadableStream | null) => {
      let buf = '';
      s?.on('data', (d: Buffer) => {
        buf += d.toString('utf8');
        let i: number;
        while ((i = buf.indexOf('\n')) >= 0) {
          const line = buf.slice(0, i).trimEnd();
          buf = buf.slice(i + 1);
          if (line) this.log(`${tag} ${line}`);
        }
      });
    };
    pipe(child.stdout);
    pipe(child.stderr);
    let exitedNow = false;
    r.exited = new Promise<void>((resolve) => {
      child.once('exit', (code, signal) => {
        exitedNow = true;
        const wasStopping = r.state === 'stopping';
        const ranMs = r.startedAt ? Date.now() - r.startedAt : 0;
        r.state = 'stopped';
        r.child = null;
        r.startedAt = 0;
        if (!wasStopping) {
          if (ranMs >= STABLE_RUN_MS) r.crashes = 0;
          this.noteCrash(r);
          this.log(`[supervisor] ${rec.id}: dario 意外退出 code=${code} signal=${signal}，退避 ${Math.max(0, r.backoffUntil - Date.now())}ms`);
        }
        resolve();
      });
    });

    const deadline = Date.now() + this.cfg.startTimeoutMs;
    while (Date.now() < deadline && !exitedNow) {
      try {
        const res = await fetch(`http://127.0.0.1:${rec.port}/livez`, { signal: AbortSignal.timeout(1_000) });
        if (res.ok) {
          r.state = 'running';
          r.startedAt = Date.now();
          r.expiredSince = null;
          this.log(`[supervisor] ${rec.id}: dario 已启动 port=${rec.port} pid=${child.pid}`);
          return { ok: true, port: rec.port };
        }
      } catch {
        // 还没监听，继续等。
      }
      await sleep(150);
    }
    if (!exitedNow) {
      await this.stopRuntime(rec.id, r, 5_000);
      this.noteCrash(r);
    }
    const message = exitedNow ? 'dario 启动后立即退出（看管理器日志里该号的输出）' : `dario ${this.cfg.startTimeoutMs}ms 内未就绪`;
    this.log(`[supervisor] ${rec.id}: ${message}`);
    return { ok: false, code: 'start_failed', message };
  }

  private noteCrash(r: Runtime): void {
    r.crashes += 1;
    r.backoffUntil = Date.now() + Math.min(BACKOFF_MAX_MS, BACKOFF_BASE_MS * 2 ** (r.crashes - 1));
  }

  private async stopRuntime(id: string, r: Runtime, timeoutMs: number): Promise<void> {
    const child = r.child;
    if (!child || r.state === 'stopped') return;
    r.state = 'stopping';
    child.kill('SIGTERM');
    const exited = r.exited ?? Promise.resolve();
    const done = await Promise.race([exited.then(() => true), sleep(timeoutMs).then(() => false)]);
    if (!done) {
      this.log(`[supervisor] ${id}: SIGTERM 后 ${timeoutMs}ms 未退出，SIGKILL`);
      child.kill('SIGKILL');
      await exited;
    }
  }

  /** 停掉该号的进程并等它退出（替换凭证、改代理、删除前调用）。 */
  async stop(id: string): Promise<void> {
    const r = this.rt.get(id);
    if (!r) return;
    if (r.startPromise) await r.startPromise;
    await this.stopRuntime(id, r, this.cfg.stopTimeoutMs);
  }

  async stopAll(): Promise<void> {
    await Promise.all([...this.rt.keys()].map((id) => this.stop(id)));
  }

  /** 新凭证 / 删除后清掉崩溃退避与死号状态。 */
  resetRuntime(id: string): void {
    this.rt.delete(id);
  }

  /** 闲置停止：从最后一次 ensure 起超过 idleStopMs 的运行中进程。 */
  async idleSweep(now = Date.now()): Promise<void> {
    for (const [id, r] of this.rt) {
      if (r.state === 'running' && now - r.lastUsed >= this.cfg.idleStopMs) {
        this.log(`[supervisor] ${id}: 闲置 ${Math.round((now - r.lastUsed) / 60_000)} 分钟，停止进程`);
        await this.stop(id);
      }
    }
  }

  /** 轮询运行中进程的 /accounts，判定 refresh token 是否已死。 */
  async pollHealth(now = Date.now()): Promise<void> {
    for (const [id, r] of this.rt) {
      if (r.state !== 'running') continue;
      const rec = this.store.get(id);
      if (!rec) continue;
      let st: SeatStatus | null = null;
      try {
        const res = await fetch(`http://127.0.0.1:${rec.port}/accounts`, {
          headers: { 'x-api-key': this.cfg.apiKey },
          signal: AbortSignal.timeout(3_000),
        });
        if (res.ok) st = parseSeatStatus(await res.json());
      } catch {
        continue;
      }
      if (!st) continue;
      r.status = st;
      r.statusAt = now;
      const expired = st.expiresInMs !== null && st.expiresInMs < 0;
      if (expired) r.expiredSince ??= now;
      else r.expiredSince = null;
      const dead = st.action === 'regrant' || (r.expiredSince !== null && now - r.expiredSince >= EXPIRED_DEAD_AFTER_MS);
      if (dead) {
        this.log(`[supervisor] ${id}: 判定 refresh token 已失效（action=${st.action} expiresInMs=${st.expiresInMs}），标记需要重新登录并停止进程`);
        this.store.update(id, { needsRelogin: true });
        await this.stop(id);
      }
    }
  }

  startTimers(): void {
    const idleEvery = Math.max(1_000, Math.min(60_000, Math.floor(this.cfg.idleStopMs / 2)));
    this.timers.push(setInterval(() => { void this.idleSweep(); }, idleEvery));
    this.timers.push(setInterval(() => { void this.pollHealth(); }, this.cfg.healthPollMs));
  }

  stopTimers(): void {
    for (const t of this.timers) clearInterval(t);
    this.timers = [];
  }

  /** 对外状态。dario 的读数放在 `seat` 下（不叫 status，免得和接口的操作结果字段撞名）。 */
  view(id: string): { state: ProcState; pid: number | null; lastUsed: number; crashes: number; backoffUntil: number; seat: SeatStatus | null; seatAt: number } {
    const r = this.rt.get(id);
    return {
      state: r?.state ?? 'stopped',
      pid: r?.child?.pid ?? null,
      lastUsed: r?.lastUsed ?? 0,
      crashes: r?.crashes ?? 0,
      backoffUntil: r?.backoffUntil ?? 0,
      seat: r?.status ?? null,
      seatAt: r?.statusAt ?? 0,
    };
  }
}
