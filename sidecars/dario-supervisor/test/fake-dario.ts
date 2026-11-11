/**
 * 测试用假 dario：`bun fake-dario.ts proxy --port=N ...`。
 * 行为由 $HOME/fake.json 控制（每次请求重新读取）：{ action?, expiresInMs?, crash? }。
 * 启动时把 argv 与环境变量写到 $HOME/launch.json，供测试检查参数与环境白名单。
 */
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const home = process.env.HOME ?? '.';
const ctl = () => (existsSync(join(home, 'fake.json')) ? JSON.parse(readFileSync(join(home, 'fake.json'), 'utf8')) : {});
writeFileSync(join(home, 'launch.json'), JSON.stringify({ argv: process.argv.slice(2), env: process.env, cwd: process.cwd() }));
if (ctl().crash) process.exit(3);

const port = Number(process.argv.find((a) => a.startsWith('--port='))?.slice(7));
const key = process.env.DARIO_API_KEY ?? '';
const server = Bun.serve({
  hostname: '127.0.0.1',
  port,
  fetch(req) {
    const url = new URL(req.url);
    if (url.pathname === '/livez') return Response.json({ status: 'ok' });
    if (req.headers.get('x-api-key') !== key) return Response.json({ error: 'Unauthorized' }, { status: 401 });
    if (url.pathname === '/accounts') {
      const c = ctl();
      return Response.json({ mode: 'pool', accounts: [{ alias: 'seat', action: c.action ?? 'none', status: 'allowed', expiresInMs: c.expiresInMs ?? 3_600_000, util5h: 0.1, util7d: 0.2, grantLevel: 'ok', requestCount: 1, rejectedCount: 0 }] });
    }
    return Response.json({ error: 'not found' }, { status: 404 });
  },
});
process.on('SIGTERM', () => { server.stop(); process.exit(0); });
