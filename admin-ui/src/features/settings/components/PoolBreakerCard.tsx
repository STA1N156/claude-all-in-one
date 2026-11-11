import { useId } from 'react'
import { OctagonAlert } from 'lucide-react'

import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Select } from '@/components/ui/select'
import { useI18n } from '@/lib/i18n'

import type { BatchGuardMode, SystemSettings, SystemSettingsPatch } from '../types'
import { BATCH_GUARD_MODES } from './BatchGuardCard'

const inputClass =
  'w-full rounded-2xl border bg-input px-4 py-2.5 text-sm text-foreground outline-none transition-colors placeholder:text-muted-foreground'

/** 与后端 `gw_core::config::pool_breaker` 默认值一致(旧后端不返回时按它们显示/比较)。 */
const DEFAULTS = { mode: 'enforce' as BatchGuardMode, windowSecs: 1800, threshold: 3 }

/** 表单态:窗口按分钟填(后端存秒)。 */
export interface PoolBreakerForm {
  mode: BatchGuardMode
  window_minutes: string
  threshold: string
}

export function poolBreakerToForm(s: SystemSettings): PoolBreakerForm {
  return {
    mode: s.pool_breaker_mode ?? DEFAULTS.mode,
    window_minutes: String(Math.round((s.pool_breaker_window_secs ?? DEFAULTS.windowSecs) / 60)),
    threshold: String(s.pool_breaker_threshold ?? DEFAULTS.threshold),
  }
}

/** 只回传变了的键;非法数字不提交。 */
export function poolBreakerPatch(form: PoolBreakerForm, original: SystemSettings): SystemSettingsPatch {
  const patch: SystemSettingsPatch = {}
  const strictInt = (s: string): number | null => {
    const n = Number(s)
    return s.trim() !== '' && Number.isInteger(n) && n >= 0 ? n : null
  }
  if (form.mode !== (original.pool_breaker_mode ?? DEFAULTS.mode)) {
    patch.pool_breaker_mode = form.mode
  }
  const minutes = strictInt(form.window_minutes)
  if (minutes !== null && minutes * 60 !== (original.pool_breaker_window_secs ?? DEFAULTS.windowSecs)) {
    patch.pool_breaker_window_secs = minutes * 60
  }
  const threshold = strictInt(form.threshold)
  if (threshold !== null && threshold !== (original.pool_breaker_threshold ?? DEFAULTS.threshold)) {
    patch.pool_breaker_threshold = threshold
  }
  return patch
}

interface PoolBreakerCardProps {
  form: PoolBreakerForm | null
  disabled: boolean
  onChange: (next: PoolBreakerForm) => void
}

/** 「号池熔断」段。熔断状态与恢复按钮在页面顶部横幅(PoolBreakerBanner)。 */
export function PoolBreakerCard({ form, disabled, onChange }: PoolBreakerCardProps) {
  const { t } = useI18n()
  const idBase = useId()
  const set = (patch: Partial<PoolBreakerForm>) => {
    if (form !== null) onChange({ ...form, ...patch })
  }
  const mode = form?.mode ?? DEFAULTS.mode
  const numField = (key: 'window_minutes' | 'threshold', label: string, hint: string, min: number) => (
    <div className="space-y-1.5">
      <label htmlFor={`${idBase}-${key}`} className="text-xs font-medium text-muted-foreground">
        {label}
      </label>
      <input
        id={`${idBase}-${key}`}
        type="number"
        step={1}
        min={min}
        value={form?.[key] ?? ''}
        disabled={disabled}
        onChange={(e) => set({ [key]: e.target.value })}
        className={inputClass}
      />
      <p className="text-xs text-muted-foreground">{hint}</p>
    </div>
  )
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2 text-base">
          <OctagonAlert className="h-4 w-4 text-primary" />
          {t('settings.section.poolBreaker')}
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-5">
        <p className="text-xs text-muted-foreground">{t('settings.poolBreaker.desc')}</p>
        <div className="space-y-1.5">
          <label htmlFor={`${idBase}-mode`} className="text-xs font-medium text-muted-foreground">
            {t('settings.poolBreaker.mode')}
          </label>
          <Select
            id={`${idBase}-mode`}
            value={mode}
            disabled={disabled}
            onChange={(e) => set({ mode: e.target.value as BatchGuardMode })}
            className="w-full sm:w-72"
          >
            {BATCH_GUARD_MODES.map((m) => (
              <option key={m} value={m}>
                {t(`settings.poolBreaker.option.${m}`)}
              </option>
            ))}
          </Select>
          <p className="text-xs text-muted-foreground">{t(`settings.poolBreaker.hint.${mode}`)}</p>
        </div>
        <div className="grid gap-4 sm:grid-cols-2">
          {numField('window_minutes', t('settings.poolBreaker.windowMinutes'), t('settings.poolBreaker.windowMinutesHint'), 5)}
          {numField('threshold', t('settings.poolBreaker.threshold'), t('settings.poolBreaker.thresholdHint'), 2)}
        </div>
      </CardContent>
    </Card>
  )
}
