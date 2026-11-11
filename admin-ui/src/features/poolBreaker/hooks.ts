import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import { queryKeys } from '@/lib/query-keys'

import { fetchPoolBreaker, resetPoolBreaker } from './api'

/** 15 秒轮询:熔断是要人马上看到的事,每个页面顶上都在用。 */
export function usePoolBreaker() {
  return useQuery({
    queryKey: queryKeys.poolBreaker.status(),
    queryFn: fetchPoolBreaker,
    refetchInterval: 15_000,
  })
}

export function useResetPoolBreaker() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: resetPoolBreaker,
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: queryKeys.poolBreaker.root })
    },
  })
}
