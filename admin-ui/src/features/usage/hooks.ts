import { keepPreviousData, useQuery } from '@tanstack/react-query'

import { queryKeys } from '@/lib/query-keys'

import {
  fetchAccountCreditTotals,
  fetchUsageByAccount,
  fetchUsageByKey,
  fetchUsageByModel,
  fetchUsageSummary,
} from './api'
import type { UsageFilter } from './types'

export function useUsageSummary(filter: UsageFilter) {
  return useQuery({
    queryKey: queryKeys.usage.summary(filter),
    queryFn: () => fetchUsageSummary(filter),
    placeholderData: keepPreviousData,
  })
}

export function useUsageByModel(filter: UsageFilter) {
  return useQuery({
    queryKey: queryKeys.usage.byModel(filter),
    queryFn: () => fetchUsageByModel(filter),
    placeholderData: keepPreviousData,
  })
}

/**
 * by-key always lists every key, so the `key` part of the filter is stripped —
 * both from the request and from the query key (changing the key filter must
 * not refetch the key list).
 */
export function useUsageByKey(filter: UsageFilter) {
  const timeFilter: UsageFilter = {
    mode: filter.mode,
    days: filter.days,
    from: filter.from,
    to: filter.to,
  }
  return useQuery({
    queryKey: queryKeys.usage.byKey(timeFilter),
    queryFn: () => fetchUsageByKey(timeFilter),
    placeholderData: keepPreviousData,
  })
}

/** Per-account usage (same filter as summary / by-model, key filter included). */
export function useUsageByAccount(filter: UsageFilter) {
  return useQuery({
    queryKey: queryKeys.usage.byAccount(filter),
    queryFn: () => fetchUsageByAccount(filter),
    placeholderData: keepPreviousData,
  })
}

/**
 * Lifetime logged credits for the given accounts (accounts page reconciliation).
 * Deliberately NOT tied to the 15s runtime poll: a minute-old total is fine for
 * spotting a mismatch, and each call is a real DB aggregate.
 */
export function useAccountCreditTotals(ids: string[]) {
  return useQuery({
    queryKey: queryKeys.usage.accountCredits(ids),
    queryFn: () => fetchAccountCreditTotals(ids),
    enabled: ids.length > 0,
    staleTime: 60_000,
    refetchInterval: 60_000,
    placeholderData: keepPreviousData,
  })
}
