/**
 * dario-supervisor 入口：每个 Claude OAuth 号一个原版 dario 进程。设计见 README.md。
 */
import { closeSync, existsSync, openSync, readFileSync, rmSync, writeSync } from 'node:fs';
import { join } from 'node:path';
import { configFromEnv } from './config.ts';
import { ProcessManager } from './procman.ts';
import { Api } from './server.ts';
import { Store } from './store.ts';

/** 同一个数据目录只允许一个管理器：两个管理器会给同一个号各起一个 dario，令牌互相作废。 */
function acquireLock(dataDir: string): () => void {
  const path = join(dataDir, 'supervisor.pid');
  if (existsSync(path)) {
    const pid = Number(readFileSync(path, 'utf8').trim());
    let alive = false;
    if (Number.isInteger(pid) && pid > 0 && pid !== process.pid) {
      try { process.kill(pid, 0); alive = true; } catch { alive = false; }
    }
    if (alive) throw new Error(`数据目录 ${dataDir} 已被 pid ${pid} 的管理器占用`);
    rmSync(path, { force: true });
  }
  const fd = openSync(path, 'wx', 0o600);
  writeSync(fd, String(process.pid));
  closeSync(fd);
  return () => rmSync(path, { force: true });
}

const cfg = configFromEnv();
const store = new Store(cfg.dataDir);
const release = acquireLock(cfg.dataDir);
const pm = new ProcessManager(cfg, store);
const api = new Api(cfg, store, pm);

const server = Bun.serve({
  hostname: cfg.host,
  port: cfg.port,
  // 冷启动最多等 startTimeoutMs，停进程最多 stopTimeoutMs；Bun 默认 10s 空闲超时会先掐断连接。
  idleTimeout: 255,
  fetch: (req) => api.handle(req),
});
pm.startTimers();
console.log(`[supervisor] 监听 ${cfg.host}:${server.port}，已登记 ${store.all().length} 个号（启动时不主动拉起进程）`);

let shuttingDown = false;
async function shutdown(sig: string): Promise<void> {
  if (shuttingDown) return;
  shuttingDown = true;
  console.log(`[supervisor] 收到 ${sig}，停止所有 dario 进程（各自排空在途请求）`);
  pm.stopTimers();
  server.stop();
  await pm.stopAll();
  release();
  process.exit(0);
}
process.on('SIGTERM', () => { void shutdown('SIGTERM'); });
process.on('SIGINT', () => { void shutdown('SIGINT'); });
