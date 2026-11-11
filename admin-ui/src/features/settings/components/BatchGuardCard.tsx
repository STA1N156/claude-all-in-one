import { useId } from 'react'
import { ShieldAlert } from 'lucide-react'

import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Select } from '@/components/ui/select'
import { useAccountsRuntime } from '@/features/accounts/hooks'
import type { BatchGuardSnapshot } from '@/features/accounts/types'
import { useI18n } from '@/lib/i18n'

import type { BatchGuardMode, SystemSettings, SystemSettingsPatch } from '../types'

const inputClass =
  'w-full rounded-2xl border bg-input px-4 py-2.5 text-sm text-foreground outline-none transition-colors placeholder:text-muted-foreground'

export const BATCH_GUARD_MODES: BatchGuardMode[] = ['off', 'observe', 'enforce']

/** 与后端 `gw_core::batch_guard` 的默认值一致(旧后端不返回这些键时按它们显示/比较)。 */
const DEFAULTS = { mode: 'observe' as BatchGuardMode, window: 600, threshold: 40, block: 21600, prefix: 80 }

/** 表单态:数字用字符串(输入中间态),名单是「每行一条」的文本。 */
export interface BatchGuardForm {
  mode: BatchGuardMode
  window_secs: string
  threshold: string
  block_secs: string
  prefix_chars: string
  block_prefixes: string
  allow_prefixes: string
}

export function batchGuardToForm(s: SystemSettings): BatchGuardForm {
  return {
    mode: s.batch_guard_mode ?? DEFAULTS.mode,
    window_secs: String(s.batch_guard_window_secs ?? DEFAULTS.window),
    threshold: String(s.batch_guard_threshold ?? DEFAULTS.threshold),
    block_secs: String(s.batch_guard_block_secs ?? DEFAULTS.block),
    prefix_chars: String(s.batch_guard_prefix_chars ?? DEFAULTS.prefix),
    block_prefixes: (s.batch_guard_block_prefixes ?? []).join('\n'),
    allow_prefixes: (s.batch_guard_allow_prefixes ?? []).join('\n'),
  }
}

/** 每行一条 → 去空白、去空行、去重(与后端写侧归一口径一致)。 */
export function linesToList(text: string): string[] {
  const out: string[] = []
  for (const raw of text.split('\n')) {
    const s = raw.trim()
    if (s !== '' && !out.includes(s)) out.push(s)
  }
  return out
}

function sameList(a: string[], b: string[]): boolean {
  return a.length === b.length && a.every((x, i) => x === b[i])
}

/** 只回传变了的键;非法数字不提交(与设置页其它新增字段同口径)。 */
export function batchGuardPatch(form: BatchGuardForm, original: SystemSettings): SystemSettingsPatch {
  const patch: SystemSettingsPatch = {}
  const strictInt = (s: string): number | null => {
    const n = Number(s)
    return s.trim() !== '' && Number.isInteger(n) && n >= 0 ? n : null
  }
  if (form.mode !== (original.batch_guard_mode ?? DEFAULTS.mode)) {
    patch.batch_guard_mode = form.mode
  }
  const nums: Array<[keyof BatchGuardForm, keyof SystemSettings, number]> = [
    ['window_secs', 'batch_guard_window_secs', DEFAULTS.window],
    ['threshold', 'batch_guard_threshold', DEFAULTS.threshold],
    ['block_secs', 'batch_guard_block_secs', DEFAULTS.block],
    ['prefix_chars', 'batch_guard_prefix_chars', DEFAULTS.prefix],
  ]
  for (const [fk, sk, def] of nums) {
    const v = strictInt(form[fk])
    if (v !== null && v !== ((original[sk] as number | undefined) ?? def)) {
      ;(patch as Record<string, unknown>)[sk] = v
    }
  }
  const block = linesToList(form.block_prefixes)
  if (!sameList(block, original.batch_guard_block_prefixes ?? [])) {
    patch.batch_guard_block_prefixes = block
  }
  const allow = linesToList(form.allow_prefixes)
  if (!sameList(allow, original.batch_guard_allow_prefixes ?? [])) {
    patch.batch_guard_allow_prefixes = allow
  }
  return patch
}

interface BatchGuardCardProps {
  form: BatchGuardForm | null
  disabled: boolean
  onChange: (next: BatchGuardForm) => void
}

/**
 * 「批量任务拦截」段:配置 + 各 worker 的实况(已判定的模板)。
 *
 * 背景:Kiro 对脚本化批量处理极度敏感,2026-10-13 一个审图批量任务出现半小时后 40 分钟内
 * 13 个号被封。判据与取舍见后端 `gw_core::batch_guard` 模块文档。
 */
