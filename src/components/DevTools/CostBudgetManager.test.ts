// SPDX-License-Identifier: GPL-3.0-or-later
import { beforeEach, expect, it, vi } from 'vitest';
const mocks = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }));
beforeEach(() => { vi.resetModules(); mocks.invoke.mockReset(); mocks.invoke.mockResolvedValue(undefined); });

it('does not subtract spending or persist NaN deltas', async () => {
    const budget = await import('./CostBudgetManager');
    await budget.recordSpending('p', 2, 10, 'c');
    await budget.recordSpending('p', -3, Number.NaN, 'c');
    expect(budget.getMonthlySpending()[0]).toMatchObject({ totalCost: 2, tokenCount: 10, requestCount: 2 });
    expect(budget.getConversationCost('c')).toMatchObject({ totalCost: 2, totalTokens: 10, requestCount: 2 });
});

it('saturates provider and conversation counters and keeps the budget blocked', async () => {
    const budget = await import('./CostBudgetManager');
    await budget.saveBudgetConfig([{ providerId: 'p', monthlyLimitUsd: 1, warningThreshold: 80, hardStop: true }]);
    await budget.recordSpending('p', Number.MAX_SAFE_INTEGER, Number.MAX_SAFE_INTEGER, 'c');
    const result = await budget.recordSpending('p', Number.POSITIVE_INFINITY, Number.MAX_VALUE, 'c');
    expect(result.allowed).toBe(false);
    expect(budget.getMonthlySpending()[0]).toMatchObject({ totalCost: Number.MAX_SAFE_INTEGER, tokenCount: Number.MAX_SAFE_INTEGER });
    expect(budget.getConversationCost('c')).toMatchObject({ totalCost: Number.MAX_SAFE_INTEGER, totalTokens: Number.MAX_SAFE_INTEGER });
    const writes = mocks.invoke.mock.calls.filter(([name]) => name === 'vault_set');
    const stored = writes[writes.length - 1]?.[1].value;
    expect(JSON.parse(stored)[0]).toMatchObject({ totalCost: Number.MAX_SAFE_INTEGER, tokenCount: Number.MAX_SAFE_INTEGER });
});

it('bounds corrupt loaded counters before adding a valid delta', async () => {
    const budget = await import('./CostBudgetManager');
    const month = `${new Date().getFullYear()}-${String(new Date().getMonth() + 1).padStart(2, '0')}`;
    mocks.invoke.mockImplementation(async (command: string, args: { key: string }) => command === 'vault_get' && args.key.startsWith('ai_spending_')
        ? JSON.stringify([{ providerId: 'p', month, totalCost: -3, tokenCount: Number.MAX_VALUE, requestCount: Number.MAX_VALUE }]) : undefined);
    await budget.initBudgetManager();
    await budget.recordSpending('p', 2, 5);
    expect(budget.getMonthlySpending()[0]).toMatchObject({ totalCost: 2, tokenCount: Number.MAX_SAFE_INTEGER, requestCount: Number.MAX_SAFE_INTEGER });
});
