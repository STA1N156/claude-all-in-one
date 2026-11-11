import { describe, expect, it } from 'vitest'

import {
  buildEnvNormalizePatch,
  envNormalizeFormFromSettings,
  hasEnvNormalizeErrors,
  toggleFamily,
  validateEnvNormalizeForm,
} from './envNormalize'
import type { SystemSettings } from './types'

const base = { cache_read_multiplier: 1 } as unknown as SystemSettings

describe('envNormalize form', () => {
  it('旧后端：取默认值，原样提交不产生任何字段', () => {
    const form = envNormalizeFormFromSettings(base)
    expect(form).toEqual({ providers: [], timezone: 'Asia/Singapore', locale: 'en-US' })
    expect(buildEnvNormalizePatch(form, base)).toEqual({})
  })

  it('只提交改动的字段；线路顺序不算改动', () => {
    const server = { ...base, env_normalize_providers: ['kiro', 'claude-dario'], env_normalize_timezone: 'UTC', env_normalize_locale: 'en-US' } as SystemSettings
    const form = envNormalizeFormFromSettings(server)
    expect(buildEnvNormalizePatch(form, server)).toEqual({})
    const edited = { ...toggleFamily(form, 'cursor', true), timezone: ' America/New_York ' }
    expect(buildEnvNormalizePatch(edited, server)).toEqual({
      env_normalize_providers: ['claude-dario', 'cursor', 'kiro'],
      env_normalize_timezone: 'America/New_York',
    })
    expect(toggleFamily(edited, 'kiro', false).providers).toEqual(['claude-dario', 'cursor'])
  })

  it('校验规则与后端一致，非法值不提交', () => {
    const form = { providers: [], timezone: 'Shanghai', locale: 'zh_CN' }
    const e = validateEnvNormalizeForm(form)
    expect(e).toEqual({ timezone: true, locale: true })
    expect(hasEnvNormalizeErrors(e)).toBe(true)
    expect(buildEnvNormalizePatch(form, base)).toEqual({})
    expect(hasEnvNormalizeErrors(validateEnvNormalizeForm({ providers: [], timezone: 'Etc/GMT+8', locale: 'en' }))).toBe(false)
  })
})
