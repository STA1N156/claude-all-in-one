import { useMemo, useState } from 'react'
import { Coins } from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { TD, TR } from '@/components/ui/table'
import { formatCredits } from '@/features/accounts/lib'
import { useI18n } from '@/lib/i18n'
import { formatInt } from '@/lib/utils'

import {
  accountUsageStatus,
  cacheHitRate,
  creditsPerRequest,
  filterAccountUsage,
  sumCredits,
  type AccountUsageStatus,
} from '../lib'
import type { AccountUsage } from '../types'
import { UsageTableCard } from './UsageTableCard'

/** 默认只渲染积分最高的前 N 个号;全时段有上千个号(含已删),一次全铺开太长。 */
const DEFAULT_VISIBLE = 50

const STATUS_VARIANT: Record<AccountUsageStatus, 'success' | 'muted' | 'outline'> = {
  active: 'success',
  disabled: 'muted',
  deleted: 'outline',
}

interface ByAccountTableProps {
  data: AccountUsage[] | undefined
  loading: boolean
}

/** 按账号积分:每个上游号在所选时间窗内消耗了多少 Kiro 积分(后端已按积分降序)。 */
export function ByAccountTable({ data, loading }: ByAccountTableProps) {
  const { t, lang } = useI18n()
  const locale = lang === 'zh' ? 'zh-CN' : 'en-US'
  const [search, setSearch] = useState('')
  const [showAll, setShowAll] = useState(false)

  const filtered = useMemo(() => filterAccountUsage(data ?? [], search), [data, search])
  const visible = showAll ? filtered : filtered.slice(0, DEFAULT_VISIBLE)

  const columns = [
    { label: t('table.accountId') },
    { label: t('usage.byAccount.tier') },
    { label: t('table.status') },
    { label: t('table.requests'), right: true },
    { label: t('usage.byAccount.credits'), right: true },
    { label: t('usage.byAccount.perRequest'), right: true },
    { label: t('table.output'), right: true },
    { label: t('usage.byAccount.hitRate'), right: true },
    { label: t('usage.byAccount.lastUsed'), right: true },
  ]

  const rows = visible.map((row) => {
    const status = accountUsageStatus(row)
    const perReq = creditsPerRequest(row)
    const hit = cacheHitRate(row)
    const lastUsed = new Date(row.last_used_at * 1000)
    return (
      <TR key={row.account_id}>
        <TD>
          <code className="font-mono text-xs">{row.account_id}</code>
        </TD>
        <TD className="whitespace-nowrap">
          {row.subscription_title && <Badge variant="muted">{row.subscription_title}</Badge>}
          {row.group_name && (
            <span className="ml-1.5 text-xs text-muted-foreground">{row.group_name}</span>
          )}
          {!row.subscription_title && !row.group_name && (
            <span className="text-muted-foreground">—</span>
          )}
        </TD>
        <TD>
          <Badge variant={STATUS_VARIANT[status]}>{t(`usage.byAccount.status.${status}`)}</Badge>
        </TD>
        <TD className="text-right">{formatInt(row.requests)}</TD>
        <TD className="text-right font-semibold tabular-nums">
          {formatCredits(row.metering_credit)}
        </TD>
        <TD className="text-right tabular-nums text-muted-foreground">
          {perReq === null ? '—' : perReq.toFixed(2)}
        </TD>
        <TD className="text-right text-muted-foreground">{formatInt(row.output_tokens)}</TD>
        <TD className="text-right text-muted-foreground" title={t('usage.byAccount.hitRateHint')}>
          {hit === null ? '—' : `${(hit * 100).toFixed(1)}%`}
        </TD>
        <TD
          className="whitespace-nowrap text-right text-muted-foreground"
          title={lastUsed.toLocaleString(locale)}
        >
          {lastUsed.toLocaleString(locale, {
            month: '2-digit',
            day: '2-digit',
            hour: '2-digit',
            minute: '2-digit',
          })}
        </TD>
      </TR>
    )
  })

  const actions = (
    <>
      {!loading && filtered.length > 0 && (
        <span className="text-xs text-muted-foreground">
          {t('usage.byAccount.total', { credits: formatCredits(sumCredits(filtered)) })}
        </span>
      )}
      <input
        type="search"
        value={search}
        onChange={(e) => setSearch(e.target.value)}
        placeholder={t('usage.byAccount.search')}
        aria-label={t('usage.byAccount.search')}
        className="w-44 rounded-2xl border bg-input px-3 py-1.5 text-sm focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/40"
      />
    </>
  )

  const footer =
    !loading && filtered.length > DEFAULT_VISIBLE ? (
      <div className="flex justify-center border-t border-black/5 px-5 py-3 dark:border-white/5">
        <Button variant="ghost" size="sm" onClick={() => setShowAll((v) => !v)}>
          {showAll
            ? t('usage.byAccount.showLess')
            : t('usage.byAccount.showAll', { n: filtered.length })}
        </Button>
      </div>
    ) : null

  return (
    <UsageTableCard
      icon={Coins}
      title={t('usage.byAccount.title')}
      count={filtered.length}
      loading={loading}
      columns={columns}
      rows={rows}
      actions={actions}
      footer={footer}
    />
  )
}
