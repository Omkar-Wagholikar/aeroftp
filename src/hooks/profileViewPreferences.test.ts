// @vitest-environment jsdom
import { act, createElement } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useMyServersBreakdown } from './useMyServersBreakdown';
import { useTableColumns, type TableColumnDef } from './useTableColumns';
import { MyServersTableFooter } from '../components/IntroHub/MyServersTableFooter';

const storage = vi.hoisted(() => ({
    blob: {} as Record<string, unknown>,
    get: vi.fn(),
    store: vi.fn(),
}));
vi.mock('../utils/secureStorage', () => ({
    secureGetWithFallback: storage.get,
    secureStoreAndClean: storage.store,
}));
vi.mock('../i18n', () => ({ useTranslation: () => (key: string) => key }));

const columns: TableColumnDef<'name' | 'subtitle'>[] = [
    { id: 'name', labelKey: 'name', sortable: true, defaultVisible: true, defaultWidth: 100 },
    { id: 'subtitle', labelKey: 'host', sortable: false, defaultVisible: true, defaultWidth: 200 },
];
let root: Root;
let host: HTMLDivElement;
let latest: ReturnType<typeof useTableColumns<'name' | 'subtitle'>>;

function Probe({ refreshKey }: { refreshKey?: number }) {
    const { breakdown, setBreakdown } = useMyServersBreakdown(refreshKey);
    latest = useTableColumns({ columns, storageKey: 'my_servers_table', refreshKey });
    return createElement('div', null,
        createElement('output', { 'data-host-visible': String(latest.config.visibility.subtitle) }),
        createElement(MyServersTableFooter, { servers: [], breakdown, onBreakdownChange: setBreakdown }),
    );
}

beforeEach(async () => {
    Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
    storage.blob = {};
    storage.get.mockReset().mockImplementation(async () => structuredClone(storage.blob));
    storage.store.mockReset().mockImplementation(async (_account, _cache, value) => {
        storage.blob = structuredClone(value);
    });
    host = document.createElement('div');
    document.body.append(host);
    root = createRoot(host);
});
afterEach(async () => {
    await act(async () => root.unmount());
    host.remove();
});

const checkbox = () => host.querySelector('input[type="checkbox"]') as HTMLInputElement;
const mount = async () => { await act(async () => root.render(createElement(Probe))); };

describe('shared CLI / GUI profile view preferences', () => {
    it('hydrates CLI choices and reloads them when the app regains focus', async () => {
        storage.blob = { ui_settings: { my_servers_table: { visibility: { subtitle: false } }, my_servers_breakdown: true } };
        await mount();
        expect(checkbox().checked).toBe(true);
        expect(latest.config.visibility.subtitle).toBe(false);
        expect(storage.get).toHaveBeenCalledWith('app_settings', 'aeroftp_settings');

        storage.blob = { ui_settings: { my_servers_table: { visibility: { subtitle: true } }, my_servers_breakdown: false } };
        await act(async () => window.dispatchEvent(new Event('focus')));
        expect(checkbox().checked).toBe(false);
        expect(latest.config.visibility.subtitle).toBe(true);
    });

    it('replaces the locked startup cache with vault choices after unlock', async () => {
        storage.blob = { ui_settings: { my_servers_table: { visibility: { subtitle: false } }, my_servers_breakdown: true } };
        await act(async () => root.render(createElement(Probe, { refreshKey: 0 })));
        expect(checkbox().checked).toBe(true);
        expect(latest.config.visibility.subtitle).toBe(false);

        storage.blob = { ui_settings: { my_servers_table: { visibility: { subtitle: true } }, my_servers_breakdown: false } };
        await act(async () => root.render(createElement(Probe, { refreshKey: 1 })));
        expect(checkbox().checked).toBe(false);
        expect(latest.config.visibility.subtitle).toBe(true);
    });

    it('the labeled checkbox persists both states without replacing table layout', async () => {
        const table = { visibility: { subtitle: false }, widths: { name: 321 }, order: ['subtitle', 'name'], sort: { colId: 'name', dir: 'desc' } };
        storage.blob = { theme: 'dark', ui_settings: { my_servers_table: table, other_table: { value: 3 } } };
        await mount();
        expect(checkbox().closest('label')?.textContent).toContain('introHub.breakdown.title');
        await act(async () => checkbox().click());
        expect(checkbox().checked).toBe(true);
        expect(storage.blob).toEqual({ theme: 'dark', ui_settings: { my_servers_table: table, other_table: { value: 3 }, my_servers_breakdown: true } });
        await act(async () => checkbox().click());
        expect(checkbox().checked).toBe(false);
        expect((storage.blob.ui_settings as Record<string, unknown>).my_servers_breakdown).toBe(false);
    });

    it('GUI column updates preserve breakdown and CLI-only visibility fields', async () => {
        storage.blob = { ui_settings: { my_servers_breakdown: true, my_servers_table: { visibility: { groups: true, subtitle: false }, future: 'keep' } } };
        await mount();
        await act(async () => latest.setVisible('subtitle', true));
        const ui = storage.blob.ui_settings as Record<string, unknown>;
        const table = ui.my_servers_table as { visibility: Record<string, boolean>; future: string };
        expect(ui.my_servers_breakdown).toBe(true);
        expect(table.visibility.groups).toBe(true);
        expect(table.visibility.subtitle).toBe(true);
        expect(table.future).toBe('keep');
    });

    it('keeps the last confirmed checkbox value when a vault write fails', async () => {
        await mount();
        storage.store.mockRejectedValueOnce(new Error('vault unavailable'));
        await act(async () => checkbox().click());
        expect(checkbox().checked).toBe(false);
    });
});
