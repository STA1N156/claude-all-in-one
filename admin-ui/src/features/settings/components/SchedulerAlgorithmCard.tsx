import { useId } from 'react'

import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Segment } from '@/components/ui/segment'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/utils'

import {
  SCHEDULER_DEFAULTS,
  SCHEDULER_FIELDS,
  type SchedulerErrors,
  type SchedulerFieldError,
  type SchedulerFieldSpec,
  type SchedulerForm,
} from '../scheduler'
import { SCHEDULER_ALGORITHMS, type SchedulerAlgorithm } from '../types'

const inputClass =
  'w-full rounded-2xl border bg-input px-4 py-2.5 text-sm text-foreground outline-none transition-colors placeholder:text-muted-foreground'

type NumberSpec = Extract<SchedulerFieldSpec, { kind: 'int' | 'float' }>
type BoolSpec = Extract<SchedulerFieldSpec, { kind: 'bool' }>

/**
 * 系统设置里的「调度器」段：算法选择 + 所选算法的参数。
 *
 * 受控组件，状态由 SettingsPage 持有、随主表单一起保存 —— 这些键都在 `/settings`
 * 的 overlay 里，与 RestockSettingsCard 那种独立端点不同，没必要自带保存按钮。
 */
export function SchedulerAlgorithmCard({
  form,
  errors,
  disabled,
  onChange,
}: {
  form: SchedulerForm | null
  errors: SchedulerErrors
  disabled: boolean
  onChange: <K extends keyof SchedulerForm>(key: K, value: SchedulerForm[K]) => void
}) {
  const { t } = useI18n()
  const idBase = useId()
  const algorithm = form?.scheduler_algorithm ?? SCHEDULER_DEFAULTS.scheduler_algorithm
  const fields = SCHEDULER_FIELDS.filter((f) => f.algorithms.includes(algorithm))

  // 切走的那个面板里留着的非法值：看不见也得说出来，否则保存被拦却找不到原因。
  const hiddenInvalid = SCHEDULER_ALGORITHMS.filter(
    (a) =>
      a !== algorithm &&
      SCHEDULER_FIELDS.some(
        (f) =>
          f.kind !== 'bool' &&
          f.algorithms.includes(a) &&
          !f.algorithms.includes(algorithm) &&
          errors[f.key] !== undefined,
      ),
  )

  const algoLabel = (a: SchedulerAlgorithm) => t(`settings.schedulerAlgo.${a}`)

  const errorText = (spec: NumberSpec, code: SchedulerFieldError) => {
    if (code !== 'outOfRange') return t(`settings.schedulerAlgo.error.${code}`)
    return spec.max === undefined
      ? t('settings.schedulerAlgo.error.min', { min: spec.min })
      : t('settings.schedulerAlgo.error.range', { min: spec.min, max: spec.max })
  }

  const numberField = (spec: NumberSpec) => {
    const id = `${idBase}-${spec.key}`
    const error = errors[spec.key]
    return (
      <div key={spec.key} className="space-y-1.5">
        <label htmlFor={id} className="text-xs font-medium text-muted-foreground">
          {t(`settings.schedulerAlgo.${spec.key}`)}
        </label>
        <input
          id={id}
          type="number"
          step={spec.kind === 'int' ? 1 : 'any'}
          min={spec.min}
          max={spec.max}
          value={form?.[spec.key] ?? ''}
          onChange={(e) => onChange(spec.key, e.target.value)}
          disabled={disabled}
          aria-invalid={error !== undefined}
          className={cn(inputClass, error !== undefined && '!border-rose-500 dark:!border-rose-400')}
        />
        {error !== undefined && (
          <p className="text-xs text-rose-600 dark:text-rose-300">{errorText(spec, error)}</p>
        )}
        <p className="text-xs text-muted-foreground">{t(`settings.schedulerAlgo.${spec.key}Hint`)}</p>
      </div>
    )
  }

  // 开关独占一整行：提示文案偏长，挤在半列里要折五六行。
  const boolField = (spec: BoolSpec) => (
    <label key={spec.key} className="flex cursor-pointer items-start gap-3 sm:col-span-2">
      <input
        type="checkbox"
        checked={form?.[spec.key] ?? SCHEDULER_DEFAULTS[spec.key]}
        onChange={(e) => onChange(spec.key, e.target.checked)}
        disabled={disabled}
        className="mt-0.5 h-4 w-4 shrink-0 rounded"
      />
      <span>
        <span className="block text-sm font-medium">{t(`settings.schedulerAlgo.${spec.key}`)}</span>
        <span className="block text-xs text-muted-foreground">
          {t(`settings.schedulerAlgo.${spec.key}Hint`)}
        </span>
      </span>
    </label>
  )

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">{t('settings.section.schedulerAlgorithm')}</CardTitle>
      </CardHeader>
      <CardContent className="space-y-5">
        <div className="space-y-2">
          <span className="block text-sm font-medium">{t('settings.schedulerAlgo.algorithm')}</span>
          {/* 窄屏（375px）下两个长标签放不下一行：整颗按钮换到第二行（圆角随之收小），
              而不是把按钮压窄、让标签在括号中间断开，也不撑出横向滚动。 */}
          <Segment
            options={SCHEDULER_ALGORITHMS.map((a) => ({ value: a, label: algoLabel(a) }))}
            value={algorithm}
            onChange={(v) => {
              if (!disabled) onChange('scheduler_algorithm', v)
            }}
            className="max-w-full flex-wrap rounded-3xl sm:rounded-full"
          />
          <p className="text-xs text-muted-foreground">{t(`settings.schedulerAlgo.${algorithm}Desc`)}</p>
        </div>

        {hiddenInvalid.map((a) => (
          <p key={a} className="rounded-lg bg-destructive/10 px-2 py-1 text-xs text-destructive">
            {t('settings.schedulerAlgo.hiddenErrors', { algo: algoLabel(a) })}
          </p>
        ))}

        <div>
          <p className="mb-2 text-xs font-medium text-muted-foreground">{t('settings.schedulerAlgo.params')}</p>
          <div className="grid gap-4 sm:grid-cols-2">
            {fields.map((f) => (f.kind === 'bool' ? boolField(f) : numberField(f)))}
          </div>
        </div>
      </CardContent>
    </Card>
  )
}
