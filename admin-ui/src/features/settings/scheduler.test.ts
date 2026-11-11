import { describe, expect, it } from 'vitest'

import {
  SCHEDULER_DEFAULTS,
  SCHEDULER_FIELDS,
  buildSchedulerPatch,
  schedulerFormFromSettings,
  validateSchedulerForm,
} from './scheduler'
import type { SystemSettings } from './types'

/** 旧版本后端的 GET 响应：只有必填字段，调度器相关字段一个都不带。 */
function legacySettings(): SystemSettings {
  return {
    default_proxy: null,
    egress_pool: null,
    cache_read_multiplier: 0.1,
    cache_cap_ratio: 0.9,
    cache_floor_ratio: 0.6,
    cache_sim_ttl_secs: 300,
    cache_max_sessions: 1000,
    rate_limit_cooldown_secs: 60,
    suspended_cooldown_secs: 3600,
    empty_response_cooldown_secs: 60,
    empty_response_window_secs: 60,
    empty_response_threshold: 3,
    affinity_ttl_secs: 1800,
    max_failures: 5,
    max_switch_attempts: 2,
    quota_poll_enabled: true,
    image_enabled: true,
    image_max_long_edge: 2000,
    image_max_pixels_single: 3750000,
    image_max_pixels_multi: 1150000,
    image_multi_threshold: 20,
    tools_in_prefix: false,
    cache_point: false,
    agent_continuation: false,
    thinking_signature: true,
    q_endpoint: false,
    default_thinking_effort: 'high',
    history_thinking_turns: 0,
  }
}

/** 新后端：有效视图里全部字段都在，且都不是默认值。 */
function tunedSettings(): SystemSettings {
  return {
    ...legacySettings(),
    scheduler_algorithm: 'affinity',
    cost_index_kappa: 0.5,
    cost_index_eta: 7.25,
    cost_index_start_spacing_ms: 500,
    cost_index_pace_ms: 2000,
    cost_index_pace_max_strikes: 5,
    cost_index_pace_personal: true,
    cost_index_max_wait_ms: 15000,
    cost_index_enterprise_price: 2.5,
    cost_index_cache_ttl_secs: 240,
    affinity_hold_ms: 800,
    affinity_migrate_up: false,
    queue_wait_ms: 3000,
    tier_hold_ms: 1500,
    tier_hold_window_ms: 5000,
    rate_limit_pace_ms: 1200,
    rate_limit_pace_max_strikes: 4,
  }
}

describe('schedulerFormFromSettings', () => {
  it('旧后端缺字段时全部按基线默认显示', () => {
    const form = schedulerFormFromSettings(legacySettings())
    expect(form.scheduler_algorithm).toBe('cost_index')
    expect(form.cost_index_kappa).toBe('1')
    expect(form.cost_index_eta).toBe('3')
    expect(form.cost_index_start_spacing_ms).toBe('1000')
    expect(form.cost_index_max_wait_ms).toBe('30000')
    expect(form.cost_index_cache_ttl_secs).toBe('300')
    expect(form.cost_index_pace_personal).toBe(false)
    expect(form.affinity_migrate_up).toBe(true)
    expect(form.affinity_hold_ms).toBe('0')
    expect(form.rate_limit_pace_max_strikes).toBe('0')
  })

  it('新后端的值原样进表单', () => {
    const form = schedulerFormFromSettings(tunedSettings())
    expect(form.scheduler_algorithm).toBe('affinity')
    expect(form.cost_index_eta).toBe('7.25')
    expect(form.cost_index_pace_personal).toBe(true)
    expect(form.affinity_migrate_up).toBe(false)
    expect(form.tier_hold_window_ms).toBe('5000')
  })
})

describe('buildSchedulerPatch', () => {
  it('未改动时不产生任何字段（旧后端 / 新后端都一样）', () => {
    for (const s of [legacySettings(), tunedSettings()]) {
      expect(buildSchedulerPatch(schedulerFormFromSettings(s), s)).toEqual({})
    }
  })

  it('只改算法 → 只带 scheduler_algorithm', () => {
    const s = legacySettings()
    const form = { ...schedulerFormFromSettings(s), scheduler_algorithm: 'affinity' as const }
    expect(buildSchedulerPatch(form, s)).toEqual({ scheduler_algorithm: 'affinity' })
  })

  it('浮点按数值比较：「1」「1.0」「1.00」都等于 1.0', () => {
    const s = legacySettings()
    const base = schedulerFormFromSettings(s)
    for (const v of ['1', '1.0', '1.00', ' 1 ']) {
      expect(buildSchedulerPatch({ ...base, cost_index_kappa: v }, s)).toEqual({})
    }
    expect(buildSchedulerPatch({ ...base, cost_index_kappa: '1.5' }, s)).toEqual({ cost_index_kappa: 1.5 })
  })

  it('开关切换 → 只带该开关', () => {
    const s = legacySettings()
    const base = schedulerFormFromSettings(s)
    expect(buildSchedulerPatch({ ...base, cost_index_pace_personal: true }, s)).toEqual({
      cost_index_pace_personal: true,
    })
    expect(buildSchedulerPatch({ ...base, affinity_migrate_up: false }, s)).toEqual({
      affinity_migrate_up: false,
    })
    // 新后端上把值改回默认，也是一次真实改动，要提交。
    const tuned = tunedSettings()
    expect(
      buildSchedulerPatch({ ...schedulerFormFromSettings(tuned), affinity_migrate_up: true }, tuned),
    ).toEqual({ affinity_migrate_up: true })
  })

  it('旧版亲和参数改动只带该字段，整数按整数提交', () => {
    const s = legacySettings()
    const form = { ...schedulerFormFromSettings(s), tier_hold_ms: '2500' }
    expect(buildSchedulerPatch(form, s)).toEqual({ tier_hold_ms: 2500 })
  })

  it('非法 / 越界值不提交', () => {
    const s = legacySettings()
    const base = schedulerFormFromSettings(s)
    expect(
      buildSchedulerPatch(
        { ...base, cost_index_start_spacing_ms: '1.5', cost_index_eta: 'abc', cost_index_kappa: '101' },
        s,
      ),
    ).toEqual({})
  })
})

