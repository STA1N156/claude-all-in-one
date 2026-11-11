import { useId } from 'react'
import { AlertTriangle } from 'lucide-react'

import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Select } from '@/components/ui/select'
import { useI18n } from '@/lib/i18n'

import type { AutoModeClassifier } from '../types'

const inputClass =
  'w-full rounded-2xl border bg-input px-4 py-2.5 text-sm text-foreground outline-none transition-colors placeholder:text-muted-foreground'

export const AUTO_MODE_OPTIONS: AutoModeClassifier[] = ['off', 'haiku', 'allow']

interface AutoModeCardProps {
  mode: AutoModeClassifier
  model: string
  disabled: boolean
  onChange: (patch: {
    auto_mode_classifier?: AutoModeClassifier
    auto_mode_classifier_model?: string
  }) => void
}

/**
 * 「自动模式安全分类器」段。
 *
 * Claude Code 的自动模式每动手一次都会先问一遍安全分类器:输入是整段 transcript,
 * 回答只有 `<block>no</block>` 这么几个字 —— 用主模型跑就是按 opus 的价钱买一句话。
 * 这里可以把它改到便宜模型,或者干脆由 caio 本地回「放行」。
 *
 * `allow` 不是性能开关而是**产品决策**:CC 的这层安全闸等于关掉,所以单独标红提示。
 */
export function AutoModeCard({ mode, model, disabled, onChange }: AutoModeCardProps) {
  const { t } = useI18n()
  const idBase = useId()
  const modeId = `${idBase}-mode`
  const modelId = `${idBase}-model`

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">{t('settings.section.autoMode')}</CardTitle>
      </CardHeader>
      <CardContent className="space-y-5">
        <p className="text-xs text-muted-foreground">{t('settings.autoMode.desc')}</p>

        <div className="space-y-1.5">
          <label htmlFor={modeId} className="text-xs font-medium text-muted-foreground">
            {t('settings.autoMode.mode')}
          </label>
          <Select
            id={modeId}
            value={mode}
            disabled={disabled}
            onChange={(e) =>
              onChange({ auto_mode_classifier: e.target.value as AutoModeClassifier })
            }
            className="w-full sm:w-72"
          >
            {AUTO_MODE_OPTIONS.map((m) => (
              <option key={m} value={m}>
                {t(`settings.autoMode.option.${m}`)}
              </option>
            ))}
          </Select>
          <p className="text-xs text-muted-foreground">{t(`settings.autoMode.hint.${mode}`)}</p>
        </div>

        {mode === 'allow' && (
          <p className="flex items-start gap-1.5 rounded-2xl border border-dashed border-destructive/40 bg-destructive/5 p-3 text-xs text-destructive">
            <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            {t('settings.autoMode.allowWarning')}
          </p>
        )}

        {mode === 'haiku' && (
          <div className="space-y-1.5 sm:max-w-sm">
            <label htmlFor={modelId} className="text-xs font-medium text-muted-foreground">
              {t('settings.autoMode.model')}
            </label>
            <input
              id={modelId}
              value={model}
              disabled={disabled}
              spellCheck={false}
              autoComplete="off"
              onChange={(e) => onChange({ auto_mode_classifier_model: e.target.value })}
              className={`${inputClass} font-mono`}
            />
            <p className="text-xs text-muted-foreground">{t('settings.autoMode.modelHint')}</p>
          </div>
        )}

      </CardContent>
    </Card>
  )
}
