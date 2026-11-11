import type { CacheStrategy, CreditModelSpec, SystemSettings, SystemSettingsPatch } from './types'

/** 参数表的一行（数字以字符串存，提交时解析）。 */
export interface CreditModelRow {
  model: string
  miss: string
  hit: string
  out: string
  base: string
}

/** 缓存计费策略段的表单态。 */
export interface CacheStrategyForm {
  cache_strategy: CacheStrategy
  cache_credit_max_output: string
  cache_credit_discount_only: boolean
  cache_credit_enterprise: boolean
  cache_io_correction: boolean
  models: CreditModelRow[]
  /** 点了「恢复内置参数」：提交时发 null，让后端回到内置表。 */
  resetModels: boolean
}

/** 后端基线默认值。旧版本后端不返回这些字段时按它显示，patch 也按它比较。 */
export const CACHE_STRATEGY_DEFAULTS = {
  cache_strategy: 'sim' as CacheStrategy,
  cache_credit_max_output: 1500,
  cache_credit_discount_only: false,
  cache_credit_enterprise: true,
  cache_io_correction: false,
}

/** 与后端 `CREDIT_MODELS_MAX`、`cache_credit_max_output` 校验一致。 */
export const CREDIT_MODELS_MAX = 32
export const CREDIT_MAX_OUTPUT_LIMIT = 100000

export type CreditRowError = 'model' | 'duplicate' | 'number' | 'order' | 'out' | 'base'

export interface CacheStrategyErrors {
  maxOutput?: 'invalid' | 'outOfRange'
  tooMany?: true
  /** 行号 → 错误码。 */
  rows: Record<number, CreditRowError>
}

const rowFromSpec = (model: string, s: CreditModelSpec): CreditModelRow => ({
  model,
  miss: String(s.miss),
  hit: String(s.hit),
  out: String(s.out),
  base: String(s.base ?? 0),
})

/** GET 响应 → 表单态。缺字段（旧后端）补基线默认。 */
export function cacheStrategyFormFromSettings(s: SystemSettings): CacheStrategyForm {
  return {
    cache_strategy: s.cache_strategy ?? CACHE_STRATEGY_DEFAULTS.cache_strategy,
    cache_credit_max_output: String(s.cache_credit_max_output ?? CACHE_STRATEGY_DEFAULTS.cache_credit_max_output),
    cache_credit_discount_only: s.cache_credit_discount_only ?? CACHE_STRATEGY_DEFAULTS.cache_credit_discount_only,
    cache_credit_enterprise: s.cache_credit_enterprise ?? CACHE_STRATEGY_DEFAULTS.cache_credit_enterprise,
    cache_io_correction: s.cache_io_correction ?? CACHE_STRATEGY_DEFAULTS.cache_io_correction,
    models: Object.entries(s.cache_credit_models ?? {}).map(([m, spec]) => rowFromSpec(m, spec)),
    resetModels: false,
  }
}

const num = (raw: string): number | null => {
  const n = Number(raw)
  return raw.trim() === '' || !Number.isFinite(n) ? null : n
}

/** 解析并校验一行；规则与后端 `CreditModelSpec::validate` 一致。 */
function parseRow(r: CreditModelRow): { spec: CreditModelSpec } | { error: CreditRowError } {
  const model = r.model.trim()
  if (model === '' || model.length > 64) return { error: 'model' }
  const [miss, hit, out, base] = [num(r.miss), num(r.hit), num(r.out), num(r.base)]
  if (miss === null || hit === null || out === null || base === null) return { error: 'number' }
  if (!(hit >= 0 && miss > hit)) return { error: 'order' }
  if (out < 0) return { error: 'out' }
  if (Math.abs(base) > 1) return { error: 'base' }
  return { spec: { miss, hit, out, base } }
}

export function validateCacheStrategyForm(form: CacheStrategyForm): CacheStrategyErrors {
  const errors: CacheStrategyErrors = { rows: {} }
  const n = num(form.cache_credit_max_output)
  if (n === null || !Number.isSafeInteger(n)) errors.maxOutput = 'invalid'
  else if (n < 0 || n > CREDIT_MAX_OUTPUT_LIMIT) errors.maxOutput = 'outOfRange'
  if (form.resetModels) return errors
  if (form.models.length > CREDIT_MODELS_MAX) errors.tooMany = true
  const seen = new Set<string>()
  form.models.forEach((r, i) => {
    const parsed = parseRow(r)
    if ('error' in parsed) errors.rows[i] = parsed.error
    else if (seen.has(r.model.trim())) errors.rows[i] = 'duplicate'
    seen.add(r.model.trim())
  })
  return errors
}

export const hasCacheStrategyErrors = (e: CacheStrategyErrors) =>
  e.maxOutput !== undefined || e.tooMany === true || Object.keys(e.rows).length > 0

/** 键排序后的规范串，用来判断表是否真的改过（行顺序不算改动）。 */
const canonical = (m: Record<string, CreditModelSpec>) =>
  JSON.stringify(
    Object.keys(m)
      .sort()
      .map((k) => [k, m[k].miss, m[k].hit, m[k].out, m[k].base ?? 0]),
  )

/**
 * 只返回与服务器当前值不同的字段（增量 PATCH）。含非法值的字段不提交
 * （正常路径下 validateCacheStrategyForm 已先拦住保存）。
 */
export function buildCacheStrategyPatch(form: CacheStrategyForm, server: SystemSettings): SystemSettingsPatch {
  const patch: SystemSettingsPatch = {}
  if (form.cache_strategy !== (server.cache_strategy ?? CACHE_STRATEGY_DEFAULTS.cache_strategy)) {
    patch.cache_strategy = form.cache_strategy
  }
  const n = num(form.cache_credit_max_output)
  const serverMax = server.cache_credit_max_output ?? CACHE_STRATEGY_DEFAULTS.cache_credit_max_output
  if (n !== null && Number.isSafeInteger(n) && n >= 0 && n <= CREDIT_MAX_OUTPUT_LIMIT && n !== serverMax) {
    patch.cache_credit_max_output = n
  }
  const serverDiscount = server.cache_credit_discount_only ?? CACHE_STRATEGY_DEFAULTS.cache_credit_discount_only
  if (form.cache_credit_discount_only !== serverDiscount) {
    patch.cache_credit_discount_only = form.cache_credit_discount_only
  }
  if (form.cache_credit_enterprise !== (server.cache_credit_enterprise ?? CACHE_STRATEGY_DEFAULTS.cache_credit_enterprise)) {
    patch.cache_credit_enterprise = form.cache_credit_enterprise
  }
  if (form.cache_io_correction !== (server.cache_io_correction ?? CACHE_STRATEGY_DEFAULTS.cache_io_correction)) {
    patch.cache_io_correction = form.cache_io_correction
  }
  if (form.resetModels) {
    patch.cache_credit_models = null
    return patch
  }
  const parsed = form.models.map(parseRow)
  if (parsed.every((p): p is { spec: CreditModelSpec } => 'spec' in p)) {
    const table = Object.fromEntries(form.models.map((r, i) => [r.model.trim(), parsed[i].spec]))
    if (Object.keys(table).length === form.models.length && canonical(table) !== canonical(server.cache_credit_models ?? {})) {
      patch.cache_credit_models = table
    }
  }
  return patch
}
