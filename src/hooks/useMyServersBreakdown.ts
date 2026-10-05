// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet -- AI-assisted (see AI-TRANSPARENCY.md)

import { useCallback, useEffect, useState } from 'react';
import { secureGetWithFallback, secureStoreAndClean } from '../utils/secureStorage';

const ACCOUNT = 'app_settings';
const CACHE_KEY = 'aeroftp_settings';
const EVENT = 'aeroftp-settings-changed';

type Settings = Record<string, unknown>;

const readBreakdown = (blob: Settings | null): boolean => {
    const ui = blob?.ui_settings as Settings | undefined;
    return ui?.my_servers_breakdown === true;
};

/** The CLI stores this same boolean in config_app_settings.ui_settings. */
export function useMyServersBreakdown(refreshKey?: number) {
    const [breakdown, setLocal] = useState(false);

    useEffect(() => {
        let cancelled = false;
        const load = async () => {
            const blob = await secureGetWithFallback<Settings>(ACCOUNT, CACHE_KEY);
            if (!cancelled) setLocal(readBreakdown(blob));
        };
        void load();
        const onFocus = () => { void load(); };
        const onChanged = (event: Event) => {
            setLocal(readBreakdown((event as CustomEvent<Settings | null>).detail));
        };
        window.addEventListener('focus', onFocus);
        window.addEventListener(EVENT, onChanged);
        return () => {
            cancelled = true;
            window.removeEventListener('focus', onFocus);
            window.removeEventListener(EVENT, onChanged);
        };
    }, [refreshKey]);

    const setBreakdown = useCallback(async (next: boolean) => {
        try {
            const existing = await secureGetWithFallback<Settings>(ACCOUNT, CACHE_KEY);
            const ui = existing?.ui_settings;
            const updated = {
                ...(existing || {}),
                ui_settings: {
                    ...(ui && typeof ui === 'object' ? ui : {}),
                    my_servers_breakdown: next,
                },
            };
            await secureStoreAndClean(ACCOUNT, CACHE_KEY, updated);
            window.dispatchEvent(new CustomEvent(EVENT, { detail: updated }));
        } catch {
            // Keep the last confirmed choice when the vault write fails.
        }
    }, []);

    return { breakdown, setBreakdown };
}
