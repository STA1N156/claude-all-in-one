import { useId } from 'react'

import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Segment } from '@/components/ui/segment'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/utils'

import {
  CREDIT_MAX_OUTPUT_LIMIT,
  CREDIT_MODELS_MAX,
  type CacheStrategyErrors,
  type CacheStrategyForm,
  type CreditModelRow,
} from '../cacheStrategy'
import { CACHE_STRATEGIES } from '../types'

const inputClass =
  'w-full rounded-2xl border bg-input px-4 py-2.5 text-sm text-foreground outline-none transition-colors placeholder:text-muted-foreground'

const NUMBER_COLUMNS = ['miss', 'hit', 'out', 'base'] as const

/**
 * 系统设置里的「缓存计费策略」段：上报 cache_read 的方式 + 积分反推参数表。
 *
 * 受控组件，状态由 SettingsPage 持有、随主表单一起保存（键都在 `/settings` overlay 里）。
 */
export function CacheStrategyCard({
  form,
  errors,
  disabled,
  onChange,
}: {
  form: CacheStrategyForm | null
  errors: CacheStrategyErrors
  disabled: boolean
  onChange: (next: CacheStrategyForm) => void
}) {
  const { t } = useI18n()
  const idBase = useId()
  const strategy = form?.cache_strategy ?? 'sim'
  const credit = strategy !== 'sim'

  const update = (patch: Partial<CacheStrategyForm>) => {
    if (form !== null && !disabled) onChange({ ...form, ...patch })
  }
  const setRow = (idx: number, patch: Partial<CreditModelRow>) =>
    update({ models: (form?.models ?? []).map((r, i) => (i === idx ? { ...r, ...patch } : r)), resetModels: false })
  const addRow = () =>
    update({ models: [...(form?.models ?? []), { model: '', miss: '', hit: '', out: '', base: '0' }], resetModels: false })
  const removeRow = (idx: number) =>
    update({ models: (form?.models ?? []).filter((_, i) => i !== idx), resetModels: false })

  const maxOutputId = `${idBase}-max-output`

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">{t('settings.section.cacheStrategy')}</CardTitle>
      </CardHeader>
      <CardContent className="space-y-5">
        <div className="space-y-2">
          <span className="block text-sm font-medium">{t('settings.cacheStrategy.strategy')}</span>
          <Segment
            options={CACHE_STRATEGIES.map((s) => ({ value: s, label: t(`settings.cacheStrategy.${s}`) }))}
            value={strategy}
            onChange={(v) => update({ cache_strategy: v })}
            className="max-w-full flex-wrap rounded-3xl sm:rounded-full"
          />
          <p className="text-xs text-muted-foreground">{t(`settings.cacheStrategy.${strategy}Desc`)}</p>
        </div>

        <label className="flex cursor-pointer items-start gap-3">
          <input
            type="checkbox"
            checked={form?.cache_io_correction ?? false}
            onChange={(e) => update({ cache_io_correction: e.target.checked })}
            disabled={disabled}
            className="mt-0.5 h-4 w-4 shrink-0 rounded"
          />
          <span>
            <span className="block text-sm font-medium">{t('settings.cacheStrategy.ioCorrection')}</span>
            <span className="block text-xs text-muted-foreground">{t('settings.cacheStrategy.ioCorrectionHint')}</span>
          </span>
        </label>

        <div className={cn('grid gap-4 sm:grid-cols-2', !credit && 'opacity-60')}>
          <div className="space-y-1.5">
            <label htmlFor={maxOutputId} className="text-xs font-medium text-muted-foreground">
              {t('settings.cacheStrategy.maxOutput')}
            </label>
            <input
              id={maxOutputId}
              type="number"
              step={1}
              min={0}
              max={CREDIT_MAX_OUTPUT_LIMIT}
              value={form?.cache_credit_max_output ?? ''}
              onChange={(e) => update({ cache_credit_max_output: e.target.value })}
              disabled={disabled}
              aria-invalid={errors.maxOutput !== undefined}
              className={cn(inputClass, errors.maxOutput !== undefined && '!border-rose-500 dark:!border-rose-400')}
            />
            {errors.maxOutput !== undefined && (
              <p className="text-xs text-rose-600 dark:text-rose-300">
                {t('settings.schedulerAlgo.error.range', { min: 0, max: CREDIT_MAX_OUTPUT_LIMIT })}
              </p>
            )}
            <p className="text-xs text-muted-foreground">{t('settings.cacheStrategy.maxOutputHint')}</p>
          </div>
          <label className="flex cursor-pointer items-start gap-3">
            <input
              type="checkbox"
              checked={form?.cache_credit_discount_only ?? false}
              onChange={(e) => update({ cache_credit_discount_only: e.target.checked })}
              disabled={disabled}
              className="mt-0.5 h-4 w-4 shrink-0 rounded"
            />
            <span>
              <span className="block text-sm font-medium">{t('settings.cacheStrategy.discountOnly')}</span>
              <span className="block text-xs text-muted-foreground">{t('settings.cacheStrategy.discountOnlyHint')}</span>
            </span>
          </label>
          <label className="flex cursor-pointer items-start gap-3">
            <input
              type="checkbox"
              checked={form?.cache_credit_enterprise ?? true}
              onChange={(e) => update({ cache_credit_enterprise: e.target.checked })}
              disabled={disabled}
              className="mt-0.5 h-4 w-4 shrink-0 rounded"
            />
            <span>
              <span className="block text-sm font-medium">{t('settings.cacheStrategy.enterprise')}</span>
              <span className="block text-xs text-muted-foreground">{t('settings.cacheStrategy.enterpriseHint')}</span>
            </span>
          </label>
        </div>

        <div className={cn('space-y-3', !credit && 'opacity-60')}>
          <div>
            <p className="text-sm font-medium">{t('settings.cacheStrategy.models')}</p>
            <p className="text-xs text-muted-foreground">{t('settings.cacheStrategy.modelsHint')}</p>
          </div>
          {form?.resetModels ? (
            <p className="rounded-lg bg-muted/60 px-3 py-2 text-xs text-muted-foreground">
              {t('settings.cacheStrategy.resetPending')}
            </p>
          ) : (
            (form?.models ?? []).map((row, idx) => {
              const err = errors.rows[idx]
              return (
                <div key={idx} className="space-y-2 rounded-2xl border p-3">
                  <div className="flex items-center gap-2">
                    <input
                      value={row.model}
                      placeholder="claude-opus-5.5"
                      onChange={(e) => setRow(idx, { model: e.target.value })}
                      disabled={disabled}
                      aria-label={t('settings.cacheStrategy.model')}
                      className={cn(inputClass, 'min-w-0 flex-1')}
                    />
                    <Button type="button" variant="ghost" size="sm" onClick={() => removeRow(idx)} disabled={disabled}>
                      {t('settings.cacheStrategy.remove')}
                    </Button>
                  </div>
                  <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
                    {NUMBER_COLUMNS.map((col) => (
                      <label key={col} className="space-y-1">
                        <span className="block text-[11px] text-muted-foreground">{t(`settings.cacheStrategy.col.${col}`)}</span>
                        <input
                          type="number"
                          step="any"
                          value={row[col]}
                          onChange={(e) => setRow(idx, { [col]: e.target.value })}
                          disabled={disabled}
                          className={inputClass}
                        />
                      </label>
                    ))}
                  </div>
                  {err !== undefined && (
                    <p className="text-xs text-rose-600 dark:text-rose-300">{t(`settings.cacheStrategy.rowError.${err}`)}</p>
                  )}
                </div>
              )
            })
          )}
          {errors.tooMany && (
            <p className="text-xs text-rose-600 dark:text-rose-300">
              {t('settings.cacheStrategy.tooMany', { max: CREDIT_MODELS_MAX })}
            </p>
          )}
          <div className="flex flex-wrap gap-2">
            <Button type="button" variant="outline" size="sm" onClick={addRow} disabled={disabled}>
              {t('settings.cacheStrategy.add')}
            </Button>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              onClick={() => update({ resetModels: true })}
              disabled={disabled || form?.resetModels === true}
            >
              {t('settings.cacheStrategy.reset')}
            </Button>
          </div>
        </div>
      </CardContent>
    </Card>
  )
}
