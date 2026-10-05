// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { updateAppSettings } from './appSettings';
import { secureGetWithFallback } from './secureStorage';

const backend = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: backend.invoke }));

beforeEach(() => {
    localStorage.clear();
    backend.invoke.mockReset();
});

describe('confirmed app-settings persistence', () => {
    it('keeps fallback unchanged on vault failure and allows the next save', async () => {
        const original = { theme: 'dark', ui_settings: { my_servers_breakdown: false } };
        localStorage.setItem('aeroftp_settings', JSON.stringify(original));
        backend.invoke.mockImplementation(async (command: string) => {
            if (command === 'get_credential') throw new Error('not found');
            throw new Error('vault write failed');
        });
        await expect(updateAppSettings(existing => ({
            ...existing,
            ui_settings: { my_servers_breakdown: true },
        }))).rejects.toThrow('vault write failed');
        expect(JSON.parse(localStorage.getItem('aeroftp_settings')!)).toEqual(original);
        expect(await secureGetWithFallback('app_settings', 'aeroftp_settings')).toEqual(original);

        backend.invoke.mockImplementation(async (command: string) => {
            if (command === 'get_credential') throw new Error('not found');
        });
        await updateAppSettings(existing => ({ ...existing, theme: 'light' }));
        expect(JSON.parse(localStorage.getItem('aeroftp_settings')!)).toEqual({ ...original, theme: 'light' });
    });

    it('merges overlapping updates after vault acknowledgement, then updates fallback', async () => {
        let vault = JSON.stringify({ theme: 'dark' });
        localStorage.setItem('aeroftp_settings', vault);
        let release!: () => void;
        const gate = new Promise<void>(r => { release = r; });
        let stores = 0;
        backend.invoke.mockImplementation(async (command: string, args: { account: string; password: string }) => {
            expect(args.account).toBe('config_app_settings');
            if (command === 'get_credential') return vault;
            if (++stores === 1) await gate;
            vault = args.password;
        });
        const first = updateAppSettings(existing => ({ ...existing, breakdown: true }));
        const second = updateAppSettings(existing => ({ ...existing, hostVisible: false }));
        await vi.waitFor(() => expect(stores).toBe(1));
        expect(JSON.parse(localStorage.getItem('aeroftp_settings')!)).toEqual({ theme: 'dark' });
        release();
        await Promise.all([first, second]);
        expect(JSON.parse(vault)).toEqual({ theme: 'dark', breakdown: true, hostVisible: false });
        expect(localStorage.getItem('aeroftp_settings')).toBe(vault);
    });
});
