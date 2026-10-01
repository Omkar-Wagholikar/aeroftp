// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { KeyRound, LogOut, RefreshCw, Save, Trash2, X } from 'lucide-react';
import { createPortal } from 'react-dom';
import { ConfirmOverlay } from '../common/ConfirmOverlay';
import { MODAL_Z } from '../../utils/modalLayers';
import { useTranslation } from '../../i18n';
import type { TranslationFunction } from '../../i18n/types';
import { describeMcpError } from './mcpErrors';

export type HttpAuthMode = 'none' | 'bearer' | 'oauth';
export type HttpAuth =
    | { mode: 'none' }
    | { mode: 'bearer' }
    | { mode: 'oauth'; client_id?: string | null; client_id_metadata_url?: string | null };
export interface HttpServerView {
    id: string;
    endpoint: string;
    auth: HttpAuth;
    enabled: boolean;
    revision: number;
    credential: 'not_required' | 'disabled' | 'missing' | 'saved' | 'authorized' | 'expired' | 'invalid';
    expires_at: number | null;
    refreshable: boolean;
}
interface Attempt { attempt: string; authorization_url: string; browser_opened: boolean }

export const authInput = (mode: HttpAuthMode, clientId = '', metadataUrl = ''): HttpAuth => mode === 'oauth'
    ? { mode, client_id: clientId.trim() || null, client_id_metadata_url: metadataUrl.trim() || null }
    : { mode };

export function AuthFields({ mode, setMode, clientId, setClientId, metadataUrl, setMetadataUrl, disabled }: {
    mode: HttpAuthMode; setMode: (mode: HttpAuthMode) => void;
    clientId: string; setClientId: (value: string) => void;
    metadataUrl: string; setMetadataUrl: (value: string) => void;
    disabled: boolean;
}) {
    const t = useTranslation();
    const field = 'min-w-40 flex-1 rounded bg-gray-900 border border-gray-600 p-2 text-sm';
    return <div className="flex flex-wrap gap-2">
        <select className="rounded bg-gray-900 border border-gray-600 p-2 text-sm" value={mode} disabled={disabled}
            aria-label={t('ai.mcpClient.authentication')} onChange={event => setMode(event.target.value as HttpAuthMode)}>
            <option value="none">{t('ai.mcpClient.authNone')}</option>
            <option value="bearer">{t('ai.mcpClient.authBearer')}</option>
            <option value="oauth">{t('ai.mcpClient.authOAuth')}</option>
        </select>
        {mode === 'oauth' && <>
            <input className={field} value={clientId} disabled={disabled} placeholder={t('ai.mcpClient.clientId')}
                aria-label={t('ai.mcpClient.clientId')} onChange={event => setClientId(event.target.value)} />
            <input className={field} value={metadataUrl} disabled={disabled} placeholder={t('ai.mcpClient.clientMetadataUrl')}
                aria-label={t('ai.mcpClient.clientMetadataUrl')} onChange={event => setMetadataUrl(event.target.value)} />
        </>}
    </div>;
}

function credentialText(t: TranslationFunction, server: HttpServerView): string {
    switch (server.credential) {
        case 'not_required': return t('ai.mcpClient.credentialNotRequired');
        case 'disabled': return t('ai.mcpClient.credentialDisabled');
        case 'saved': return t('ai.mcpClient.credentialSaved');
        case 'missing': return server.auth.mode === 'bearer'
            ? t('ai.mcpClient.credentialBearerMissing') : t('ai.mcpClient.credentialOAuthMissing');
        case 'authorized': return server.expires_at
            ? t('ai.mcpClient.credentialAuthorizedUntil', { time: new Date(server.expires_at * 1000).toLocaleString() })
            : t('ai.mcpClient.credentialAuthorized');
        case 'expired': return t('ai.mcpClient.credentialExpired');
        case 'invalid': return t('ai.mcpClient.credentialInvalid');
    }
}

