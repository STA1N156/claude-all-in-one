import type { SchedulerAlgorithm, SystemSettings, SystemSettingsPatch } from './types'

/** 数值型调度器参数（表单里以字符串存，提交时解析）。 */
export type SchedulerNumberKey =
  | 'cost_index_kappa'
  | 'cost_index_eta'
  | 'cost_index_start_spacing_ms'
  | 'cost_index_pace_ms'
  | 'cost_index_pace_max_strikes'
  | 'cost_index_max_wait_ms'
  | 'cost_index_enterprise_price'
  | 'cost_index_cache_ttl_secs'
  | 'affinity_hold_ms'
  | 'queue_wait_ms'
  | 'tier_hold_ms'
  | 'tier_hold_window_ms'
  | 'rate_limit_pace_ms'
  | 'rate_limit_pace_max_strikes'

/** 开关型调度器参数。 */
export type SchedulerBoolKey = 'cost_index_pace_personal' | 'affinity_migrate_up'

export type SchedulerFieldKey = SchedulerNumberKey | SchedulerBoolKey

/** 调度器表单态：算法 + 数值（字符串）+ 开关。 */
export type SchedulerForm = { scheduler_algorithm: SchedulerAlgorithm } & Record<
  SchedulerNumberKey,
  string
> &
  Record<SchedulerBoolKey, boolean>

/** 单个字段的校验错误码（文案由组件按字段的 min/max 拼）。 */
export type SchedulerFieldError = 'invalid' | 'notInteger' | 'outOfRange'

export type SchedulerErrors = Partial<Record<SchedulerNumberKey, SchedulerFieldError>>

export type SchedulerFieldSpec =
  | {
      key: SchedulerNumberKey
      kind: 'int' | 'float'
      min: number
      /** 缺省 = 无上限（旧版亲和参数后端只要求 ≥ 0）。 */
      max?: number
      /** 在哪些算法的面板里显示。 */
      algorithms: readonly SchedulerAlgorithm[]
    }
  | {
      key: SchedulerBoolKey
      kind: 'bool'
      algorithms: readonly SchedulerAlgorithm[]
    }

/**
 * 后端基线默认值。GET 缺字段（旧版本后端）时按它显示，patch 也按它比较 ——
 * 两处必须同一口径，否则未改动的字段会被捎带给不认识它的旧后端（400）。
 */
export const SCHEDULER_DEFAULTS: { scheduler_algorithm: SchedulerAlgorithm } & Record<
  SchedulerNumberKey,
  number
> &
  Record<SchedulerBoolKey, boolean> = {
  scheduler_algorithm: 'cost_index',
  cost_index_kappa: 1.0,
  cost_index_eta: 3.0,
  cost_index_start_spacing_ms: 1000,
  cost_index_pace_ms: 1000,
  cost_index_pace_max_strikes: 3,
  cost_index_pace_personal: false,
  cost_index_max_wait_ms: 30000,
  cost_index_enterprise_price: 3.0,
  cost_index_cache_ttl_secs: 300,
  affinity_hold_ms: 0,
  affinity_migrate_up: true,
  queue_wait_ms: 0,
  tier_hold_ms: 0,
  tier_hold_window_ms: 0,
  rate_limit_pace_ms: 0,
  rate_limit_pace_max_strikes: 0,
}

const COST: readonly SchedulerAlgorithm[] = ['cost_index']
const AFFINITY: readonly SchedulerAlgorithm[] = ['affinity']
const BOTH: readonly SchedulerAlgorithm[] = ['cost_index', 'affinity']

/**
 * 字段元数据；数组顺序 = 面板里的显示顺序。范围与后端 PUT 校验一致（越界后端回 400）。
 * `affinity_migrate_up` 与排队号的 `rate_limit_pace_*` 两种算法共用，两个面板都显示、绑同一个值。
 */
export const SCHEDULER_FIELDS: readonly SchedulerFieldSpec[] = [
  { key: 'cost_index_kappa', kind: 'float', min: 0, max: 100, algorithms: COST },
  { key: 'cost_index_eta', kind: 'float', min: 0, max: 1000, algorithms: COST },
  { key: 'cost_index_start_spacing_ms', kind: 'int', min: 0, max: 10000, algorithms: COST },
  { key: 'cost_index_max_wait_ms', kind: 'int', min: 0, max: 120000, algorithms: COST },
  { key: 'cost_index_enterprise_price', kind: 'float', min: 0.01, max: 100, algorithms: COST },
  { key: 'cost_index_cache_ttl_secs', kind: 'int', min: 1, max: 3600, algorithms: COST },
  // 排队号的 429 节流两种算法共用（企业号不会被封，口径与算法无关），两个面板都显示。
  { key: 'rate_limit_pace_ms', kind: 'int', min: 0, algorithms: BOTH },
  { key: 'rate_limit_pace_max_strikes', kind: 'int', min: 0, algorithms: BOTH },
  { key: 'cost_index_pace_personal', kind: 'bool', algorithms: COST },
  { key: 'cost_index_pace_ms', kind: 'int', min: 0, max: 60000, algorithms: COST },
  { key: 'cost_index_pace_max_strikes', kind: 'int', min: 0, max: 100, algorithms: COST },
  { key: 'affinity_hold_ms', kind: 'int', min: 0, algorithms: AFFINITY },
  { key: 'queue_wait_ms', kind: 'int', min: 0, algorithms: AFFINITY },
  { key: 'tier_hold_ms', kind: 'int', min: 0, algorithms: AFFINITY },
  { key: 'tier_hold_window_ms', kind: 'int', min: 0, algorithms: AFFINITY },
  { key: 'affinity_migrate_up', kind: 'bool', algorithms: BOTH },
]

