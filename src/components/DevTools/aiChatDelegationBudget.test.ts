// SPDX-License-Identifier: GPL-3.0-or-later
import { beforeEach, describe, expect, it, vi } from 'vitest';
const mocks = vi.hoisted(() => ({ check: vi.fn(), record: vi.fn() }));
vi.mock('./CostBudgetManager', () => ({ checkBudget: mocks.check, recordSpending: mocks.record }));
import { runBudgetedDelegation } from './aiChatDelegationBudget';
const allowed = { allowed: true, currentSpend: 0, limit: 10, percentUsed: 0, warning: false };
describe('delegation spending', () => {
    beforeEach(() => { vi.clearAllMocks(); mocks.check.mockReturnValue(allowed); mocks.record.mockResolvedValue(allowed); });
    it('refuses provider dispatch when the monthly budget is exhausted', async () => {
        mocks.check.mockReturnValue({ ...allowed, allowed: false, message: 'Budget exhausted' });
        const dispatch = vi.fn();
        await expect(runBudgetedDelegation('provider', undefined, 'chat', dispatch, vi.fn())).rejects.toThrow('Budget exhausted');
        expect(dispatch).not.toHaveBeenCalled(); expect(mocks.record).not.toHaveBeenCalled();
    });
    it('records aggregate usage with the pinned model pricing before returning the answer', async () => {
        const dispatch = vi.fn(async () => ({ answer: 'done', inputTokens: 1000, outputTokens: 500 }));
        const result = await runBudgetedDelegation('provider', { inputCostPer1k: 2, outputCostPer1k: 3 }, 'chat', dispatch, vi.fn());
        expect(mocks.record).toHaveBeenCalledWith('provider', 3.5, 1500, 'chat');
        expect(result.tokenInfo?.cost).toBe(3.5); expect(result.result.answer).toBe('done');
    });
    it('does not invent provider usage when dispatch fails', async () => {
        await expect(runBudgetedDelegation('provider', undefined, undefined, () => Promise.reject(new Error('failed')), vi.fn())).rejects.toThrow('failed');
        expect(mocks.record).not.toHaveBeenCalled();
    });
});