export function BatchGuardCard({ form, disabled, onChange }: BatchGuardCardProps) {
  const { t } = useI18n()
  const idBase = useId()
  const runtime = useAccountsRuntime()
  const set = (patch: Partial<BatchGuardForm>) => {
    if (form !== null) onChange({ ...form, ...patch })
  }
  const mode = form?.mode ?? DEFAULTS.mode

  const live: Array<{ instance: string; group: string; snap: BatchGuardSnapshot }> = []
  for (const inst of runtime.data ?? []) {
    if (inst.online && inst.batch_guard) {
      live.push({ instance: inst.instance, group: inst.group, snap: inst.batch_guard })
    }
  }

  const numField = (
    key: 'window_secs' | 'threshold' | 'block_secs' | 'prefix_chars',
    label: string,
    hint: string,
  ) => (
    <div className="space-y-1.5">
      <label htmlFor={`${idBase}-${key}`} className="text-xs font-medium text-muted-foreground">
        {label}
      </label>
      <input
        id={`${idBase}-${key}`}
        type="number"
        step={1}
        min={0}
        value={form?.[key] ?? ''}
        disabled={disabled}
        onChange={(e) => set({ [key]: e.target.value })}
        className={inputClass}
      />
      <p className="text-xs text-muted-foreground">{hint}</p>
    </div>
  )

  const listField = (key: 'block_prefixes' | 'allow_prefixes', label: string, hint: string) => (
    <div className="space-y-1.5">
      <label htmlFor={`${idBase}-${key}`} className="text-xs font-medium text-muted-foreground">
        {label}
      </label>
      <textarea
        id={`${idBase}-${key}`}
        rows={3}
        value={form?.[key] ?? ''}
        disabled={disabled}
        spellCheck={false}
        onChange={(e) => set({ [key]: e.target.value })}
        className={`${inputClass} font-mono text-xs`}
      />
      <p className="text-xs text-muted-foreground">{hint}</p>
    </div>
  )

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2 text-base">
          <ShieldAlert className="h-4 w-4 text-primary" />
          {t('settings.section.batchGuard')}
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-5">
        <p className="text-xs text-muted-foreground">{t('settings.batchGuard.desc')}</p>

        <div className="space-y-1.5">
          <label htmlFor={`${idBase}-mode`} className="text-xs font-medium text-muted-foreground">
            {t('settings.batchGuard.mode')}
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
                {t(`settings.batchGuard.option.${m}`)}
              </option>
            ))}
          </Select>
          <p className="text-xs text-muted-foreground">{t(`settings.batchGuard.hint.${mode}`)}</p>
        </div>

        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
          {numField('window_secs', t('settings.batchGuard.windowSecs'), t('settings.batchGuard.windowSecsHint'))}
          {numField('threshold', t('settings.batchGuard.threshold'), t('settings.batchGuard.thresholdHint'))}
          {numField('block_secs', t('settings.batchGuard.blockSecs'), t('settings.batchGuard.blockSecsHint'))}
          {numField('prefix_chars', t('settings.batchGuard.prefixChars'), t('settings.batchGuard.prefixCharsHint'))}
        </div>

        <div className="grid gap-4 lg:grid-cols-2">
          {listField('block_prefixes', t('settings.batchGuard.blockPrefixes'), t('settings.batchGuard.blockPrefixesHint'))}
          {listField('allow_prefixes', t('settings.batchGuard.allowPrefixes'), t('settings.batchGuard.allowPrefixesHint'))}
        </div>

        <div className="space-y-2 rounded-2xl border border-black/5 p-3 dark:border-white/5">
          <p className="text-xs font-medium text-muted-foreground">{t('settings.batchGuard.live')}</p>
          {live.length === 0 ? (
            <p className="text-xs text-muted-foreground">{t('settings.batchGuard.liveNone')}</p>
          ) : (
            live.map(({ instance, group, snap }) => (
              <div key={instance} className="space-y-1.5">
                <p className="text-xs">
                  <span className="font-medium">{group}</span>
                  <span className="text-muted-foreground">
                    {' · '}
                    {t('settings.batchGuard.totals', {
                      mode: t(`settings.batchGuard.option.${snap.mode}`),
                      batch: snap.batch_total,
                      blocked: snap.blocked_total,
                      tracked: snap.tracked,
                    })}
                    {snap.flagged_total > snap.flagged.length &&
                      ` · ${t('settings.batchGuard.truncated', { shown: snap.flagged.length, total: snap.flagged_total })}`}
                  </span>
                </p>
                {snap.flagged.length === 0 ? (
                  <p className="text-xs text-muted-foreground">{t('settings.batchGuard.noFlagged')}</p>
                ) : (
                  <div className="overflow-x-auto">
                    <table className="w-full min-w-[480px] text-xs">
                      <thead className="text-muted-foreground">
                        <tr>
                          <th className="py-1 text-left font-medium">{t('settings.batchGuard.col.head')}</th>
                          <th className="py-1 text-left font-medium">{t('settings.batchGuard.col.reason')}</th>
                          <th className="py-1 text-right font-medium">{t('settings.batchGuard.col.count')}</th>
                          <th className="py-1 text-right font-medium">{t('settings.batchGuard.col.since')}</th>
                          <th className="py-1 text-right font-medium">{t('settings.batchGuard.col.remaining')}</th>
                        </tr>
                      </thead>
                      <tbody>
                        {snap.flagged.map((f) => (
                          <tr key={`${f.head}-${f.reason}`} className="border-t border-black/5 dark:border-white/5">
                            <td className="max-w-[18rem] truncate py-1 font-mono" title={f.head}>
                              {f.head}
                            </td>
                            <td className="py-1">{t(`settings.batchGuard.reason.${f.reason}`)}</td>
                            <td className="py-1 text-right tabular-nums">{f.count}</td>
                            <td className="py-1 text-right tabular-nums">{f.since}</td>
                            <td className="py-1 text-right tabular-nums">{Math.ceil(f.remaining_secs / 60)}m</td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                )}
              </div>
            ))
          )}
        </div>
      </CardContent>
    </Card>
  )
}
