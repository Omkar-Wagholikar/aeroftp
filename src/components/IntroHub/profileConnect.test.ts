// @vitest-environment jsdom
// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)
import { act, createElement as h } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { MyServersPanel } from './MyServersPanel';
import { ConnectScope, type ProfileConnector } from '../../gui/connectScope';
import type { ServerProfile } from '../../types';
const state = vi.hoisted(() => ({
    profiles: [] as ServerProfile[],
    invoke: vi.fn<(name: string, args?: Record<string, unknown>) => Promise<unknown>>(),
    refreshDevices: vi.fn(async () => []),
    status: { isUnlocked: true, activeUserId: 1, unlockedUserId: 1 },
}));
vi.mock('@tauri-apps/api/core', () => ({ invoke: state.invoke }));
vi.mock('../../i18n', () => ({ useTranslation: () => (key: string) => key }));
vi.mock('../../utils/serverProfileStore', () => ({
    loadSavedServerProfiles: async () => state.profiles, loadSavedServerProfilesStrict: async () => state.profiles,
    storeSavedServerProfiles: vi.fn(), mergeSavedServerProfile: async () => state.profiles,
}));
vi.mock('../../utils/userPartitions', () => ({ getUnlockStatus: async () => ({ ...state.status }), listUsers: async () => [] }));
vi.mock('../../utils/favoriteServers', () => ({ loadFavoriteServers: async () => [], saveFavoriteServers: vi.fn() }));
vi.mock('../../utils/serverGroups', () => ({ loadServerGroups: async () => [], saveServerGroups: vi.fn(), newServerGroupId: vi.fn(), pruneServerFromGroups: vi.fn(), reorderServerGroups: vi.fn() }));
vi.mock('../../hooks/useActivityLog', () => ({ useActivityLog: () => ({ log: vi.fn() }) }));
vi.mock('../../hooks/useStorageThresholds', () => ({ useStorageThresholds: () => ({ thresholds: {} }) }));
vi.mock('../../hooks/useMyServersDensity', () => ({ useMyServersDensity: () => ({ density: 'normal', setDensity: vi.fn() }) }));
vi.mock('../../hooks/useAeroShareEnabled', () => ({ useAeroShareEnabled: () => false }));
vi.mock('../../hooks/usePeerDriveStates', () => ({ usePeerDriveStates: () => ({ states: new Map(), refresh: vi.fn() }) }));
vi.mock('../../hooks/useDeviceAttachState', () => ({ useDeviceAttachState: () => ({ attachedProfileIds: new Set(), refresh: state.refreshDevices }) }));
vi.mock('../../hooks/useProviderHealth', () => ({ useProviderHealth: () => ({ getStatus: () => undefined, scanItems: vi.fn() }) }));
vi.mock('../../hooks/useCardLayout', () => ({ useCardLayout: () => 'compact' }));
vi.mock('../../hooks/useMyServersColumns', () => ({ useMyServersColumns: () => ({}) }));
vi.mock('../../hooks/useMyServersBreakdown', () => ({ useMyServersBreakdown: () => ({ breakdown: 'all', setBreakdown: vi.fn() }) }));
vi.mock('../../hooks/useResponsiveColumns', () => ({ useResponsiveColumns: () => [3, vi.fn()] }));
vi.mock('./MyServersToolbar', () => ({ MyServersToolbar: () => null }));
vi.mock('./MyServersSidebar', () => ({ MyServersSidebar: () => null }));
vi.mock('./ServerCard', () => ({ ServerCard: () => null }));
vi.mock('./MyServersTable', () => ({ MyServersTable: () => null }));
vi.mock('./MyServersTableFooter', () => ({ MyServersTableFooter: () => null }));
let root: Root; let host: HTMLDivElement; let connector: ProfileConnector;
const connect = vi.fn(async () => 'connected' as const);
beforeEach(() => {
    (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;
    localStorage.clear(); connect.mockClear(); state.invoke.mockReset();
    state.status = { isUnlocked: true, activeUserId: 1, unlockedUserId: 1 };
    state.profiles = [{ id: 'fixture', name: 'Fixture', protocol: 'ftp', host: 'fixture.invalid', username: 'fixture' } as ServerProfile];
    host = document.createElement('div'); document.body.append(host); root = createRoot(host);
});
afterEach(async () => { await act(async () => root.unmount()); host.remove(); });
async function mount() {
    await act(async () => root.render(h(MyServersPanel, { onConnect: connect, onEdit: vi.fn(), onQuickConnect: vi.fn(),
        registerProfileConnector: value => { connector = value; return () => {}; } })));
}
async function until(check: () => boolean) {
    for (let n = 0; !check(); n++) {
        if (n > 100) throw new Error('component wait timed out');
        await act(async () => { await new Promise(resolve => setTimeout(resolve, 5)); });
    }
}
it('stops the actual saved-profile handler while its credential read is pending', async () => {
    let release!: (value: string) => void;
    state.invoke.mockImplementation(name => name === 'get_credential' ? new Promise(resolve => { release = resolve; }) : Promise.resolve(undefined));
    await mount(); const scope = new ConnectScope(); let pending!: ReturnType<ProfileConnector>;
    await act(async () => { pending = connector('fixture', scope); });
    await until(() => !!release); scope.cancel(new Error('lease_interrupted'));
    await act(async () => { release('PRIVATE_PASSWORD_SENTINEL'); await expect(pending).rejects.toThrow(); });
    expect(connect).not.toHaveBeenCalled();
});
it('prevents OAuth key continuations from opening authentication after Stop', async () => {
    state.profiles[0].protocol = 'dropbox'; let release!: (value: string) => void;
    state.invoke.mockImplementation(name => name === 'get_credential' ? new Promise(resolve => { release = resolve; }) : Promise.resolve(undefined));
    await mount(); const scope = new ConnectScope(); let pending!: ReturnType<ProfileConnector>;
    await act(async () => { pending = connector('fixture', scope); }); await until(() => !!release);
    scope.cancel(new Error('lease_interrupted'));
    await act(async () => { release('PRIVATE_API_KEY_SENTINEL'); await expect(pending).rejects.toThrow(); });
    expect(state.invoke.mock.calls.map(([name]) => name)).not.toContain('oauth2_full_auth');
    expect(connect).not.toHaveBeenCalled();
});
it('checks account changes after the actual credential wait without exposing the secret', async () => {
    let release!: (value: string) => void;
    state.invoke.mockImplementation(name => name === 'get_credential' ? new Promise(resolve => { release = resolve; }) : Promise.resolve(undefined));
    await mount(); let pending!: ReturnType<ProfileConnector>;
    await act(async () => { pending = connector('fixture', new ConnectScope()); }); await until(() => !!release);
    state.status.activeUserId = 2;
    await act(async () => { release('PRIVATE_PASSWORD_SENTINEL'); await expect(pending).rejects.toThrow('lease_interrupted'); });
    expect(connect).not.toHaveBeenCalled(); expect(host.textContent).not.toContain('PRIVATE');
});

it('does not re-authenticate OAuth or 4shared after their actual connect call is interrupted', async () => {
    for (const protocol of ['dropbox', 'fourshared'] as const) {
        state.profiles[0].protocol = protocol; let reject!: (error: Error) => void;
        const command = protocol === 'fourshared' ? 'fourshared_connect' : 'oauth2_connect';
        state.invoke.mockReset().mockImplementation(name => {
            if (name === 'get_credential') return Promise.resolve('PRIVATE_API_KEY_SENTINEL');
            if (name.endsWith('_has_tokens')) return Promise.resolve(true);
            if (name === command) return new Promise((_yes, no) => { reject = no; });
            return Promise.resolve(undefined);
        });
        if (protocol === 'dropbox') await mount();
        const scope = new ConnectScope(); let pending!: ReturnType<ProfileConnector>;
        await act(async () => { pending = connector('fixture', scope); }); await until(() => !!reject);
        scope.cancel(new Error('lease_interrupted'));
        await act(async () => { reject(new Error('token expired')); await expect(pending).rejects.toThrow(); });
        expect(state.invoke.mock.calls.map(([name]) => name)).not.toContain(protocol === 'fourshared' ? 'fourshared_full_auth' : 'oauth2_full_auth');
        expect(connect).not.toHaveBeenCalled();
    }
});
