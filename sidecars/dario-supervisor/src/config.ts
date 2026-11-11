/** 运行参数，全部来自环境变量（容器内由 compose 注入）。 */
export interface Config {
  host: string;
  port: number;
  /** 管理器自身鉴权与每个 dario 进程的 DARIO_API_KEY 共用同一把 key。 */
  apiKey: string;
  /** 原版 dario 的 dist/cli.js。 */
  darioCli: string;
  /** 用来启动 dario 的 bun。dario 必须直接跑在 bun 上：node 下它会再派生 bun 子进程且不转发 SIGTERM。 */
  bunBin: string;
  dataDir: string;
  portMin: number;
  portMax: number;
  /** 多久没有请求就停掉进程。dario 的在线心跳每 5 秒一次且不能关，常驻不像真人。 */
  idleStopMs: number;
  /** 冷启动等待 /livez 就绪的上限（含 dario 启动时可能做的一次令牌刷新）。 */
  startTimeoutMs: number;
  /** 运行中进程的 /accounts 轮询间隔。 */
  healthPollMs: number;
  /** 进程停止时等它排空在途请求的上限（dario 自己的 --shutdown-grace 默认 90s）。 */
  stopTimeoutMs: number;
  maxBodyMb: number;
}

function int(name: string, fallback: number): number {
  const raw = process.env[name];
  if (raw === undefined || raw === '') return fallback;
  const n = Number(raw);
  if (!Number.isInteger(n) || n < 0) throw new Error(`${name} 须为非负整数，收到 ${raw}`);
  return n;
}

export function configFromEnv(): Config {
  const apiKey = process.env.DARIO_API_KEY ?? '';
  if (apiKey.length < 16) throw new Error('DARIO_API_KEY 未设置或过短（至少 16 字符）');
  const cfg: Config = {
    host: process.env.SUPERVISOR_HOST ?? '127.0.0.1',
    port: int('SUPERVISOR_PORT', 8788),
    apiKey,
    darioCli: process.env.DARIO_CLI ?? '/opt/dario/dist/cli.js',
    bunBin: process.env.BUN_BIN ?? process.execPath,
    dataDir: process.env.DATA_DIR ?? '/data',
    portMin: int('PORT_MIN', 18800),
    portMax: int('PORT_MAX', 18999),
    idleStopMs: int('IDLE_STOP_MS', 30 * 60_000),
    startTimeoutMs: int('START_TIMEOUT_MS', 15_000),
    healthPollMs: int('HEALTH_POLL_MS', 30_000),
    stopTimeoutMs: int('STOP_TIMEOUT_MS', 95_000),
    maxBodyMb: int('DARIO_MAX_BODY_MB', 16),
  };
  if (cfg.portMin > cfg.portMax) throw new Error('PORT_MIN 不能大于 PORT_MAX');
  return cfg;
}