type NumberSpec = Extract<SchedulerFieldSpec, { kind: 'int' | 'float' }>
type BoolSpec = Extract<SchedulerFieldSpec, { kind: 'bool' }>

const NUMBER_FIELDS = SCHEDULER_FIELDS.filter((f): f is NumberSpec => f.kind !== 'bool')
const BOOL_FIELDS = SCHEDULER_FIELDS.filter((f): f is BoolSpec => f.kind === 'bool')

/** 服务器当前有效值（缺字段 = 旧后端 → 基线默认）。 */
const serverNumber = (s: SystemSettings, key: SchedulerNumberKey): number =>
  s[key] ?? SCHEDULER_DEFAULTS[key]
const serverBool = (s: SystemSettings, key: SchedulerBoolKey): boolean =>
  s[key] ?? SCHEDULER_DEFAULTS[key]

/**
 * 解析 + 校验一个数值字段。解析用 Number 而非 parseInt/parseFloat：'1.5' 填进整数字段、
 * '12abc' 这类输入不该被静默截断成别的值。整数用 isSafeInteger，顺带挡住 1e300 这种
 * JSON 能发、后端 u64 解析不了的值。
 */
function checkNumber(spec: NumberSpec, raw: string): { value: number } | { error: SchedulerFieldError } {
  const n = Number(raw)
  if (raw.trim() === '' || !Number.isFinite(n)) return { error: 'invalid' }
  if (spec.kind === 'int' && !Number.isSafeInteger(n)) return { error: 'notInteger' }
  if (n < spec.min || (spec.max !== undefined && n > spec.max)) return { error: 'outOfRange' }
  return { value: n }
}

/** GET 响应 → 调度器表单态。缺字段补基线默认（与 buildSchedulerPatch 的比较口径一致）。 */
export function schedulerFormFromSettings(s: SystemSettings): SchedulerForm {
  const numbers = Object.fromEntries(
    NUMBER_FIELDS.map((f) => [f.key, String(serverNumber(s, f.key))]),
  ) as Record<SchedulerNumberKey, string>
  const bools = Object.fromEntries(BOOL_FIELDS.map((f) => [f.key, serverBool(s, f.key)])) as Record<
    SchedulerBoolKey,
    boolean
  >
  return {
    scheduler_algorithm: s.scheduler_algorithm ?? SCHEDULER_DEFAULTS.scheduler_algorithm,
    ...numbers,
    ...bools,
  }
}

/**
 * 校验全部数值字段（不只当前算法的：切走的面板里若留着非法值，也得拦住保存）。
 *
 * 传了 `server` 时，与服务器当前值相同的字段一律放行 —— 它不会被提交，没理由拦。
 * 这样即使 YAML 基线本身越界，也不会让一个从没碰过的字段挡住整页保存。
 */
export function validateSchedulerForm(form: SchedulerForm, server?: SystemSettings): SchedulerErrors {
  const errors = NUMBER_FIELDS.flatMap((spec): Array<[SchedulerNumberKey, SchedulerFieldError]> => {
    const raw = form[spec.key]
    if (server && raw.trim() !== '' && Number(raw) === serverNumber(server, spec.key)) return []
    const r = checkNumber(spec, raw)
    return 'error' in r ? [[spec.key, r.error]] : []
  })
  return Object.fromEntries(errors) as SchedulerErrors
}

/**
 * 只返回与服务器当前值不同的字段（增量 PATCH）。比较对象是 `server[key] ?? 基线默认`：
 * 旧后端不返回某字段、而表单停在默认值时，该字段不会出现在 patch 里（旧后端的
 * deny_unknown_fields 会把整次保存判 400）。数值按数值比较（'1' 与 1.0 相等）；
 * 非法/越界值一律不提交（正常路径下 validateSchedulerForm 已先拦住保存）。
 */
export function buildSchedulerPatch(form: SchedulerForm, server: SystemSettings): SystemSettingsPatch {
  const algorithm: Array<[string, unknown]> =
    form.scheduler_algorithm !== (server.scheduler_algorithm ?? SCHEDULER_DEFAULTS.scheduler_algorithm)
      ? [['scheduler_algorithm', form.scheduler_algorithm]]
      : []
  const numbers = NUMBER_FIELDS.flatMap((spec): Array<[string, unknown]> => {
    const r = checkNumber(spec, form[spec.key])
    return 'value' in r && r.value !== serverNumber(server, spec.key) ? [[spec.key, r.value]] : []
  })
  const bools = BOOL_FIELDS.flatMap((spec): Array<[string, unknown]> =>
    form[spec.key] !== serverBool(server, spec.key) ? [[spec.key, form[spec.key]]] : [],
  )
  return Object.fromEntries([...algorithm, ...numbers, ...bools]) as SystemSettingsPatch
}
