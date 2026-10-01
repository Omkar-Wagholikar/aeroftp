// @vitest-environment jsdom
import { act, createElement } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { McpHttpServersPanel } from './McpHttpServersPanel';

const invoke = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke }));
vi.mock('../../i18n', async () => {
    const { translations } = (await import('../../i18n/locales/en.json')).default as { translations: Record<string, unknown> };
    const t = (key: string, params?: Record<string, string | number>) => {
        const value = key.split('.').reduce<unknown>((node, part) => (node as Record<string, unknown> | undefined)?.[part], translations);
        return typeof value === 'string' ? value.replace(/\{(\w+)\}/g, (_, name: string) => String(params?.[name] ?? name)) : key;
    };
    return { useTranslation: () => t };
});

let root: Root;
let host: HTMLDivElement;
const oauthServer = {
    id: 'remote', endpoint: 'https://mcp.example.com/mcp', auth: { mode: 'oauth', client_id: 'client' },
    enabled: true, revision: 3, credential: 'missing', expires_at: null, refreshable: false,
};
const render = async () => { await act(async () => root.render(createElement(McpHttpServersPanel))); };
const click = async (element: Element) => { await act(async () => (element as HTMLElement).click()); };
const input = async (element: HTMLInputElement | HTMLSelectElement, value: string) => {
    await act(async () => {
        const prototype = element instanceof HTMLSelectElement ? HTMLSelectElement.prototype : HTMLInputElement.prototype;
        Object.getOwnPropertyDescriptor(prototype, 'value')!.set!.call(element, value);
        element.dispatchEvent(new Event(element instanceof HTMLSelectElement ? 'change' : 'input', { bubbles: true }));
    });
};
const button = (text: string) => Array.from(host.querySelectorAll('button')).find(e => e.textContent?.trim() === text)!;
beforeEach(() => {
    (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;
    invoke.mockReset();
    host = document.createElement('div'); document.body.append(host); root = createRoot(host);
});
afterEach(async () => { await act(async () => root.unmount()); host.remove(); });

describe('MCP HTTP settings', () => {
    it('adds a disabled server with backend-assigned revision and no client-chosen vault account', async () => {
        invoke.mockImplementation(async (command: string) => command === 'mcp_client_http_list_servers' ? [] : undefined);
        await render();
        await input(host.querySelector('[aria-label="Server ID"]') as HTMLInputElement, 'remote');
        await input(host.querySelector('[aria-label="HTTPS endpoint URL"]') as HTMLInputElement, ' https://mcp.example.com/mcp ');
        await input(host.querySelector('[aria-label="Authentication"]') as HTMLSelectElement, 'bearer');
        await click(button('Add server'));
        const upsert = invoke.mock.calls.find(([command]) => command === 'mcp_client_http_upsert_server')!;
        expect(upsert[1]).toEqual({ server: {
            id: 'remote', endpoint: 'https://mcp.example.com/mcp', auth: { mode: 'bearer' }, enabled: false, expected_revision: 0,
        } });
        expect(JSON.stringify(upsert[1])).not.toContain('vault_account');
    });

    it('rejects a non-HTTPS endpoint before calling the backend', async () => {
        invoke.mockImplementation(async () => []);
        await render();
        await input(host.querySelector('[aria-label="Server ID"]') as HTMLInputElement, 'remote');
        await input(host.querySelector('[aria-label="HTTPS endpoint URL"]') as HTMLInputElement, 'http://mcp.example.com/mcp');
        await click(button('Add server'));
        expect(host.textContent).toContain('Enter a valid server ID and a public HTTPS endpoint.');
        expect(invoke.mock.calls.some(([command]) => command === 'mcp_client_http_upsert_server')).toBe(false);
    });

    it('drives authorize, shows only the public URL, cancels and maps the redacted outcome', async () => {
        let finish!: (value: unknown) => void;
        let fail!: (reason: unknown) => void;
        invoke.mockImplementation((command: string) => {
            if (command === 'mcp_client_http_list_servers') return Promise.resolve([oauthServer]);
            if (command === 'mcp_client_http_oauth_begin') return Promise.resolve({
                attempt: 'opaque', authorization_url: 'https://auth.example.com/authorize?state=public', browser_opened: false,
            });
            if (command === 'mcp_client_http_oauth_wait') return new Promise((resolve, reject) => { finish = resolve; fail = reject; });
            return Promise.resolve();
        });
        await render();
        await click(button('Authorize'));
        expect(host.textContent).toContain('Waiting for the authorization in your browser...');
        expect(host.textContent).toContain('https://auth.example.com/authorize?state=public');
        await click(button('Cancel'));
        expect(invoke).toHaveBeenCalledWith('mcp_client_http_oauth_cancel', { attempt: 'opaque' });
        await act(async () => fail('MCP_OAUTH_CANCELLED'));
        expect(host.textContent).toContain('The authorization was cancelled.');
        expect(host.textContent).not.toContain('Waiting for the authorization');
        await click(button('Authorize'));
        await act(async () => finish(undefined));
        expect(host.textContent).toContain('Authorization saved.');
    });

    it('cancels an attempt still waiting when the settings unmount', async () => {
        invoke.mockImplementation((command: string) => {
            if (command === 'mcp_client_http_list_servers') return Promise.resolve([oauthServer]);
            if (command === 'mcp_client_http_oauth_begin') return Promise.resolve({
                attempt: 'opaque', authorization_url: 'https://auth.example.com/authorize', browser_opened: true,
            });
            if (command === 'mcp_client_http_oauth_wait') return new Promise(() => undefined);
            return Promise.resolve();
        });
        await render();
        await click(button('Authorize'));
        expect(host.textContent).not.toContain('https://auth.example.com/authorize');
        await act(async () => root.unmount());
        expect(invoke).toHaveBeenCalledWith('mcp_client_http_oauth_cancel', { attempt: 'opaque' });
        root = createRoot(host);
    });

    it('cancels an attempt whose start resolves after the settings unmount, without waiting on it', async () => {
        let started!: (value: unknown) => void;
        invoke.mockImplementation((command: string) => {
            if (command === 'mcp_client_http_list_servers') return Promise.resolve([oauthServer]);
            if (command === 'mcp_client_http_oauth_begin') return new Promise(resolve => { started = resolve; });
            return Promise.resolve();
        });
        await render();
        await click(button('Authorize'));
        await act(async () => root.unmount());
        expect(invoke.mock.calls.some(([command]) => command === 'mcp_client_http_oauth_cancel')).toBe(false);
        await act(async () => started({ attempt: 'late', authorization_url: 'https://auth.example.com/authorize', browser_opened: true }));
        expect(invoke).toHaveBeenCalledWith('mcp_client_http_oauth_cancel', { attempt: 'late' });
        expect(invoke.mock.calls.some(([command]) => command === 'mcp_client_http_oauth_wait')).toBe(false);
        root = createRoot(host);
    });

    it('shows redacted credential state, refresh and sign-out only for an existing authorization', async () => {
        const authorized = { ...oauthServer, credential: 'authorized', refreshable: true, expires_at: null };
        invoke.mockImplementation(async (command: string) => command === 'mcp_client_http_list_servers' ? [authorized] : undefined);
        await render();
        expect(host.textContent).toContain('Authorized');
        await click(button('Refresh authorization'));
        expect(invoke).toHaveBeenCalledWith('mcp_client_http_oauth_refresh', { serverId: 'remote' });
        expect(host.textContent).toContain('Authorization refreshed.');
        expect(button('Sign out')).toBeTruthy();
        invoke.mockImplementation(async (command: string) => command === 'mcp_client_http_list_servers' ? [oauthServer] : undefined);
        await click(button('Save server'));
        expect(button('Sign out')).toBeUndefined();
    });

    it('sends the stated revision on toggle and maps a stale write', async () => {
        invoke.mockImplementation(async (command: string) => {
            if (command === 'mcp_client_http_list_servers') return [{ ...oauthServer, auth: { mode: 'none' }, credential: 'not_required' }];
            if (command === 'mcp_client_http_upsert_server') throw 'MCP_CONFIG_STALE_REVISION';
        });
        await render();
        await click(host.querySelector('input[type=checkbox]')!);
        expect(invoke).toHaveBeenCalledWith('mcp_client_http_upsert_server', { server: {
            id: 'remote', endpoint: 'https://mcp.example.com/mcp', auth: { mode: 'none' }, enabled: false, expected_revision: 3,
        } });
        expect(host.textContent).toContain('These settings changed elsewhere. Refresh and try again.');
        expect(host.textContent).not.toContain('MCP_CONFIG_STALE_REVISION');
    });
});
