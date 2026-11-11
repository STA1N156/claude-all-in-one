import { api } from '@/lib/api'

import type { CreateNodeResult, ExecResult, NodesResponse, RevokeNodeResult } from './types'

/** GET /admin/api/nodes → 库里的子机记录 + hub 的实时状态。 */
export async function fetchNodes(): Promise<NodesResponse> {
  const response = await api.get<NodesResponse>('/nodes')
  return response.data
}

/** POST /admin/api/nodes → 新建一台子机,返回只显示一次的加入令牌与安装命令。 */
export async function createNode(name: string): Promise<CreateNodeResult> {
  const response = await api.post<CreateNodeResult>('/nodes', { name })
  return response.data
}

/** PATCH /admin/api/nodes/{id} → 改名 / 排空(disabled)。 */
export async function updateNode(
  id: string,
  patch: { name?: string; disabled?: boolean },
): Promise<void> {
  await api.patch(`/nodes/${encodeURIComponent(id)}`, patch)
}

/**
 * DELETE /admin/api/nodes/{id} → 吊销(hub 几秒内断开它)。
 *
 * 上面还有号时后端回 409:号绑死在子机上,吊销后那些号再也调不动,必须 force 确认。
 */
export async function deleteNode(id: string, force: boolean): Promise<RevokeNodeResult> {
  const response = await api.delete<RevokeNodeResult>(`/nodes/${encodeURIComponent(id)}`, {
    params: force ? { force: true } : {},
  })
  return response.data
}

/** POST /admin/api/nodes/{id}/reset-binding → 解除机器绑定(子机重装后用同一令牌再连)。 */
export async function resetNodeBinding(id: string): Promise<void> {
  await api.post(`/nodes/${encodeURIComponent(id)}/reset-binding`)
}

/** POST /admin/api/nodes/{id}/exec → 在子机上跑一条 bash(后台终端,以 caio 用户、清空环境执行)。 */
export async function execOnNode(
  id: string,
  command: string,
  timeoutSecs: number,
  signal?: AbortSignal,
): Promise<ExecResult> {
  // 子机侧上限 120s + 链路余量;axios 实例默认 30s 会在长跑命令上提前断开。
  const response = await api.post<ExecResult>(
    `/nodes/${encodeURIComponent(id)}/exec`,
    { command, timeout_secs: timeoutSecs },
    { timeout: 150_000, signal },
  )
  return response.data
}
