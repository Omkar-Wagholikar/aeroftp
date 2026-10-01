// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

import { describe, expect, it } from 'vitest';
import { delegationCardEntries } from './aiChatDelegationCards';

describe('delegation result cards', () => {
    it('keeps all six completed workers when the event window loses the first child', () => {
        const workers = Array.from({ length: 6 }, (_, index) => ({ childId: `child-${index + 1}` }));
        const history = [
            { childId: null, status: 'running' as const },
            ...workers.flatMap(worker => [
                { childId: worker.childId, status: 'queued' as const },
                { childId: worker.childId, status: 'running' as const },
                { childId: worker.childId, status: 'completed' as const },
            ]),
            { childId: null, status: 'completed' as const },
        ];
        const events = history.slice(-16);
        expect(events.some(event => event.childId === workers[0].childId)).toBe(false);

        const cards = delegationCardEntries(workers, events, false);
        expect(cards.map(card => card.childId)).toEqual(workers.map(worker => worker.childId));
        expect(cards.every(card => card.status === 'completed' && card.worker)).toBe(true);
    });
});
