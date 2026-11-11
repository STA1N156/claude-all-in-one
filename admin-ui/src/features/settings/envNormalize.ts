import type { SystemSettings, SystemSettingsPatch } from './types'

/** 可开关的线路（与后端 `gw_core::envnorm::KNOWN_FAMILIES` 一致）。 */
export const ENV_NORMALIZE_FAMILIES = ['claude-dario', 'kiro', 'cursor'] as const
export type EnvNormalizeFamily = (typeof ENV_NORMALIZE_FAMILIES)[number]

/** 后端默认值。旧版本后端不返回这些字段时按它显示，patch 也按它比较。 */
export const ENV_NORMALIZE_DEFAULTS = {
  providers: [] as string[],
  timezone: 'Asia/Singapore',
  locale: 'en-US',
}

export interface EnvNormalizeForm {
  providers: string[]
  timezone: string
  locale: string
}

export interface EnvNormalizeErrors {
  timezone?: true
  locale?: true
}

/** 与后端 `valid_timezone` / `valid_locale` 同一规则。 */
const TIMEZONE_RE = /^(UTC|[A-Za-z]+(\/[A-Za-z0-9_+-]+){1,2})$/
const LOCALE_RE = /^[a-z]{2,3}(-[A-Z]{2})?$/

const sorted = (xs: string[]) => [...new Set(xs)].sort()

export function envNormalizeFormFromSettings(s: SystemSettings): EnvNormalizeForm {
  return {
    providers: sorted(s.env_normalize_providers ?? ENV_NORMALIZE_DEFAULTS.providers),
    timezone: s.env_normalize_timezone ?? ENV_NORMALIZE_DEFAULTS.timezone,
    locale: s.env_normalize_locale ?? ENV_NORMALIZE_DEFAULTS.locale,
  }
}

export function validateEnvNormalizeForm(f: EnvNormalizeForm): EnvNormalizeErrors {
  const e: EnvNormalizeErrors = {}
  if (!TIMEZONE_RE.test(f.timezone.trim())) e.timezone = true
  if (!LOCALE_RE.test(f.locale.trim())) e.locale = true
  return e
}

export const hasEnvNormalizeErrors = (e: EnvNormalizeErrors) => e.timezone === true || e.locale === true

/** 只返回与服务器当前值不同的字段（增量 PATCH）；非法值不提交。 */
export function buildEnvNormalizePatch(f: EnvNormalizeForm, server: SystemSettings): SystemSettingsPatch {
  const patch: SystemSettingsPatch = {}
  const cur = envNormalizeFormFromSettings(server)
  const providers = sorted(f.providers)
  if (JSON.stringify(providers) !== JSON.stringify(cur.providers)) patch.env_normalize_providers = providers
  const errors = validateEnvNormalizeForm(f)
  if (!errors.timezone && f.timezone.trim() !== cur.timezone) patch.env_normalize_timezone = f.timezone.trim()
  if (!errors.locale && f.locale.trim() !== cur.locale) patch.env_normalize_locale = f.locale.trim()
  return patch
}

export function toggleFamily(f: EnvNormalizeForm, family: EnvNormalizeFamily, on: boolean): EnvNormalizeForm {
  const set = new Set(f.providers)
  if (on) set.add(family)
  else set.delete(family)
  return { ...f, providers: sorted([...set]) }
}
