// @vitest-environment jsdom
import { act, createElement } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { McpServersPanel } from './McpServersPanel';

const invoke = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke }));

let root: Root;
let host: HTMLDivElement;
const saved = { id: 'fixture', command: '/usr/bin/node', args: ['saved'], env: {}, enabled: false, revision: 1 };
const render = async () => { await act(async () => root.render(createElement(McpServersPanel))); };
const click = async (element: Element) => { await act(async () => (element as HTMLElement).click()); };
const input = async (element: HTMLInputElement | HTMLTextAreaElement, value: string) => {
    await act(async () => {
        const prototype = element instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
        Object.getOwnPropertyDescriptor(prototype, 'value')!.set!.call(element, value);
        element.dispatchEvent(new Event('input', { bubbles: true }));
    });
};
beforeEach(() => {
    (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;
    invoke.mockReset();
    host = document.createElement('div'); document.body.append(host); root = createRoot(host);
});
afterEach(async () => { await act(async () => root.unmount()); host.remove(); });

describe('MCP settings drafts and refresh ordering', () => {
    it('preserves drafts across toggle and environment refresh but adopts changed saved fields independently', async () => {
        let current = structuredClone(saved);
        invoke.mockImplementation(async (command: string) => command === 'mcp_client_list_servers' ? [structuredClone(current)] : undefined);
        await render();
        // The first executable input belongs to the Add form; select the card's input by its saved value.
        const cardCommand = Array.from(host.querySelectorAll('input')).find(e => e.value === saved.command)!;
        const args = host.querySelector('textarea')!;
        await input(cardCommand, '/draft/node'); await input(args, 'draft-arg');
        current = { ...current, enabled: true, revision: 2 };
        await click(host.querySelector('input[type=checkbox]')!);
        expect(cardCommand.isConnected).toBe(true);
        expect(cardCommand.value).toBe('/draft/node'); expect(args.value).toBe('draft-arg');
        current = { ...current, env: { TOKEN: { vault_account: 'fixture-secret' } }, revision: 3 } as typeof current;
        await click(host.querySelector('[aria-label="Refresh MCP servers"]')!);
        expect(cardCommand.value).toBe('/draft/node'); expect(args.value).toBe('draft-arg');
        current = { ...current, command: '/saved/new', revision: 4 };
        await click(host.querySelector('[aria-label="Refresh MCP servers"]')!);
        expect(cardCommand.value).toBe('/saved/new'); expect(args.value).toBe('draft-arg');
        current = { ...current, args: ['new-arg'], revision: 5 };
        await click(host.querySelector('[aria-label="Refresh MCP servers"]')!);
        expect(args.value).toBe('new-arg');
    });

    it('ignores an older initial response after a newer refresh completes', async () => {
        let initial!: (value: unknown) => void;
        invoke.mockImplementationOnce(() => new Promise(resolve => { initial = resolve; }))
            .mockResolvedValueOnce([{ ...saved, id: 'newest' }]);
        await render();
        await click(host.querySelector('[aria-label="Refresh MCP servers"]')!);
        expect(host.textContent).toContain('newest');
        await act(async () => initial([{ ...saved, id: 'obsolete' }]));
        expect(host.textContent).toContain('newest'); expect(host.textContent).not.toContain('obsolete');
    });
});
