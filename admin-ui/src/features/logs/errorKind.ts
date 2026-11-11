import type { RequestLogRow } from './types'

/** 后端 `UpstreamErrorKind::ContentFiltered` 的 error_kind 串:上游内容安全分类器拒答。 */
export const CONTENT_FILTERED_KIND = 'ContentFiltered'

/**
 * 后端 `gw_core::batch_guard::LOG_KIND_BLOCKED`:caio 判定为脚本化批量任务、在选号前直接拒绝
 * (400,没有落到任何号上)。**改这个串要同步改后端**(两边单测钉住字面量)。
 */
export const BATCH_BLOCKED_KIND = 'BatchBlocked'

/** 是否为批量任务拦截拒绝的请求。 */
export function isBatchBlocked(row: Pick<RequestLogRow, 'error_kind'>): boolean {
  return row.error_kind === BATCH_BLOCKED_KIND
}

/**
 * 后端 `gates::refusal_guard::LOG_KIND_REPEAT`:同一条请求已被上游分类器拒答过、超过允许的
 * 重发次数,caio 在选号前本地回拒答(400,没有落到任何号上)。**改这个串要同步改后端**。
 */
export const REFUSAL_REPEAT_KIND = 'RefusalRepeat'

/** 是否为拒答记忆本地拦下的重发。 */
export function isRefusalRepeat(row: Pick<RequestLogRow, 'error_kind'>): boolean {
  return row.error_kind === REFUSAL_REPEAT_KIND
}

/** 是否为上游分类器拦截的请求(零产出直接报错,或答到一半 stop_reason=refusal)。 */
export function isClassifierBlocked(row: Pick<RequestLogRow, 'error_kind'>): boolean {
  return row.error_kind === CONTENT_FILTERED_KIND
}

/**
 * 从 response_payload 取客户端实际收到的错误文案(`{"type":"error","error":{"message":...}}`)。
 * 不是错误体(成功回复/旧日志/空串)或解析失败 → null。
 */
export function clientErrorMessage(responsePayload: string): string | null {
  if (!responsePayload) return null
  try {
    const v: unknown = JSON.parse(responsePayload)
    if (typeof v !== 'object' || v === null) return null
    const body = v as { type?: unknown; error?: { message?: unknown } }
    if (body.type !== 'error') return null
    const msg = body.error?.message
    return typeof msg === 'string' && msg.length > 0 ? msg : null
  } catch {
    return null
  }
}