export function McpHttpServerCard({ server, refresh }: { server: HttpServerView; refresh: () => Promise<void> }) {
    const t = useTranslation();
    const oauth = server.auth.mode === 'oauth' ? server.auth : null;
    const [endpoint, setEndpoint] = useState(server.endpoint);
    const [mode, setMode] = useState<HttpAuthMode>(server.auth.mode);
    const [clientId, setClientId] = useState(oauth?.client_id ?? '');
    const [metadataUrl, setMetadataUrl] = useState(oauth?.client_id_metadata_url ?? '');
    const [token, setToken] = useState('');
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState('');
    const [notice, setNotice] = useState('');
    const [attempt, setAttempt] = useState<Attempt | null>(null);
    const [pending, setPending] = useState<'remove' | 'signOut' | null>(null);
    const live = useRef<string | null>(null);

    useEffect(() => { setEndpoint(server.endpoint); }, [server.endpoint]);
    useEffect(() => { setMode(server.auth.mode); }, [server.auth.mode]);
    useEffect(() => { setClientId(oauth?.client_id ?? ''); }, [oauth?.client_id]);
    useEffect(() => { setMetadataUrl(oauth?.client_id_metadata_url ?? ''); }, [oauth?.client_id_metadata_url]);
    // Leaving the settings abandons the browser authorization: the backend listener ends with it.
    useEffect(() => () => {
        if (live.current) void invoke('mcp_client_http_oauth_cancel', { attempt: live.current }).catch(() => undefined);
    }, []);

    const perform = async (action: () => Promise<void>, done?: string) => {
        setBusy(true); setError(''); setNotice('');
        let failed = false;
        try { await action(); if (done) setNotice(done); }
        catch (cause) { failed = true; setError(describeMcpError(t, cause)); }
        finally {
            try { await refresh(); }
            catch (cause) { if (!failed) setError(describeMcpError(t, cause)); }
            finally { setBusy(false); }
        }
    };

    const save = (changes: { enabled?: boolean } = {}) => invoke<void>('mcp_client_http_upsert_server', { server: {
        id: server.id,
        endpoint: changes.enabled === undefined ? endpoint.trim() : server.endpoint,
        auth: changes.enabled === undefined ? authInput(mode, clientId, metadataUrl) : server.auth,
        enabled: changes.enabled ?? server.enabled,
        expected_revision: server.revision,
    } });

    const saveToken = () => perform(async () => {
        try { await invoke('mcp_client_http_set_bearer', { serverId: server.id, secret: token }); }
        finally { setToken(''); }
    });

    const authorize = () => perform(async () => {
        const started = await invoke<Attempt>('mcp_client_http_oauth_begin', { serverId: server.id });
        live.current = started.attempt;
        setAttempt(started);
        try { await invoke('mcp_client_http_oauth_wait', { attempt: started.attempt }); }
        finally { live.current = null; setAttempt(null); }
    }, t('ai.mcpClient.authorizeDone'));

    const cancel = () => {
        if (attempt) void invoke('mcp_client_http_oauth_cancel', { attempt: attempt.attempt }).catch(() => undefined);
    };

    const button = 'flex items-center gap-1 rounded px-3 py-1.5 text-sm disabled:opacity-50';
    const hasAuthorization = ['authorized', 'expired', 'invalid'].includes(server.credential);
    return <div className="rounded-lg border border-gray-700 bg-gray-800 p-4 space-y-3">
        <div className="flex items-center justify-between gap-3">
            <div className="min-w-0">
                <h3 className="font-medium text-white">{server.id}</h3>
                <p className="text-xs text-gray-400 break-all">{server.endpoint}</p>
                <p className="text-xs text-gray-400">{credentialText(t, server)} · {t('ai.mcpClient.healthPending')}</p>
            </div>
            <div className="flex items-center gap-3">
                <label className="flex items-center gap-2 text-sm text-gray-300">
                    <input type="checkbox" checked={server.enabled} disabled={busy}
                        onChange={event => perform(() => save({ enabled: event.target.checked }))} /> {t('ai.mcpClient.enabled')}
                </label>
                <button type="button" disabled={busy} aria-label={t('ai.mcpClient.removeServer', { name: server.id })}
                    className="text-red-400 disabled:opacity-50" onClick={() => setPending('remove')}><Trash2 size={16} /></button>
            </div>
        </div>
        <label className="block text-xs text-gray-300">{t('ai.mcpClient.endpoint')}
            <input className="mt-1 w-full rounded bg-gray-900 border border-gray-600 p-2 text-sm" value={endpoint}
                disabled={busy} onChange={event => setEndpoint(event.target.value)} />
        </label>
        <AuthFields mode={mode} setMode={setMode} clientId={clientId} setClientId={setClientId}
            metadataUrl={metadataUrl} setMetadataUrl={setMetadataUrl} disabled={busy} />
        <button type="button" disabled={busy} className={`${button} bg-purple-700`}
            onClick={() => perform(() => save())}><Save size={14} /> {t('ai.mcpClient.saveServer')}</button>
        {server.auth.mode === 'bearer' && <div className="flex flex-wrap gap-2 border-t border-gray-700 pt-3">
            <input className="min-w-40 flex-1 rounded bg-gray-900 border border-gray-600 p-2 text-sm" value={token}
                type="password" autoComplete="new-password" placeholder={t('ai.mcpClient.tokenValue')}
                aria-label={t('ai.mcpClient.tokenValue')} disabled={busy} onChange={event => setToken(event.target.value)} />
            <button type="button" disabled={busy || !token} className={`${button} bg-gray-700`}
                onClick={() => void saveToken()}>{t('ai.mcpClient.saveToken')}</button>
        </div>}
        {server.auth.mode === 'oauth' && <div className="flex flex-wrap items-center gap-2 border-t border-gray-700 pt-3">
            {attempt ? <>
                <span className="text-xs text-gray-300" role="status">{t('ai.mcpClient.waitingBrowser')}</span>
                <button type="button" className={`${button} bg-gray-700`} onClick={cancel}><X size={14} /> {t('ai.mcpClient.cancel')}</button>
                {!attempt.browser_opened && <p className="w-full text-xs text-gray-400">{t('ai.mcpClient.browserNotOpened')}{' '}
                    <span className="select-all break-all font-mono text-gray-200">{attempt.authorization_url}</span></p>}
            </> : <>
                <button type="button" disabled={busy || !server.enabled} className={`${button} bg-purple-700`}
                    onClick={() => void authorize()}><KeyRound size={14} /> {t('ai.mcpClient.authorize')}</button>
                {server.refreshable && hasAuthorization && <button type="button" disabled={busy} className={`${button} bg-gray-700`}
                    onClick={() => void perform(() => invoke('mcp_client_http_oauth_refresh', { serverId: server.id }), t('ai.mcpClient.refreshDone'))}>
                    <RefreshCw size={14} /> {t('ai.mcpClient.refreshAuthorization')}</button>}
                {hasAuthorization && <button type="button" disabled={busy} className={`${button} bg-gray-700`}
                    onClick={() => setPending('signOut')}><LogOut size={14} /> {t('ai.mcpClient.signOut')}</button>}
            </>}
        </div>}
        {notice && <p role="status" className="text-xs text-green-400">{notice}</p>}
        {error && <p role="alert" className="text-xs text-red-400">{error}</p>}
        {pending && createPortal(<ConfirmOverlay
            message={pending === 'remove'
                ? t('ai.mcpClient.confirmRemoveHttp', { name: server.id })
                : t('ai.mcpClient.confirmSignOut', { name: server.id })}
            onCancel={() => setPending(null)}
            onConfirm={() => {
                const action = pending;
                setPending(null);
                void perform(() => invoke(action === 'remove' ? 'mcp_client_http_remove_server' : 'mcp_client_http_oauth_sign_out',
                    { serverId: server.id }));
            }}
            zClass={MODAL_Z.globalConfirm}
        />, document.body)}
    </div>;
}
