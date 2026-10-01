// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

type ChildStatus = 'queued' | 'running' | 'completed' | 'failed' | 'cancelled' | 'budget_exhausted';

export function delegationCardEntries<TWorker extends { childId: string }>(
    workers: readonly TWorker[],
    events: readonly { childId?: string | null; status: ChildStatus }[],
    cancelled: boolean,
): { childId: string; worker?: TWorker; status: ChildStatus }[] {
    const ids = new Set([
        ...workers.map(worker => worker.childId),
        ...events.flatMap(event => event.childId ? [event.childId] : []),
    ]);
    return Array.from(ids, childId => {
        const worker = workers.find(item => item.childId === childId);
        let lastStatus: ChildStatus | undefined;
        for (let index = events.length - 1; index >= 0; index--) {
            if (events[index].childId === childId) {
                lastStatus = events[index].status;
                break;
            }
        }
        lastStatus ??= worker ? 'completed' : 'queued';
        const status = cancelled && (lastStatus === 'queued' || lastStatus === 'running')
            ? 'cancelled' : lastStatus;
        return { childId, worker, status };
    });
}