describe('validateSchedulerForm', () => {
  it('默认值全部合法', () => {
    expect(validateSchedulerForm(schedulerFormFromSettings(legacySettings()))).toEqual({})
    expect(validateSchedulerForm(schedulerFormFromSettings(tunedSettings()))).toEqual({})
  })

  it('整数字段拒绝小数、负数、越界', () => {
    const base = schedulerFormFromSettings(legacySettings())
    expect(validateSchedulerForm({ ...base, cost_index_start_spacing_ms: '1.5' })).toEqual({
      cost_index_start_spacing_ms: 'notInteger',
    })
    expect(validateSchedulerForm({ ...base, cost_index_start_spacing_ms: '-1' })).toEqual({
      cost_index_start_spacing_ms: 'outOfRange',
    })
    expect(validateSchedulerForm({ ...base, cost_index_start_spacing_ms: '10001' })).toEqual({
      cost_index_start_spacing_ms: 'outOfRange',
    })
    expect(validateSchedulerForm({ ...base, cost_index_cache_ttl_secs: '0' })).toEqual({
      cost_index_cache_ttl_secs: 'outOfRange',
    })
    expect(validateSchedulerForm({ ...base, affinity_hold_ms: '-1' })).toEqual({
      affinity_hold_ms: 'outOfRange',
    })
    expect(validateSchedulerForm({ ...base, queue_wait_ms: '1e300' })).toEqual({
      queue_wait_ms: 'notInteger',
    })
  })

  it('浮点字段接受小数、拒绝越界与非数字', () => {
    const base = schedulerFormFromSettings(legacySettings())
    expect(validateSchedulerForm({ ...base, cost_index_kappa: '0.25' })).toEqual({})
    expect(validateSchedulerForm({ ...base, cost_index_kappa: '100.5' })).toEqual({
      cost_index_kappa: 'outOfRange',
    })
    expect(validateSchedulerForm({ ...base, cost_index_enterprise_price: '0' })).toEqual({
      cost_index_enterprise_price: 'outOfRange',
    })
    expect(validateSchedulerForm({ ...base, cost_index_eta: '' })).toEqual({ cost_index_eta: 'invalid' })
    expect(validateSchedulerForm({ ...base, cost_index_eta: '3x' })).toEqual({ cost_index_eta: 'invalid' })
  })

  it('未选中算法的字段同样校验（切走也拦得住）', () => {
    const base = schedulerFormFromSettings(legacySettings())
    expect(base.scheduler_algorithm).toBe('cost_index')
    expect(validateSchedulerForm({ ...base, tier_hold_window_ms: '0.5' })).toEqual({
      tier_hold_window_ms: 'notInteger',
    })
  })

  it('与服务器当前值相同的字段放行（基线越界也不挡整页保存）', () => {
    const s: SystemSettings = { ...legacySettings(), cost_index_kappa: 200 }
    const form = schedulerFormFromSettings(s)
    expect(validateSchedulerForm(form)).toEqual({ cost_index_kappa: 'outOfRange' })
    expect(validateSchedulerForm(form, s)).toEqual({})
    // 一旦改动，就按范围校验。
    expect(validateSchedulerForm({ ...form, cost_index_kappa: '150' }, s)).toEqual({
      cost_index_kappa: 'outOfRange',
    })
  })

  it('默认值落在各自范围内', () => {
    const form = schedulerFormFromSettings({ ...legacySettings(), ...SCHEDULER_DEFAULTS })
    expect(validateSchedulerForm(form)).toEqual({})
  })
})

describe('排队号 429 节流的可见性', () => {
  // 2026-09-30 事故：排队号在新算法下曾改用 cost_index_pace_*，熔断 3 次就进冷却。
  // 修正后排队号两种算法都只认 rate_limit_pace_*，所以两个面板都必须能看到、能改它们。
  it('rate_limit_pace_* 在两种算法的面板里都显示', () => {
    for (const key of ['rate_limit_pace_ms', 'rate_limit_pace_max_strikes'] as const) {
      const spec = SCHEDULER_FIELDS.find((f) => f.key === key)
      expect(spec?.algorithms).toEqual(['cost_index', 'affinity'])
    }
  })
})
